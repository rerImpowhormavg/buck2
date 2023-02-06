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
use dice::DiceEquality;
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

#[derive(Clone, Dupe, Copy, Debug, Allocative)]
pub enum DiceCleanup {
    Block,
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

impl FromStr for DiceCleanup {
    type Err = InvalidType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "BLOCK" => Ok(DiceCleanup::Block),
            "RUN" => Ok(DiceCleanup::Run),
            _ => Err(InvalidType("DiceCleanup".to_owned(), s.to_owned())),
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
    /// Whether to wait for idle DICE.
    dice_cleanup_config: DiceCleanup,
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
    version: DiceEquality,
}

impl DiceStatus {
    fn idle() -> Self {
        Self::Available { active: None }
    }

    fn active(version: DiceEquality) -> Self {
        Self::Available {
            active: Some(ActiveDice { version }),
        }
    }
}

impl ConcurrencyHandlerData {
    fn has_no_active_traces(&self) -> bool {
        self.active_traces.is_empty()
    }

    /// Attempt a transition to cleanup, or straight to idle if cleanup can be skipped. Returns
    /// whether the transition was done.
    fn transition_to_cleanup(&mut self, dice: &Dice) -> bool {
        if !self.has_no_active_traces() {
            return false;
        }

        tracing::info!("Transitioning ActiveDice to cleanup");

        // When releasing the active DICE, if any work is ongoing, place it in a clean up
        // state. Callers will wait until it goes idle.
        self.cleanup_epoch += 1;
        self.dice_status = DiceStatus::Cleanup {
            future: dice.wait_for_idle().boxed().shared(),
            epoch: self.cleanup_epoch,
        };

        true
    }
}

#[async_trait]
pub trait DiceUpdater: Send + Sync {
    async fn update(
        &self,
        mut ctx: DiceTransactionUpdater,
    ) -> anyhow::Result<DiceTransactionUpdater>;
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
        dice_cleanup_config: DiceCleanup,
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
            dice_cleanup_config,
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

        let span = tracing::span!(tracing::Level::DEBUG, "wait_for_others", trace = %trace);
        let _enter = span.enter();

        let mut data = self.data.lock();

        let transaction = loop {
            match &data.dice_status {
                DiceStatus::Cleanup { future, epoch } => {
                    tracing::debug!("ActiveDice is in cleanup");
                    let future = future.clone();
                    let epoch = *epoch;

                    if matches!(self.dice_cleanup_config, DiceCleanup::Block) {
                        drop(data);
                        event_dispatcher
                            .span_async(
                                buck2_data::DiceCleanupStart { epoch: epoch as _ },
                                async move { (future.await, buck2_data::DiceCleanupEnd {}) },
                            )
                            .await;
                        data = self.data.lock();
                    }

                    // Once the cleanup future resolves, check that we haven't completely skipped
                    // an epoch (in which case we need to cleanup again), and proceed to report
                    // DICE is available again.
                    if data.cleanup_epoch == epoch {
                        data.dice_status = DiceStatus::idle();
                    }
                }
                DiceStatus::Available { active } => {
                    tracing::debug!("ActiveDice is available");
                    // we rerun the updates in case that files on disk have changed between commands.
                    // this might cause some churn, but concurrent commands don't happen much and
                    // isn't a big perf bottleneck. Dice should be able to resurrect nodes properly.
                    let transaction = event_dispatcher
                        .span_async(buck2_data::DiceStateUpdateStart {}, async {
                            (
                                async {
                                    let updater = self.dice.updater();
                                    let user_data =
                                        user_data.provide(&updater.existing_state()).await?;
                                    let transaction =
                                        updates.update(updater).await?.commit_with_data(user_data);
                                    anyhow::Ok(transaction)
                                }
                                .await,
                                buck2_data::DiceStateUpdateEnd {},
                            )
                        })
                        .await?;

                    if let Some(active) = active {
                        let is_same_state = transaction.equivalent(&active.version);

                        // If we have a different state, attempt to transition to cleanup. This will
                        // succeed only if the current state is not in use.
                        if !is_same_state {
                            if data.transition_to_cleanup(&self.dice) {
                                continue;
                            }
                        }

                        tracing::debug!("ActiveDice has an active_transaction");

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
                        tracing::debug!("ActiveDice has no active_transaction");
                        event_dispatcher.instant_event(NoActiveDiceState {});
                        data.dice_status = DiceStatus::active(transaction.equality_token());
                        break transaction;
                    }
                }
            }
        };

        tracing::info!("Acquired access to DICE");

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
        tracing::info!("Command has exited: {}", self.1);

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

        if data.has_no_active_traces() {
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
    use std::time::Duration;

    use allocative::Allocative;
    use async_trait::async_trait;
    use buck2_events::dispatch::EventDispatcher;
    use buck2_events::trace::TraceId;
    use derivative::Derivative;
    use derive_more::Display;
    use dice::DetectCycles;
    use dice::Dice;
    use dice::DiceComputations;
    use dice::DiceTransactionUpdater;
    use dice::InjectedKey;
    use dice::Key;
    use dice::UserComputationData;
    use dupe::Dupe;
    use more_futures::cancellable_future::with_structured_cancellation;
    use parking_lot::Mutex;
    use tokio::sync::Barrier;
    use tokio::sync::RwLock;

    use super::*;

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
            mut ctx: DiceTransactionUpdater,
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

        let concurrency = ConcurrencyHandler::new(
            dice,
            NestedInvocation::Run,
            ParallelInvocation::Run,
            DiceCleanup::Block,
        );

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

        let concurrency = ConcurrencyHandler::new(
            dice,
            NestedInvocation::Error,
            ParallelInvocation::Run,
            DiceCleanup::Block,
        );

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

        let concurrency = ConcurrencyHandler::new(
            dice,
            NestedInvocation::Run,
            ParallelInvocation::Run,
            DiceCleanup::Block,
        );

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
            DiceCleanup::Block,
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

        let concurrency = ConcurrencyHandler::new(
            dice.dupe(),
            NestedInvocation::Run,
            ParallelInvocation::Run,
            DiceCleanup::Block,
        );

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

    #[tokio::test]
    async fn test_cleanup_stage() -> anyhow::Result<()> {
        #[derive(Clone, Dupe, Derivative, Allocative, Display)]
        #[derivative(Hash, Eq, PartialEq, Debug)]
        #[display(fmt = "TestKey")]
        struct TestKey {
            #[derivative(Debug = "ignore", Hash = "ignore", PartialEq = "ignore")]
            is_executing: Arc<Mutex<()>>,
        }

        #[async_trait::async_trait]
        impl Key for TestKey {
            type Value = ();

            #[allow(clippy::await_holding_lock)]
            async fn compute(&self, _ctx: &DiceComputations) -> Self::Value {
                let _guard = self.is_executing.lock();

                // TODO: use critical_section as it's simpler, but this stack doesn't have it and
                // this works equally well here :)
                with_structured_cancellation(|_obs| tokio::time::sleep(Duration::from_secs(1)))
                    .await;
            }

            fn equality(_me: &Self::Value, _other: &Self::Value) -> bool {
                true
            }
        }

        let key = TestKey {
            is_executing: Arc::new(Mutex::new(())),
        };

        let key = &key;

        let dice = Dice::builder().build(DetectCycles::Enabled);

        let concurrency = ConcurrencyHandler::new(
            dice.dupe(),
            NestedInvocation::Error,
            ParallelInvocation::Block,
            DiceCleanup::Block,
        );

        // Kick off our computation and wait until it's running.

        concurrency
            .enter(
                EventDispatcher::null(),
                &TestDiceDataProvider,
                &NoChanges,
                |dice| async move {
                    let compute = dice.compute(key).fuse();

                    let started = async {
                        while !key.is_executing.is_locked() {
                            tokio::task::yield_now().await;
                        }
                    }
                    .fuse();

                    // NOTE: We still need to poll `compute` for it to actually spawn, hence the
                    // select below.

                    futures::pin_mut!(compute);
                    futures::pin_mut!(started);

                    futures::select! {
                        _ = compute => panic!("compute finished before started?"),
                        _ = started => {}
                    }
                },
                false,
                Vec::new(),
            )
            .await?;

        // Now, re-enter. We expect to reuse and therefore to not wait.

        concurrency
            .enter(
                EventDispatcher::null(),
                &TestDiceDataProvider,
                &NoChanges,
                |_dice| async move {
                    // The key should still be evaluating by now.
                    assert!(key.is_executing.is_locked());
                },
                false,
                Vec::new(),
            )
            .await?;

        // Now, enter with a different context. This time, we expect to not reuse.

        concurrency
            .enter(
                EventDispatcher::null(),
                &TestDiceDataProvider,
                &CtxDifferent,
                |_dice| async move {
                    assert!(!key.is_executing.is_locked());
                },
                false,
                Vec::new(),
            )
            .await?;

        Ok(())
    }
}
