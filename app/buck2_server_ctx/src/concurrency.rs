/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Handles command concurrency.
//!
//! `buck2` supports limited concurrency for commands.
//! If there are no buckconfig changes, nor file changes, then commands can be allowed to execute
//! concurrently. Otherwise, `buck2` will block waiting for other commands to finish.

use std::fmt::Debug;
use std::str::FromStr;
use std::sync::Arc;

use allocative::Allocative;
use async_condvar_fair::Condvar;
use async_trait::async_trait;
use buck2_core::soft_error;
use buck2_core::truncate::truncate;
use buck2_data::DiceBlockConcurrentCommandEnd;
use buck2_data::DiceBlockConcurrentCommandStart;
use buck2_data::DiceEqualityCheck;
use buck2_data::DiceSynchronizeSectionEnd;
use buck2_data::DiceSynchronizeSectionStart;
use buck2_data::NoActiveDiceState;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::trace::TraceId;
use dice::Dice;
use dice::DiceComputations;
use dice::DiceTransaction;
use dice::DiceTransactionUpdater;
use dice::UserComputationData;
use dupe::Dupe;
use futures::future::BoxFuture;
use futures::future::Future;
use futures::future::FutureExt;
use futures::future::Shared;
use itertools::Itertools;
use parking_lot::lock_api::MutexGuard;
use parking_lot::FairMutex;
use parking_lot::RawFairMutex;
use starlark_map::small_map::SmallMap;
use thiserror::Error;

#[derive(Error, Debug)]
enum ConcurrencyHandlerError {
    #[error(
        "Recursive invocation of Buck, which is discouraged, but will probably work (using the same state). Trace Ids: {0}. Recursive invocation command: `{1}`"
    )]
    NestedInvocationWithSameStates(String, String),
    #[error(
        "Recursive invocation of Buck, with a different state - computation will continue but may produce incorrect results. Trace Ids: {0}. Recursive invocation command: `{1}`"
    )]
    NestedInvocationWithDifferentStates(String, String),
    #[error(
        "Parallel invocation of Buck, with a different state - computation will continue but may produce incorrect results. Trace Ids: {0}"
    )]
    ParallelInvocationWithDifferentStates(String),
}

#[derive(Clone, Dupe, Copy, Debug, Allocative)]
pub enum ParallelInvocation {
    Block,
    Run,
}

#[derive(Clone, Dupe, Copy, Debug, Allocative)]
pub enum NestedInvocation {
    Error,
    Run,
}

#[derive(Error, Debug)]
#[error("Invalid type of `{0}`: `{1}`")]
pub struct InvalidType(String, String);

impl FromStr for ParallelInvocation {
    type Err = InvalidType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "BLOCK" => Ok(ParallelInvocation::Block),
            "RUN" => Ok(ParallelInvocation::Run),
            _ => Err(InvalidType("ParallelInvocation".to_owned(), s.to_owned())),
        }
    }
}

impl FromStr for NestedInvocation {
    type Err = InvalidType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "ERROR" => Ok(NestedInvocation::Error),
            "RUN" => Ok(NestedInvocation::Run),
            _ => Err(InvalidType("NestedInvocation".to_owned(), s.to_owned())),
        }
    }
}

#[derive(Clone, Dupe, Copy, Debug)]
pub enum RunState {
    NestedSameState,
    NestedDifferentState,
    ParallelSameState,
    ParallelDifferentState,
}

#[derive(Clone, Dupe, Copy, Debug)]
pub enum BypassSemaphore {
    Run(RunState),
    Block,
    Error,
}

/// Manages concurrent commands, blocking when appropriate.
///
/// Currently, we allow concurrency if two `DiceTransactions` are deemed equivalent, such that
/// any computation result that occurs in one is directly reusable by another.
#[derive(Clone, Dupe, Allocative)]
pub struct ConcurrencyHandler {
    data: Arc<FairMutex<ConcurrencyHandlerData>>,
    // use an async condvar because the `wait` to `notify` spans across an async function (namely
    // the entire command execution).
    #[allocative(skip)]
    cond: Arc<Condvar>,
    dice: Arc<Dice>,
    // configuration on how to handle nested invocations with different states
    nested_invocation_config: NestedInvocation,
    // configuration on how to handle parallel invocations with different states
    parallel_invocation_config: ParallelInvocation,
}

#[derive(Allocative)]
struct ConcurrencyHandlerData {
    // the currently active `Dice` being used. Commands can only run concurrently if these are
    // "equivalent".
    dice_status: DiceStatus,
    // A list of the currently running traces. It's theoretically possible that we use the same
    // trace twice if we support user supplied `TraceId` and have nested invocations, so we keep
    // a map of number of occurrences.
    active_traces: SmallMap<TraceId, usize>,
    // The current active trace that is executing.
    active_trace: Option<TraceId>,
    // The current active trace's argv.
    active_trace_argv: Option<Vec<String>>,
    // The epoch of the last ActiveDice we assigned.
    cleanup_epoch: usize,
}

#[derive(Allocative)]
enum DiceStatus {
    Available {
        active: Option<ActiveDice>,
    },
    Cleanup {
        future: Shared<BoxFuture<'static, ()>>,
        epoch: usize,
    },
}

#[derive(Allocative)]
struct ActiveDice {
    transaction: DiceTransaction,
}

impl DiceStatus {
    fn idle() -> Self {
        Self::Available { active: None }
    }

    fn active(transaction: DiceTransaction) -> Self {
        Self::Available {
            active: Some(ActiveDice { transaction }),
        }
    }

    fn as_active_mut(&mut self) -> Option<&mut ActiveDice> {
        match self {
            Self::Available {
                active: Some(ref mut active),
            } => Some(active),
            _ => None,
        }
    }
}

#[async_trait]
pub trait DiceUpdater: Send + Sync {
    async fn update(&self, ctx: DiceTransactionUpdater) -> anyhow::Result<DiceTransactionUpdater>;
}

#[async_trait]
pub trait DiceDataProvider: Send + Sync + 'static {
    async fn provide(&self, ctx: &DiceComputations) -> anyhow::Result<UserComputationData>;
}

impl ConcurrencyHandler {
    pub fn new(
        dice: Arc<Dice>,
        nested_invocation_config: NestedInvocation,
        parallel_invocation_config: ParallelInvocation,
    ) -> Self {
        ConcurrencyHandler {
            data: Arc::new(FairMutex::new(ConcurrencyHandlerData {
                dice_status: DiceStatus::idle(),
                active_traces: SmallMap::<TraceId, usize>::new(),
                active_trace: None,
                active_trace_argv: None,
                cleanup_epoch: 0,
            })),
            cond: Default::default(),
            dice,
            nested_invocation_config,
            parallel_invocation_config,
        }
    }

    /// Enters a critical section that requires concurrent command synchronization,
    /// and runs the given `exec` function in the critical section.
    pub async fn enter<F, Fut, R>(
        &self,
        event_dispatcher: EventDispatcher,
        data: &dyn DiceDataProvider,
        updates: &dyn DiceUpdater,
        exec: F,
        is_nested_invocation: bool,
        sanitized_argv: Vec<String>,
    ) -> anyhow::Result<R>
    where
        F: FnOnce(DiceTransaction) -> Fut,
        Fut: Future<Output = R> + Send,
    {
        let events = event_dispatcher.dupe();

        let (_guard, transaction) = event_dispatcher
            .span_async(DiceSynchronizeSectionStart {}, async move {
                (
                    self.wait_for_others(
                        data,
                        updates,
                        events,
                        is_nested_invocation,
                        sanitized_argv,
                    )
                    .await,
                    DiceSynchronizeSectionEnd {},
                )
            })
            .await?;

        Ok(exec(transaction).await)
    }

    #[allow(clippy::await_holding_lock)]
    // this is normally super unsafe, but because we are using an async condvar that takes care
    // of unlocking this mutex, this mutex is actually essentially never held across awaits.
    // The async condvar will handle properly allowing under threads to proceed, avoiding
    // starvation.
    async fn wait_for_others(
        &self,
        user_data: &dyn DiceDataProvider,
        updates: &dyn DiceUpdater,
        event_dispatcher: EventDispatcher,
        is_nested_invocation: bool,
        sanitized_argv: Vec<String>,
    ) -> anyhow::Result<(OnExecExit, DiceTransaction)> {
        let trace = event_dispatcher.trace_id().dupe();
        let mut data = self.data.lock();

        let transaction = loop {
            match &data.dice_status {
                DiceStatus::Cleanup { future, epoch } => {
                    let future = future.clone();
                    let epoch = *epoch;

                    drop(data);
                    future.await;
                    data = self.data.lock();

                    // Once the cleanup future resolves, check that we haven't completely skipped
                    // an epoch (in which case we need to cleanup again), and proceed to report
                    // DICE is available again.
                    if data.cleanup_epoch == epoch {
                        data.dice_status = DiceStatus::idle();
                    }
                }
                DiceStatus::Available { active } => {
                    // we rerun the updates in case that files on disk have changed between commands.
                    // this might cause some churn, but concurrent commands don't happen much and
                    // isn't a big perf bottleneck. Dice should be able to resurrect nodes properly.
                    let transaction = event_dispatcher
                        .span_async(buck2_data::DiceStateUpdateStart {}, async {
                            (
                                async {
                                    let transaction_update = self.dice.updater();
                                    let new_user_data = user_data
                                        .provide(transaction_update.existing_state())
                                        .await?;
                                    let transaction =
                                        transaction_update.commit_with_data(new_user_data);
                                    let transaction =
                                        updates.update(transaction.into_updater()).await?.commit();

                                    anyhow::Ok(transaction)
                                }
                                .await,
                                buck2_data::DiceStateUpdateEnd {},
                            )
                        })
                        .await?;

                    if let Some(active) = active {
                        let is_same_state = active.transaction.equivalent(&transaction);

                        event_dispatcher.instant_event(DiceEqualityCheck {
                            is_equal: is_same_state,
                        });

                        let bypass_semaphore =
                            self.determine_bypass_semaphore(is_same_state, is_nested_invocation);

                        match bypass_semaphore {
                            BypassSemaphore::Error => {
                                return Err(anyhow::Error::new(
                                    ConcurrencyHandlerError::NestedInvocationWithDifferentStates(
                                        format_traces(&data.active_traces, trace.dupe()),
                                        format_argv(&sanitized_argv),
                                    ),
                                ));
                            }
                            BypassSemaphore::Run(state) => {
                                self.emit_logs(
                                    state,
                                    &data.active_traces,
                                    trace.dupe(),
                                    format_argv(&sanitized_argv),
                                )?;

                                break transaction;
                            }
                            BypassSemaphore::Block => {
                                let active_trace = data.active_trace.as_ref().unwrap().to_string();

                                tracing::warn!(
                                    "Running parallel invocation with different states with blocking: \nWaiting trace ID: {} \nCommand: `{}`\nExecuting trace ID: {} \nCommand: `{}`",
                                    &trace,
                                    format_argv(&sanitized_argv),
                                    active_trace,
                                    format_argv(data.active_trace_argv.as_ref().unwrap()),
                                );

                                data = event_dispatcher
                                    .span_async(
                                        DiceBlockConcurrentCommandStart {
                                            current_active_trace_id: active_trace.clone(),
                                            cmd_args: format_argv(
                                                data.active_trace_argv.as_ref().unwrap(),
                                            ),
                                        },
                                        async {
                                            (
                                                self.cond.wait(data).await,
                                                DiceBlockConcurrentCommandEnd {
                                                    ending_active_trace_id: active_trace,
                                                },
                                            )
                                        },
                                    )
                                    .await;
                            }
                        }
                    } else {
                        event_dispatcher.instant_event(NoActiveDiceState {});
                        data.dice_status = DiceStatus::active(transaction.dupe());
                        break transaction;
                    }
                }
            }
        };

        data.active_trace = Some(trace.dupe());
        data.active_trace_argv = Some(sanitized_argv);

        // create the on exit drop handler, which will take care of notifying tasks.
        let drop_guard = OnExecExit::new(self.dupe(), trace.dupe(), data);

        Ok((drop_guard, transaction))
    }

    /// Access dice without locking for dumps.
    pub fn unsafe_dice(&self) -> &Arc<Dice> {
        &self.dice
    }

    fn determine_bypass_semaphore(
        &self,
        is_same_state: bool,
        is_nested_invocation: bool,
    ) -> BypassSemaphore {
        if is_same_state {
            if is_nested_invocation {
                BypassSemaphore::Run(RunState::NestedSameState)
            } else {
                BypassSemaphore::Run(RunState::ParallelSameState)
            }
        } else if is_nested_invocation {
            match self.nested_invocation_config {
                NestedInvocation::Error => BypassSemaphore::Error,
                NestedInvocation::Run => BypassSemaphore::Run(RunState::NestedDifferentState),
            }
        } else {
            match self.parallel_invocation_config {
                ParallelInvocation::Run => BypassSemaphore::Run(RunState::ParallelDifferentState),
                ParallelInvocation::Block => BypassSemaphore::Block,
            }
        }
    }

    fn emit_logs(
        &self,
        state: RunState,
        active_traces: &SmallMap<TraceId, usize>,
        current_trace: TraceId,
        current_trace_args: String,
    ) -> anyhow::Result<()> {
        let active_traces = format_traces(active_traces, current_trace);

        match state {
            RunState::NestedSameState => {
                soft_error!(
                    "nested_invocation_same_dice_state",
                    anyhow::anyhow!(ConcurrencyHandlerError::NestedInvocationWithSameStates(
                        active_traces,
                        current_trace_args,
                    ))
                )?;
            }
            RunState::NestedDifferentState => {
                soft_error!(
                    "nested_invocation_different_dice_state",
                    anyhow::anyhow!(
                        ConcurrencyHandlerError::NestedInvocationWithDifferentStates(
                            active_traces,
                            current_trace_args
                        ),
                    )
                )?;
            }
            RunState::ParallelDifferentState => {
                soft_error!(
                    "parallel_invocation_different_dice_state",
                    anyhow::anyhow!(
                        ConcurrencyHandlerError::ParallelInvocationWithDifferentStates(
                            active_traces,
                        ),
                    )
                )?;
            }
            _ => {}
        }

        Ok(())
    }
}

fn format_traces(active_traces: &SmallMap<TraceId, usize>, current_trace: TraceId) -> String {
    let mut traces = active_traces
        .keys()
        .map(|trace| trace.to_string())
        .join(", ");

    if !active_traces.contains_key(&current_trace) {
        traces.push_str(&format!(", {}", &current_trace));
    }

    traces
}

fn format_argv(arg: &[String]) -> String {
    let mut iter = arg.iter();
    // Skip the "/path/to/buck2" part so we can just emit "buck2" for the start of the cmd
    iter.next();

    let cmd = format!("buck2 {}", iter.join(" "));
    truncate(&cmd, 500)
}

/// Held to execute a command so that when the command is canceled, we properly remove its state
/// from the handler so that it's no longer registered as a ongoing command.
struct OnExecExit(ConcurrencyHandler, TraceId);

impl OnExecExit {
    pub fn new(
        handler: ConcurrencyHandler,
        trace: TraceId,
        mut guard: MutexGuard<'_, RawFairMutex, ConcurrencyHandlerData>,
    ) -> Self {
        *guard.active_traces.entry(trace.dupe()).or_default() += 1;
        Self(handler, trace)
    }
}

impl Drop for OnExecExit {
    fn drop(&mut self) {
        let mut data = self.0.data.lock();
        let refs = {
            let refs = data
                .active_traces
                .get_mut(&self.1)
                .expect("command was active but not in active traces");
            *refs -= 1;

            *refs
        };
        if refs == 0 {
            data.active_traces.remove(&self.1);
        }

        if data.active_traces.is_empty() {
            data.dice_status
                .as_active_mut()
                .expect("should have an active dice");

            // When releasing the active DICE, place it in a clean up state. Callers will wait
            // until it goes idle.
            data.cleanup_epoch += 1;

            data.dice_status = DiceStatus::Cleanup {
                future: self.0.dice.wait_for_idle().boxed().shared(),
                epoch: data.cleanup_epoch,
            };

            // we notify all commands since we don't know how many can actually wake up and run
            // concurrently as several of the currently waiting commands could be "equivalent".
            // This could cause commands to wake up out of order and race, such that the longest
            // waiting command might not still be forced to wait. In reality, it is probably not
            // a terrible issue, as we are unlikely to have many concurrent commands, and people
            // are unlikely to usually care about the precise order they get to run.
            self.0.cond.notify_all()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use allocative::Allocative;
    use async_trait::async_trait;
    use buck2_events::dispatch::EventDispatcher;
    use buck2_events::trace::TraceId;
    use derive_more::Display;
    use dice::cycles::DetectCycles;
    use dice::Dice;
    use dice::DiceComputations;
    use dice::DiceTransactionUpdater;
    use dice::InjectedKey;
    use dice::UserComputationData;
    use dupe::Dupe;
    use tokio::sync::Barrier;
    use tokio::sync::RwLock;

    use crate::concurrency::ConcurrencyHandler;
    use crate::concurrency::DiceDataProvider;
    use crate::concurrency::DiceUpdater;
    use crate::concurrency::NestedInvocation;
    use crate::concurrency::ParallelInvocation;

    struct NoChanges;

    #[async_trait]
    impl DiceUpdater for NoChanges {
        async fn update(
            &self,
            ctx: DiceTransactionUpdater,
        ) -> anyhow::Result<DiceTransactionUpdater> {
            Ok(ctx)
        }
    }

    struct CtxDifferent;

    #[async_trait]
    impl DiceUpdater for CtxDifferent {
        async fn update(
            &self,
            ctx: DiceTransactionUpdater,
        ) -> anyhow::Result<DiceTransactionUpdater> {
            ctx.changed(vec![K])?;
            Ok(ctx)
        }
    }

    #[derive(Clone, Dupe, Display, Debug, Hash, Eq, PartialEq, Allocative)]
    struct K;

    #[async_trait]
    impl InjectedKey for K {
        type Value = usize;

        fn compare(_x: &Self::Value, _y: &Self::Value) -> bool {
            false
        }
    }

    struct TestDiceDataProvider;

    #[async_trait]
    impl DiceDataProvider for TestDiceDataProvider {
        async fn provide(&self, _ctx: &DiceComputations) -> anyhow::Result<UserComputationData> {
            Ok(Default::default())
        }
    }

    #[tokio::test]
    async fn nested_invocation_same_transaction() {
        let dice = Dice::builder().build(DetectCycles::Enabled);

        let concurrency =
            ConcurrencyHandler::new(dice, NestedInvocation::Run, ParallelInvocation::Run);

        let traces1 = TraceId::new();
        let traces2 = TraceId::new();
        let traces3 = TraceId::new();

        let barrier = Arc::new(Barrier::new(3));

        let fut1 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces1),
            &TestDiceDataProvider,
            &NoChanges,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            true,
            Vec::new(),
        );
        let fut2 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces2),
            &TestDiceDataProvider,
            &NoChanges,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            true,
            Vec::new(),
        );
        let fut3 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces3),
            &TestDiceDataProvider,
            &NoChanges,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            true,
            Vec::new(),
        );

        let (r1, r2, r3) = futures::future::join3(fut1, fut2, fut3).await;
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();
    }

    #[tokio::test]
    async fn nested_invocation_should_error() {
        let dice = Dice::builder().build(DetectCycles::Enabled);

        let concurrency =
            ConcurrencyHandler::new(dice, NestedInvocation::Error, ParallelInvocation::Run);

        let traces1 = TraceId::new();
        let traces2 = TraceId::new();

        let barrier = Arc::new(Barrier::new(2));

        let fut1 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces1),
            &TestDiceDataProvider,
            &NoChanges,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            true,
            Vec::new(),
        );

        let fut2 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces2),
            &TestDiceDataProvider,
            &CtxDifferent,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            true,
            Vec::new(),
        );

        match futures::future::try_join(fut1, fut2).await {
            Err(e) => assert!(e.to_string().contains("Recursive invocation")),
            Ok(_) => {
                panic!("Futures should not have completed successfully")
            }
        }
    }

    #[tokio::test]
    async fn parallel_invocation_same_transaction() {
        let dice = Dice::builder().build(DetectCycles::Enabled);

        let concurrency =
            ConcurrencyHandler::new(dice, NestedInvocation::Run, ParallelInvocation::Run);

        let traces1 = TraceId::new();
        let traces2 = TraceId::new();
        let traces3 = TraceId::new();

        let barrier = Arc::new(Barrier::new(3));

        let fut1 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces1),
            &TestDiceDataProvider,
            &NoChanges,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            false,
            Vec::new(),
        );
        let fut2 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces2),
            &TestDiceDataProvider,
            &NoChanges,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            false,
            Vec::new(),
        );
        let fut3 = concurrency.enter(
            EventDispatcher::null_sink_with_trace(traces3),
            &TestDiceDataProvider,
            &NoChanges,
            |_| {
                let b = barrier.dupe();
                async move {
                    b.wait().await;
                }
            },
            false,
            Vec::new(),
        );

        let (r1, r2, r3) = futures::future::join3(fut1, fut2, fut3).await;
        r1.unwrap();
        r2.unwrap();
        r3.unwrap();
    }

    #[tokio::test]
    async fn parallel_invocation_different_traceid_blocks() -> anyhow::Result<()> {
        let dice = Dice::builder().build(DetectCycles::Enabled);

        let concurrency = ConcurrencyHandler::new(
            dice.dupe(),
            NestedInvocation::Run,
            ParallelInvocation::Block,
        );

        let traces1 = TraceId::new();
        let traces2 = traces1.dupe();
        let traces_different = TraceId::new();

        let block1 = Arc::new(RwLock::new(()));
        let blocked1 = block1.write().await;

        let block2 = Arc::new(RwLock::new(()));
        let blocked2 = block2.write().await;

        let barrier1 = Arc::new(Barrier::new(3));
        let barrier2 = Arc::new(Barrier::new(2));

        let arrived = Arc::new(AtomicBool::new(false));

        let fut1 = tokio::spawn({
            let concurrency = concurrency.dupe();
            let barrier = barrier1.dupe();
            let b = block1.dupe();

            async move {
                concurrency
                    .enter(
                        EventDispatcher::null_sink_with_trace(traces1),
                        &TestDiceDataProvider,
                        &NoChanges,
                        |_| async move {
                            barrier.wait().await;
                            let _g = b.read().await;
                        },
                        false,
                        Vec::new(),
                    )
                    .await
            }
        });

        let fut2 = tokio::spawn({
            let concurrency = concurrency.dupe();
            let barrier = barrier1.dupe();
            let b = block2.dupe();

            async move {
                concurrency
                    .enter(
                        EventDispatcher::null_sink_with_trace(traces2),
                        &TestDiceDataProvider,
                        &NoChanges,
                        |_| async move {
                            barrier.wait().await;
                            let _g = b.read().await;
                        },
                        false,
                        Vec::new(),
                    )
                    .await
            }
        });

        barrier1.wait().await;

        let fut3 = tokio::spawn({
            let concurrency = concurrency.dupe();
            let barrier = barrier2.dupe();
            let arrived = arrived.dupe();

            async move {
                barrier.wait().await;
                concurrency
                    .enter(
                        EventDispatcher::null_sink_with_trace(traces_different),
                        &TestDiceDataProvider,
                        &CtxDifferent,
                        |_| async move {
                            arrived.store(true, Ordering::Relaxed);
                        },
                        false,
                        Vec::new(),
                    )
                    .await
            }
        });

        barrier2.wait().await;

        assert!(!arrived.load(Ordering::Relaxed));

        drop(blocked1);
        fut1.await??;

        assert!(!arrived.load(Ordering::Relaxed));

        drop(blocked2);
        fut2.await??;

        fut3.await??;

        assert!(arrived.load(Ordering::Relaxed));

        Ok(())
    }

    #[tokio::test]
    async fn parallel_invocation_different_traceid_bypass_semaphore() -> anyhow::Result<()> {
        let dice = Dice::builder().build(DetectCycles::Enabled);

        let concurrency =
            ConcurrencyHandler::new(dice.dupe(), NestedInvocation::Run, ParallelInvocation::Run);

        let traces1 = TraceId::new();
        let traces2 = traces1.dupe();
        let traces_different = TraceId::new();

        let barrier = Arc::new(Barrier::new(3));

        let fut1 = tokio::spawn({
            let concurrency = concurrency.dupe();
            let barrier = barrier.dupe();

            async move {
                concurrency
                    .enter(
                        EventDispatcher::null_sink_with_trace(traces1),
                        &TestDiceDataProvider,
                        &NoChanges,
                        |_| async move {
                            barrier.wait().await;
                        },
                        false,
                        Vec::new(),
                    )
                    .await
            }
        });

        let fut2 = tokio::spawn({
            let concurrency = concurrency.dupe();
            let barrier = barrier.dupe();

            async move {
                concurrency
                    .enter(
                        EventDispatcher::null_sink_with_trace(traces2),
                        &TestDiceDataProvider,
                        &NoChanges,
                        |_| async move {
                            barrier.wait().await;
                        },
                        false,
                        Vec::new(),
                    )
                    .await
            }
        });

        let fut3 = tokio::spawn({
            let concurrency = concurrency.dupe();
            let barrier = barrier.dupe();

            async move {
                concurrency
                    .enter(
                        EventDispatcher::null_sink_with_trace(traces_different),
                        &TestDiceDataProvider,
                        &CtxDifferent,
                        |_| async move {
                            barrier.wait().await;
                        },
                        false,
                        Vec::new(),
                    )
                    .await
            }
        });

        let (r1, r2, r3) = futures::future::join3(fut1, fut2, fut3).await;
        r1??;
        r2??;
        r3??;

        Ok(())
    }
}
