/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Write;
use std::time::Duration;

use buck2_event_observer::action_stats::ActionStats;
use buck2_event_observer::display;
use buck2_event_observer::display::TargetDisplayOptions;
use buck2_event_observer::span_tracker::BuckEventSpanHandle;
use buck2_event_observer::span_tracker::BuckEventSpanTracker;
use superconsole::components::bordering::BorderedSpec;
use superconsole::components::splitting::SplitKind;
use superconsole::components::Bordered;
use superconsole::components::Bounded;
use superconsole::components::Expanding;
use superconsole::components::Split;
use superconsole::style::Stylize;
use superconsole::Component;
use superconsole::Dimensions;
use superconsole::Direction;
use superconsole::DrawMode;
use superconsole::Line;
use superconsole::Lines;
use superconsole::Span;
use superconsole::State;

use self::table_builder::Table;
use crate::subscribers::subscriber::Tick;
use crate::subscribers::superconsole::common::HeaderLineComponent;
use crate::subscribers::superconsole::common::StaticStringComponent;
use crate::subscribers::superconsole::timed_list::table_builder::Row;
use crate::subscribers::superconsole::SuperConsoleConfig;
use crate::subscribers::superconsole::TimeSpeed;

mod table_builder;

/// The minimum time disparity between a single subaction and a target elapsed time
/// before the former will be displayed separately from the latter's time.
/// Heuristic uses a percent difference to normalize for long running actions.
const DISPLAY_SUBACTION_CUTOFF: f64 = 0.9;

/// Information about notable event durations.
#[derive(Debug)]
pub struct Cutoffs {
    /// Cutoff for normal execution time.
    pub inform: Duration,
    /// Cutoff for abnormal but still OK execution time.
    pub warn: Duration,
    /// Minimum time an event must be alive before it is worth displaying.
    pub _notable: Duration,
}

/// This component renders each event and a timer indicating for how long the event has been ongoing.
#[derive(Debug)]
pub(crate) struct TimedListBody(Bounded);

impl TimedListBody {
    pub(crate) fn new(max_size: usize, cutoffs: Cutoffs) -> Self {
        Self(Bounded::new(
            box Expanding::new(box TimedListBodyInner { max_size, cutoffs }),
            None,
            Some(max_size),
        ))
    }
}

#[derive(Debug)]
struct TimedListBodyInner {
    max_size: usize,
    cutoffs: Cutoffs,
}

impl Component for TimedListBody {
    fn draw_unchecked(
        &self,
        state: &State,
        dimensions: Dimensions,
        mode: DrawMode,
    ) -> anyhow::Result<Vec<superconsole::Line>> {
        self.0.draw(state, dimensions, mode)
    }
}

impl TimedListBodyInner {
    /// Render a root  as `root [first child + remaining children]`
    fn draw_root_first_child(
        &self,
        state: &State,
        root: &BuckEventSpanHandle,
        single_child: BuckEventSpanHandle,
        remaining_children: usize,
    ) -> anyhow::Result<Row> {
        let time_speed = state.get::<TimeSpeed>()?;
        let info = root.info();
        let child_info = single_child.info();

        let event_string = {
            // always display the event and subaction
            let mut builder = format!(
                "{} [{}",
                display::display_event(&info.event, TargetDisplayOptions::for_console())?,
                display::display_event(&child_info.event, TargetDisplayOptions::for_console())?
            );

            let subaction_ratio =
                child_info.start.elapsed().as_secs_f64() / info.start.elapsed().as_secs_f64();

            // but only display the time of the subaction if it differs significantly.
            if subaction_ratio < DISPLAY_SUBACTION_CUTOFF {
                let subaction_time = display::duration_as_secs_elapsed(
                    child_info.start.elapsed(),
                    time_speed.speed(),
                );
                builder.push(' ');
                builder.push_str(&subaction_time);
            }

            if remaining_children > 0 {
                write!(builder, " + {}", remaining_children)
                    .expect("Write to String is not fallible");
            }

            builder.push(']');
            builder
        };

        Row::text(
            0,
            event_string,
            display::duration_as_secs_elapsed(info.start.elapsed(), time_speed.speed()),
            info.start.elapsed(),
            &self.cutoffs,
        )
    }

    fn draw_root(&self, root: &BuckEventSpanHandle, state: &State) -> anyhow::Result<Vec<Row>> {
        let time_speed = state.get::<TimeSpeed>()?;
        let two_lines = state.get::<SuperConsoleConfig>()?.two_lines;
        let info = root.info();

        let mut it = root.children();

        match it.next() {
            Some(first) if !two_lines => Ok(vec![self.draw_root_first_child(
                state,
                root,
                first,
                it.len(),
            )?]),
            first => {
                let mut rows = Vec::new();
                rows.push(Row::span(0, info, time_speed.speed(), &self.cutoffs)?);

                for child in first.into_iter().chain(it) {
                    rows.push(Row::span(
                        2,
                        child.info(),
                        time_speed.speed(),
                        &self.cutoffs,
                    )?);
                }
                Ok(rows)
            }
        }
    }
}

impl Component for TimedListBodyInner {
    fn draw_unchecked(
        &self,
        state: &State,
        dimensions: Dimensions,
        mode: DrawMode,
    ) -> anyhow::Result<Lines> {
        let spans = state.get::<BuckEventSpanTracker>()?;

        let time_speed = state.get::<TimeSpeed>()?;

        let mut roots = spans.iter_roots();

        let mut builder = Table::new();

        let mut first_not_rendered = None;

        for root in &mut roots {
            let rows = self.draw_root(&root, state)?;

            if builder.len() + rows.len() >= self.max_size {
                first_not_rendered = Some(root);
                break;
            }

            builder.rows.extend(rows);
        }

        // Add remaining unshown tasks, if any.
        if let Some(first) = first_not_rendered {
            let remaining_msg = format!("...and {} more not shown above.", roots.len() + 1);
            let longest_count = first.info().start.elapsed();
            let formatted_count =
                display::duration_as_secs_elapsed(longest_count, time_speed.speed());

            builder.rows.push(Row::styled(
                0,
                remaining_msg.italic(),
                formatted_count.italic(),
                longest_count,
                &self.cutoffs,
            )?);
        }

        builder.draw(&superconsole::state![], dimensions, mode)
    }
}

/// This component is used to display summary counts about the number of jobs.
#[derive(Debug)]
struct CountComponent;

impl Component for CountComponent {
    fn draw_unchecked(
        &self,
        state: &State,
        _dimensions: Dimensions,
        mode: DrawMode,
    ) -> anyhow::Result<Lines> {
        let spans = state.get::<BuckEventSpanTracker>()?;
        let action_stats = state.get::<ActionStats>()?;
        let time_speed = state.get::<TimeSpeed>()?;

        let finished = spans.roots_completed();
        let progress = spans.iter_roots().len();

        let elapsed = display::duration_as_secs_elapsed(
            state.get::<Tick>()?.elapsed_time,
            time_speed.speed(),
        );

        let contents = match mode {
            DrawMode::Normal => {
                if action_stats.log_stats() {
                    format!(
                        "Jobs: In progress: {}. Finished: {}. Cache hits: {}%. Time elapsed: {}",
                        progress,
                        finished,
                        action_stats.action_cache_hit_percentage(),
                        elapsed
                    )
                } else {
                    format!(
                        "Jobs: In progress: {}. Finished: {}. Time elapsed: {}",
                        progress, finished, elapsed
                    )
                }
            }
            DrawMode::Final => {
                if action_stats.log_stats() {
                    format!(
                        "Jobs completed: {}. Time elapsed: {}. {}",
                        finished, elapsed, action_stats,
                    )
                } else {
                    format!("Jobs completed: {}. Time elapsed: {}.", finished, elapsed,)
                }
            }
        };
        Ok(vec![Line::unstyled(&contents)?])
    }
}

/// Wrapper component for Header + Count
#[derive(Debug)]
pub(crate) struct TimedListHeader(Bordered);

impl TimedListHeader {
    pub(crate) fn new(header: String) -> Self {
        let info = box StaticStringComponent { header };
        let count = box CountComponent;
        let header_split = box HeaderLineComponent::new(info, count);
        let header_box = Bordered::new(
            header_split,
            BorderedSpec {
                bottom: Some(Span::sanitized("-")),
                top: None,
                left: None,
                right: None,
            },
        );

        Self(header_box)
    }
}

impl Component for TimedListHeader {
    fn draw_unchecked(
        &self,
        state: &State,
        dimensions: Dimensions,
        mode: DrawMode,
    ) -> anyhow::Result<Lines> {
        self.0.draw(state, dimensions, mode)
    }
}

/// Component that displays ongoing events and their durations + summary stats.
#[derive(Debug)]
pub struct TimedList {
    child: Split,
}

impl TimedList {
    /// * `max_events` is the maximum number of events displayed
    /// * `cutoffs` determines durations for warnings, time-outs, and baseline notability.
    /// * `header` is the string displayed at the top of the list.
    pub fn new(max_events: usize, cutoffs: Cutoffs, header: String) -> Self {
        let head = box TimedListHeader::new(header);
        // Subtract for the header and the padding row above and beneath
        let body = box TimedListBody::new(max_events, cutoffs);

        Self {
            child: Split::new(vec![head, body], Direction::Vertical, SplitKind::Adaptive),
        }
    }
}

impl Component for TimedList {
    fn draw_unchecked(
        &self,
        state: &State,
        dimensions: Dimensions,
        mode: DrawMode,
    ) -> anyhow::Result<Lines> {
        let span_tracker: &BuckEventSpanTracker = state.get()?;

        match mode {
            DrawMode::Normal if !span_tracker.is_unused() => {
                self.child.draw(state, dimensions, mode)
            }
            // show a summary at the end
            DrawMode::Final => CountComponent.draw(state, dimensions, DrawMode::Final),
            _ => Ok(vec![]),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use buck2_data::FakeStart;
    use buck2_data::SpanStartEvent;
    use buck2_events::span::SpanId;
    use buck2_events::trace::TraceId;
    use buck2_events::BuckEvent;
    use superconsole::style::style;

    use super::*;
    use crate::subscribers::subscriber::Tick;

    const CUTOFFS: Cutoffs = Cutoffs {
        inform: Duration::from_secs(2),
        warn: Duration::from_secs(4),
        _notable: Duration::from_millis(200),
    };

    #[test]
    fn test_normal() -> anyhow::Result<()> {
        let tick = Tick::now();
        let now = SystemTime::now();

        let timed_list = TimedList::new(5, CUTOFFS, "test".to_owned());
        let label = Arc::new(BuckEvent::new(
            now,
            TraceId::new(),
            Some(SpanId::new()),
            None,
            buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::Fake(FakeStart {
                    caramba: "test".to_owned(),
                })),
            }),
        ));

        let module = Arc::new(BuckEvent::new(
            now,
            TraceId::new(),
            Some(SpanId::new()),
            None,
            buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::Fake(FakeStart {
                    caramba: "foo".to_owned(),
                })),
            }),
        ));

        let mut state = BuckEventSpanTracker::new();
        state
            .start_at(&label, tick.start_time - Duration::from_secs(3))
            .unwrap();
        state
            .start_at(&module, tick.start_time - Duration::from_secs(1))
            .unwrap();

        let time_speed = TimeSpeed::new(Some(1.0)).unwrap();
        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 1,
        };

        let timed_list_state = SuperConsoleConfig::default();

        let output = timed_list.draw(
            &superconsole::state!(&state, &tick, &time_speed, &action_stats, &timed_list_state),
            Dimensions {
                width: 40,
                height: 10,
            },
            DrawMode::Normal,
        )?;
        let expected = vec![
            vec!["test", "Jobs: In progress: 2. Finished: 0. C"].try_into()?,
            Line::sanitized(&"-".repeat(40)),
            Line::from_iter([
                Span::new_styled("test -- speak of the devil".to_owned().dark_yellow())?,
                Span::padding(10),
                Span::new_styled("3.0s".to_owned().dark_yellow())?,
            ]),
            Line::from_iter([
                Span::new_unstyled("foo -- speak of the devil".to_owned())?,
                Span::padding(11),
                Span::new_unstyled("1.0s".to_owned())?,
            ]),
        ];

        pretty_assertions::assert_eq!(output, expected);

        Ok(())
    }

    #[test]
    fn test_remaining() -> anyhow::Result<()> {
        let tick = Tick::now();
        let now = SystemTime::now();

        let e1 = BuckEvent::new(
            now,
            TraceId::new(),
            Some(SpanId::new()),
            None,
            buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::Fake(FakeStart {
                    caramba: "e1".to_owned(),
                })),
            }),
        );

        let e2 = BuckEvent::new(
            now,
            TraceId::new(),
            Some(SpanId::new()),
            None,
            buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::Fake(FakeStart {
                    caramba: "e2".to_owned(),
                })),
            }),
        );

        let e3 = BuckEvent::new(
            now,
            TraceId::new(),
            Some(SpanId::new()),
            None,
            buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::Fake(FakeStart {
                    caramba: "e3".to_owned(),
                })),
            }),
        );

        let mut state = BuckEventSpanTracker::new();

        for e in vec![e1, e2, e3] {
            state
                .start_at(
                    &Arc::new(e.clone()),
                    tick.start_time - Duration::from_secs(1),
                )
                .unwrap();
        }

        let time_speed = TimeSpeed::new(Some(1.0)).unwrap();
        let timed_list = TimedList::new(2, CUTOFFS, "test".to_owned());
        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 1,
        };

        let timed_list_state = SuperConsoleConfig::default();

        let output = timed_list.draw(
            &superconsole::state!(&state, &tick, &time_speed, &action_stats, &timed_list_state),
            Dimensions {
                width: 40,
                height: 10,
            },
            DrawMode::Normal,
        )?;
        let expected = vec![
            vec!["test", "Jobs: In progress: 3. Finished: 0. C"].try_into()?,
            Line::sanitized(&"-".repeat(40)),
            Line::from_iter([
                Span::new_styled(style("e1 -- speak of the devil".to_owned()))?,
                Span::padding(12),
                Span::new_styled(style("1.0s".to_owned()))?,
            ]),
            Line::from_iter([
                Span::new_styled("...and 2 more not shown above.".to_owned().italic())?,
                Span::padding(6),
                Span::new_styled("1.0s".to_owned().italic())?,
            ]),
        ];

        pretty_assertions::assert_eq!(output, expected);

        Ok(())
    }

    #[test]
    fn test_children() -> anyhow::Result<()> {
        let tick = Tick::now();
        let now = SystemTime::now();

        let parent = SpanId::new();

        let timed_list = TimedList::new(5, CUTOFFS, "test".to_owned());

        let action = Arc::new(BuckEvent::new(
            now,
            TraceId::new(),
            Some(parent),
            None,
            SpanStartEvent {
                data: Some(
                    buck2_data::ActionExecutionStart {
                        key: Some(buck2_data::ActionKey {
                            id: Default::default(),
                            owner: Some(buck2_data::action_key::Owner::TargetLabel(
                                buck2_data::ConfiguredTargetLabel {
                                    label: Some(buck2_data::TargetLabel {
                                        package: "pkg".into(),
                                        name: "target".into(),
                                    }),
                                    configuration: Some(buck2_data::Configuration {
                                        full_name: "conf".into(),
                                    }),
                                    execution_configuration: None,
                                },
                            )),
                            key: "".to_owned(),
                        }),
                        name: Some(buck2_data::ActionName {
                            category: "category".into(),
                            identifier: "identifier".into(),
                        }),
                        kind: buck2_data::ActionKind::NotSet as i32,
                    }
                    .into(),
                ),
            }
            .into(),
        ));

        let prepare = Arc::new(BuckEvent::new(
            now,
            TraceId::new(),
            Some(SpanId::new()),
            Some(parent),
            SpanStartEvent {
                data: Some(
                    buck2_data::ExecutorStageStart {
                        stage: Some(buck2_data::PrepareAction {}.into()),
                    }
                    .into(),
                ),
            }
            .into(),
        ));

        let mut state = BuckEventSpanTracker::new();
        state
            .start_at(&action, tick.start_time - Duration::from_secs(10))
            .unwrap();
        state
            .start_at(&prepare, tick.start_time - Duration::from_secs(5))
            .unwrap();

        let time_speed = TimeSpeed::new(Some(1.0)).unwrap();

        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 1,
        };

        let timed_list_state = SuperConsoleConfig::default();

        let output = timed_list.draw(
            &superconsole::state!(&state, &tick, &time_speed, &action_stats, &timed_list_state),
            Dimensions {
                width: 80,
                height: 10,
            },
            DrawMode::Normal,
        )?;
        let expected = vec![
            vec![
                "test",
                "     ",
                "Jobs: In progress: 1. Finished: 0. Cache hits: 100%. Time elapsed: 0.0s",
            ]
            .try_into()?,
            Line::sanitized(&"-".repeat(80)),
            Line::from_iter([
                Span::new_styled(
                    "pkg:target -- action (category identifier) [prepare 5.0s]"
                        .to_owned()
                        .dark_red(),
                )?,
                Span::padding(18),
                Span::new_styled("10.0s".to_owned().dark_red())?,
            ]),
        ];

        pretty_assertions::assert_eq!(output, expected);

        // Now, add another action. Normally we don't have multiple stages actually running
        // concurrently but this is a test!

        let re_download = Arc::new(BuckEvent::new(
            now,
            TraceId::new(),
            Some(SpanId::new()),
            Some(parent),
            SpanStartEvent {
                data: Some(
                    buck2_data::ExecutorStageStart {
                        stage: Some(
                            buck2_data::ReStage {
                                stage: Some(buck2_data::ReDownload {}.into()),
                            }
                            .into(),
                        ),
                    }
                    .into(),
                ),
            }
            .into(),
        ));

        state
            .start_at(&re_download, tick.start_time - Duration::from_secs(2))
            .unwrap();

        let output = timed_list.draw(
            &superconsole::state!(&state, &tick, &time_speed, &action_stats, &timed_list_state),
            Dimensions {
                width: 80,
                height: 10,
            },
            DrawMode::Normal,
        )?;
        let expected = vec![
            vec![
                "test",
                "     ",
                "Jobs: In progress: 1. Finished: 0. Cache hits: 100%. Time elapsed: 0.0s",
            ]
            .try_into()?,
            Line::sanitized(&"-".repeat(80)),
            Line::from_iter([
                Span::new_styled(
                    "pkg:target -- action (category identifier) [prepare 5.0s + 1]"
                        .to_owned()
                        .dark_red(),
                )?,
                Span::padding(14),
                Span::new_styled("10.0s".to_owned().dark_red())?,
            ]),
        ];

        pretty_assertions::assert_eq!(output, expected);

        Ok(())
    }
}
