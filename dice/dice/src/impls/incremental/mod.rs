/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//!
//! The incrementality module of BUCK
//!
//! This is responsible for performing incremental caching and invalidations
//! with multiple versions in-flight at the same time.
//!

use std::any::Any;
use std::borrow::Cow;
use std::fmt::Debug;

use allocative::Allocative;
use dupe::Dupe;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use more_futures::cancellable_future::try_to_disable_cancellation;
use more_futures::spawner::Spawner;
use tokio::sync::oneshot;
use triomphe::Arc;

use crate::api::error::DiceError;
use crate::api::error::DiceResult;
use crate::impls::core::graph::history::CellHistory;
use crate::impls::core::graph::types::VersionedGraphKey;
use crate::impls::core::graph::types::VersionedGraphResult;
use crate::impls::core::state::CoreStateHandle;
use crate::impls::core::state::StateRequest;
use crate::impls::ctx::SharedLiveTransactionCtx;
use crate::impls::evaluator::AsyncEvaluator;
use crate::impls::evaluator::SyncEvaluator;
use crate::impls::events::DiceEventDispatcher;
use crate::impls::key::DiceKey;
use crate::impls::task::dice::DiceTask;
use crate::impls::task::handle::DiceTaskHandle;
use crate::impls::task::spawn_dice_task;
use crate::impls::value::DiceComputedValue;
use crate::versions::VersionRanges;

/// The incremental engine that manages all the handling of the results of a
/// specific key, performing the recomputation if necessary
///
/// The computation of an identical request (same key and version) is
/// automatically deduplicated, so that identical requests share the same set of
/// work. It is guaranteed that there is at most one computation in flight at a
/// time if they share the same key and version.
#[derive(Allocative)]
pub(crate) struct IncrementalEngine {
    state: CoreStateHandle,
}

impl Debug for IncrementalEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalEngine").finish_non_exhaustive()
    }
}

#[allow(unused)] // TODO(bobyf) temporary
impl IncrementalEngine {
    fn new(state: CoreStateHandle) -> Self {
        Self { state }
    }

    pub(crate) fn spawn_for_key<T>(
        state: CoreStateHandle,
        spawner: std::sync::Arc<dyn Spawner<T>>,
        spawn_ctx: &T,
        k: DiceKey,
        eval: AsyncEvaluator,
        transaction_ctx: SharedLiveTransactionCtx,
        events_dispatcher: DiceEventDispatcher,
    ) -> DiceTask {
        spawn_dice_task(spawner, spawn_ctx, async move |handle| {
            let engine = IncrementalEngine::new(state);

            engine
                .eval_entry_versioned(k, eval, transaction_ctx, events_dispatcher, handle)
                .await;

            Box::new(()) as Box<dyn Any + Send + 'static>
        })
    }

    pub(crate) fn project_for_key(
        state: CoreStateHandle,
        task: &DiceTask,
        k: DiceKey,
        eval: SyncEvaluator,
        transaction_ctx: SharedLiveTransactionCtx,
        event_dispatcher: DiceEventDispatcher,
    ) -> DiceResult<DiceComputedValue> {
        task.get_or_complete(|| {
            event_dispatcher.started(k);

            let v = transaction_ctx.get_version();

            debug!(msg = "running projection");

            let eval_result = eval.evaluate(k);

            debug!(msg = "projection finished. updating caches");

            let res = match eval_result {
                Ok(res) => {
                    // send the update but don't wait for it
                    let (tx, _rx) = tokio::sync::oneshot::channel();
                    state.request(StateRequest::UpdateComputed {
                        key: VersionedGraphKey::new(v, k),
                        value: res.value.dupe(),
                        deps: Arc::new(res.deps.into_iter().collect()),
                        resp: tx,
                    });
                    // TODO(bobyf) consider if we want to block and wait for the cache

                    Ok(res.value)
                }
                Err(e) => Err(e),
            };

            debug!(msg = "update future completed");
            event_dispatcher.finished(k);

            res.map(|v| {
                DiceComputedValue::new(
                    v,
                    Arc::new(CellHistory::verified(transaction_ctx.get_version())),
                )
            })
        })
    }

    async fn eval_entry_versioned(
        &self,
        k: DiceKey,
        eval: AsyncEvaluator,
        transaction_ctx: SharedLiveTransactionCtx,
        events_dispatcher: DiceEventDispatcher,
        task_handle: DiceTaskHandle,
    ) {
        let v = transaction_ctx.get_version();
        let (tx, rx) = oneshot::channel();
        self.state.request(StateRequest::LookupKey {
            key: VersionedGraphKey::new(v, k),
            resp: tx,
        });

        let state_result = rx.await.unwrap();

        match state_result {
            VersionedGraphResult::Match(entry) => {
                debug!( k = ?k ,msg = "found existing entry with matching version in cache. reusing result.",);
                task_handle.finished(Ok(entry))
            }
            VersionedGraphResult::Compute => {
                self.compute(k, eval, &transaction_ctx, events_dispatcher, task_handle)
                    .await;
            }

            VersionedGraphResult::CheckDeps(mismatch) => {
                match self
                    .compute_whether_dependencies_changed(
                        eval.dupe(),
                        &transaction_ctx,
                        &mismatch.verified_versions,
                        mismatch.deps_to_validate,
                    )
                    .await
                {
                    DidDepsChange::Changed | DidDepsChange::NoDeps => {
                        self.compute(k, eval, &transaction_ctx, events_dispatcher, task_handle)
                            .await;
                    }
                    DidDepsChange::NoChange(deps) => {
                        // report reuse
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        self.state.request(StateRequest::UpdateComputed {
                            key: VersionedGraphKey::new(v, k),
                            value: mismatch.entry,
                            deps,
                            resp: tx,
                        });

                        task_handle.finished(Ok(rx.await.unwrap()))
                    }
                }
            }
        }
    }

    #[instrument(
        level = "debug",
        skip(self, transaction_ctx, eval, task_handle, event_dispatcher),
        fields(k = ?k, version = %transaction_ctx.get_version()),
    )]
    async fn compute(
        &self,
        k: DiceKey,
        eval: AsyncEvaluator,
        transaction_ctx: &SharedLiveTransactionCtx,
        event_dispatcher: DiceEventDispatcher,
        task_handle: DiceTaskHandle,
    ) {
        task_handle.computing();

        event_dispatcher.started(k);

        let v = transaction_ctx.get_version();

        // TODO(bobyf) these also make good locations where we want to perform instrumentation
        debug!(msg = "running evaluator");

        let eval_result = eval.evaluate(k).await;

        let _guard = match try_to_disable_cancellation() {
            Some(g) => g,
            None => {
                debug!("evaluation cancelled, skipping cache updates");
                task_handle.finished(Err(DiceError::cancelled()));
                return;
            }
        };

        debug!(msg = "evaluation finished. updating caches");

        match eval_result {
            Ok(res) => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.state.request(StateRequest::UpdateComputed {
                    key: VersionedGraphKey::new(v, k),
                    value: res.value,
                    deps: Arc::new(res.deps.into_iter().collect()),
                    resp: tx,
                });

                task_handle.finished(Ok(rx.await.unwrap()))
            }
            Err(e) => task_handle.finished(Err(e)),
        }

        debug!(msg = "update future completed");
        event_dispatcher.finished(k);
    }

    /// determines if the given 'Dependency' has changed between versions 'last_version' and
    /// 'target_version'
    #[instrument(
        level = "debug",
        skip(self, _transaction_ctx, _eval),
        fields(version = %_transaction_ctx.get_version(), verified_versions = %verified_versions)
    )]
    async fn compute_whether_dependencies_changed(
        &self,
        _eval: AsyncEvaluator,
        _transaction_ctx: &SharedLiveTransactionCtx,
        verified_versions: &VersionRanges,
        deps: Arc<Vec<DiceKey>>,
    ) -> DidDepsChange {
        async fn recompute_dep(_k: DiceKey) -> DiceResult<VersionRanges> {
            unimplemented!("todo")
        }

        if deps.is_empty() {
            return DidDepsChange::NoDeps;
        }

        let mut fs: FuturesUnordered<_> = deps.iter().map(|dep| recompute_dep(*dep)).collect();

        let mut verified_versions = Cow::Borrowed(verified_versions);

        while let Some(dep_res) = fs.next().await {
            match dep_res {
                Ok(dep_version_ranges) => {
                    verified_versions =
                        Cow::Owned(verified_versions.intersect(&dep_version_ranges));
                    if verified_versions.is_empty() {
                        debug!(msg = "deps changed");
                        return DidDepsChange::Changed;
                    }
                }
                Err(_dice_err) => {
                    // we don't cache DiceErrors, so this must be because the dependency changed
                    // If the cycle/DiceError is real, we'll hit and propagate it when we recompute
                    // the parent key.
                    return DidDepsChange::Changed;
                }
            }
        }

        debug!(msg = "deps did not change");

        DidDepsChange::NoChange(deps)
    }
}

#[allow(unused)] // TODO(bobyf) temporary
enum DidDepsChange {
    Changed,
    /// These deps did not change
    NoChange(Arc<Vec<DiceKey>>),
    NoDeps,
}
