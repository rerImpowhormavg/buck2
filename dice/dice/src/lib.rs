/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//!
//! The dynamic incremental caching computation engine that powers buckv2.
//!
//! The computation engine will output values corresponding to given `Key`s,
//! reusing previously computed values when possible. `Key`s computations are
//! allowed to request other `Key`s via a `ComputationContext`.
//!
//! Example:
//! ```
//! mod c {
//!
//!     /// declaring computations:
//!     use async_trait::async_trait;
//!     use derive_more::Display;
//!     use dice::{Key, InjectedKey, DiceComputations, DiceDataBuilder, data::DiceData, DiceTransactionUpdater};
//!     use std::sync::Arc;
//!     use allocative::Allocative;
//!
//!     /// A configuration computation that consists of values that are pre-computed outside of DICE
//!     pub struct InjectConfigs<'compute>(&'compute DiceComputations);
//!
//!     impl<'compute> InjectConfigs<'compute> {
//!         pub async fn get_config(&self) -> usize {
//!             self.0.compute(&ConfigKey).await.unwrap()
//!         }
//!     }
//!
//!     #[derive(Clone, Debug, Display, Eq, Hash, PartialEq, Allocative)]
//!     #[display(fmt = "{:?}", self)]
//!     struct ConfigKey;
//!
//!     #[async_trait]
//!     impl InjectedKey for ConfigKey {
//!         type Value = usize;
//!
//!         fn compare(x: &Self::Value,y: &Self::Value) -> bool {
//!             x == y
//!         }
//!     }
//!
//!     pub struct MyComputation<'compute>(&'compute DiceComputations);
//!
//!     impl<'compute> MyComputation<'compute> {
//!         // declaring a computation function
//!         pub async fn compute_a(&self, a: usize, s: String) -> Arc<String> {
//!             #[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative)]
//!             #[display(fmt = "{:?}", self)]
//!             struct ComputeA(usize, String);
//!
//!             #[async_trait]
//!             impl Key for ComputeA {
//!                 type Value = Arc<String>;
//!
//!                 async fn compute(&self, ctx: &DiceComputations) -> Self::Value {
//!                     // request for other computations on the self
//!                     let n = ctx.my_computation().compute_b(self.0).await;
//!                     Arc::new(self.1.repeat(n))
//!                 }
//!
//!                 fn equality(x: &Self::Value,y: &Self::Value) -> bool {
//!                     x == y
//!                 }
//!             }
//!
//!             self.0.compute(&ComputeA(a, s)).await.unwrap()
//!         }
//!
//!         // second computation function
//!         pub async fn compute_b(&self, a: usize) -> usize {
//!                 self.0.compute(&ComputeB(a)).await.unwrap()
//!         }
//!     }
//!
//!     #[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative)]
//!     #[display(fmt = "{:?}", self)]
//!     struct ComputeB(usize);
//!
//!     #[async_trait]
//!     impl Key for ComputeB {
//!         type Value = usize;
//!
//!         async fn compute(&self, ctx: &DiceComputations) -> Self::Value {
//!             self.0 + ctx.injected_configs().get_config().await + ctx.global_data().static_data().len()
//!         }
//!
//!         fn equality(x: &Self::Value,y: &Self::Value) -> bool {
//!             x == y
//!         }
//!     }
//!
//!     // trait to register the computation to DICE
//!     pub trait HasMyComputation {
//!         fn my_computation(&self) -> MyComputation;
//!     }
//!
//!     // attach the declared computation to DICE via the context
//!     impl HasMyComputation for DiceComputations {
//!         fn my_computation(&self) -> MyComputation {
//!             MyComputation(self)
//!         }
//!     }
//!
//!     // trait to register the precomputed configs to DICE
//!     pub trait HasInjectedConfig {
//!         fn injected_configs(&self) -> InjectConfigs;
//!     }
//!
//!     impl HasInjectedConfig for DiceComputations {
//!         fn injected_configs(&self) -> InjectConfigs {
//!             InjectConfigs(self)
//!         }
//!     }
//!
//!     pub trait SetInjectedConfig {
//!         fn inject_config(&self, i: usize);
//!     }
//!
//!     impl SetInjectedConfig for DiceTransactionUpdater {
//!         /// ways to inject the precomputed values to dice
//!         fn inject_config(&self, i: usize) {
//!             self.changed_to(vec![(ConfigKey, i)]).unwrap()
//!         }
//!     }
//!
//!     pub trait StaticData {
//!         fn static_data(&self) -> &String;
//!     }
//!
//!     impl StaticData for DiceData {
//!         fn static_data(&self) -> &String {
//!             self.get::<String>().unwrap()
//!         }
//!     }
//!
//!     pub trait SetStaticData {
//!         fn set_static(&mut self, s: String);
//!     }
//!
//!     impl SetStaticData for DiceDataBuilder {
//!         fn set_static(&mut self, s: String) {
//!             self.set(s);
//!         }
//!     }
//! }
//!
//! /// how to use computations
//! use dice::{Dice, cycles::DetectCycles};
//! use std::sync::Arc;
//! use c::*;
//!
//! let mut rt = tokio::runtime::Runtime::new().unwrap();
//! let mut builder = Dice::builder();
//! builder.set_static("len4".into());
//! let engine = builder.build(DetectCycles::Disabled);
//!
//! // inject config
//! let ctx = engine.updater();
//! ctx.inject_config(0);
//!
//! let ctx = ctx.commit();
//!
//! // request the computation from DICE
//! rt.block_on(async {
//!     assert_eq!("aaaaaaaa", &*ctx.my_computation().compute_a(4, "a".into()).await);
//! });
//!
//! let ctx = engine.updater();
//! ctx.inject_config(2);
//!
//! let ctx = ctx.commit();
//!
//! // request the computation from DICE
//! rt.block_on(async {
//!     assert_eq!("aaaaaaaaaa", &*ctx.my_computation().compute_a(4, "a".into()).await);
//! });
//! ```

#![feature(async_closure)]
#![feature(box_syntax)]
#![feature(entry_insert)]
#![feature(fn_traits)]
#![feature(test)]
#![feature(map_try_insert)]
#![feature(map_entry_replace)]
// Plugins
#![cfg_attr(feature = "gazebo_lint", feature(plugin))]
#![cfg_attr(feature = "gazebo_lint", allow(deprecated))] // :(
#![cfg_attr(feature = "gazebo_lint", plugin(gazebo_lint))]
// This sometimes flag false positives where proc-macros expand pass by value into pass by refs
#![allow(clippy::trivially_copy_pass_by_ref)]

#[macro_use]
extern crate gazebo;

#[macro_use]
extern crate tracing;

pub mod cycles;
pub mod data;
mod dice_future;
mod dice_task;
mod future_handle;
mod incremental;
mod injected;
pub mod introspection;
pub(crate) mod key;
mod map;
pub(crate) mod metrics;
pub(crate) mod opaque;
pub(crate) mod projection;
mod sync_handle;
mod transaction;
mod user_data;

#[cfg(test)]
mod tests;

// ctx contains pub data that we don't want to expose, so we hide the whole mod but expose just the
// data we want to expose
mod ctx;

mod api;

use std::fmt::Debug;
use std::io::Write;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::sync::Weak;

use allocative::Allocative;
use async_trait::async_trait;
use dupe::Dupe;
pub use fnv::FnvHashMap as HashMap;
pub use fnv::FnvHashSet as HashSet;
use futures::future::Future;
use futures::StreamExt;
use gazebo::prelude::*;
use parking_lot::RwLock;
use serde::Serializer;
use tokio::sync::watch;

pub use crate::api::error::DiceError;
pub use crate::api::error::DiceResult;
use crate::ctx::ComputationData;
use crate::ctx::DiceComputationImpl;
pub use crate::ctx::DiceComputations;
pub use crate::ctx::DiceEvent;
pub use crate::ctx::DiceEventListener;
use crate::cycles::DetectCycles;
use crate::data::DiceData;
use crate::future_handle::WeakDiceFutureHandle;
use crate::incremental::evaluator::Evaluator;
use crate::incremental::graph::storage_properties::StorageProperties;
use crate::incremental::graph::GraphNode;
use crate::incremental::transaction_ctx::TransactionCtx;
pub use crate::incremental::versions::VersionNumber;
use crate::incremental::versions::VersionTracker;
use crate::incremental::IncrementalComputeProperties;
use crate::incremental::IncrementalEngine;
use crate::incremental::StorageType;
use crate::incremental::ValueWithDeps;
pub use crate::injected::InjectedKey;
use crate::introspection::serialize_dense_graph;
use crate::introspection::serialize_graph;
pub use crate::key::Key;
use crate::key::StoragePropertiesForKey;
use crate::map::DiceMap;
pub use crate::metrics::Metrics;
pub use crate::opaque::OpaqueValue;
pub use crate::projection::DiceProjectionComputations;
pub use crate::projection::ProjectionKey;
use crate::projection::ProjectionKeyProperties;
pub use crate::transaction::DiceTransaction;
pub use crate::transaction::DiceTransactionUpdater;
pub use crate::user_data::UserComputationData;

/// An incremental computation engine that executes arbitrary computations that
/// maps `Key`s to values.
#[derive(Allocative)]
pub struct Dice {
    data: DiceData,
    pub(crate) map: Arc<RwLock<DiceMap>>,
    global_versions: Arc<VersionTracker>,
    detect_cycles: DetectCycles,
    /// Number of active transactions.
    /// Or more precisely, the number of alive transaction context objects.
    active_transaction_count: AtomicU32,
    #[allocative(skip)]
    active_versions_observer: watch::Receiver<usize>,
}

impl Debug for Dice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dice")
            .field("detect_cycles", &self.detect_cycles)
            .finish_non_exhaustive()
    }
}

impl Dice {
    pub fn builder() -> DiceDataBuilder {
        DiceDataBuilder::new()
    }

    fn new(data: DiceData, detect_cycles: DetectCycles) -> Arc<Self> {
        let map = Arc::new(RwLock::new(DiceMap::new()));
        let weak_map = Arc::downgrade(&map);
        let (active_versions_sender, active_versions_observer) = watch::channel(0);

        Arc::new(Dice {
            data,
            map,
            global_versions: VersionTracker::new(box move |update| {
                if let Some(deleted) = update.deleted_version() {
                    if let Some(engines) = weak_map.upgrade() {
                        engines
                            .read()
                            .engines()
                            .map(|engine| engine.gc_version(deleted));
                    }
                }

                // If the corresponding Dice has been dropped, then so be it, ignore the error.
                active_versions_sender.send_replace(update.active_version_count());
            }),
            detect_cycles,
            active_transaction_count: AtomicU32::new(0),
            active_versions_observer,
        })
    }

    pub fn updater(self: &Arc<Dice>) -> DiceTransactionUpdater {
        self.updater_with_data(UserComputationData::new())
    }

    pub fn updater_with_data(
        self: &Arc<Dice>,
        extra: UserComputationData,
    ) -> DiceTransactionUpdater {
        DiceTransactionUpdater::new(self.make_ctx(ComputationData::new(extra, self.detect_cycles)))
    }

    fn make_ctx(self: &Arc<Dice>, extra: ComputationData) -> DiceComputations {
        DiceComputations(Arc::new(DiceComputationImpl::new_transaction(
            self.dupe(),
            self.global_versions.current(),
            self.global_versions.write(),
            extra,
        )))
    }

    /// finds the computation index for the given key
    fn find_cache<K>(self: &Arc<Dice>) -> Arc<IncrementalEngine<StoragePropertiesForKey<K>>>
    where
        K: Key,
    {
        if let Some(cache) = self
            .map
            .read()
            .find_cache_opt::<StoragePropertiesForKey<K>>()
        {
            return cache;
        }

        self.map
            .write()
            .find_cache(|| IncrementalEngine::new(StoragePropertiesForKey::<K>::new(self)))
    }

    fn find_projection_cache<P: ProjectionKey>(
        self: &Arc<Dice>,
    ) -> Arc<IncrementalEngine<ProjectionKeyProperties<P>>>
    where
        P: ProjectionKey,
    {
        if let Some(cache) = self
            .map
            .read()
            .find_cache_opt::<ProjectionKeyProperties<P>>()
        {
            return cache;
        }

        self.map
            .write()
            .find_cache(|| IncrementalEngine::new(ProjectionKeyProperties::<P>::new(self)))
    }

    fn unstable_take(self: &Arc<Dice>) -> DiceMap {
        debug!(msg = "clearing all Dice state");
        let mut map = self.map.write();
        std::mem::replace(&mut map, DiceMap::new())
    }

    pub fn serialize_tsv(
        &self,
        nodes: impl Write,
        edges: impl Write,
        nodes_currently_running: impl Write,
    ) -> anyhow::Result<()> {
        serialize_graph(
            &self.to_introspectable(),
            nodes,
            edges,
            nodes_currently_running,
        )
    }

    pub fn serialize_serde<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: Serializer,
    {
        serialize_dense_graph(&self.to_introspectable(), serializer)?;

        Ok(())
    }

    pub fn detect_cycles(&self) -> &DetectCycles {
        &self.detect_cycles
    }

    pub fn metrics(&self) -> Metrics {
        Metrics::collect(self)
    }

    /// Wait until all active versions have exited.
    pub fn wait_for_idle(&self) -> impl Future<Output = ()> + 'static {
        let obs = self.active_versions_observer.clone();
        let mut obs = tokio_stream::wrappers::WatchStream::new(obs);

        async move {
            while let Some(v) = obs.next().await {
                if v == 0 {
                    break;
                }
            }
        }
    }
}

pub struct DiceDataBuilder(DiceData);

impl DiceDataBuilder {
    fn new() -> Self {
        Self(DiceData::new())
    }

    pub fn set<K: Send + Sync + 'static>(&mut self, val: K) {
        self.0.set(val);
    }

    pub fn build(self, detect_cycles: DetectCycles) -> Arc<Dice> {
        Dice::new(self.0, detect_cycles)
    }
}

#[derive(Clone, Dupe)]
struct Eval(Weak<Dice>);

#[async_trait]
impl<K: Key> IncrementalComputeProperties for StoragePropertiesForKey<K> {
    type DiceTask = WeakDiceFutureHandle<Self>;

    async fn recompute(
        key: &Self::Key,
        engine: &Arc<IncrementalEngine<Self>>,
        transaction_ctx: &Arc<TransactionCtx>,
        extra: &ComputationData,
    ) -> DiceResult<GraphNode<StoragePropertiesForKey<K>>> {
        engine
            .eval_entry_versioned(key, transaction_ctx, extra.subrequest(key)?)
            .await
    }
}

#[async_trait]
impl<K: Key> Evaluator for StoragePropertiesForKey<K> {
    async fn eval(
        &self,
        k: &K,
        transaction_ctx: Arc<TransactionCtx>,
        extra: ComputationData,
    ) -> ValueWithDeps<K::Value> {
        let ctx = DiceComputationImpl::new_for_key_evaluation(
            self.dice
                .upgrade()
                .expect("Dice holds DiceMap so it should still be alive here"),
            transaction_ctx,
            extra,
        );

        let ctx = DiceComputations(ctx);

        let value = k.compute(&ctx).await;

        let both_deps = ctx.0.finalize();

        ValueWithDeps { value, both_deps }
    }
}

pub mod testing {
    use crate::cycles::DetectCycles;
    use crate::user_data::UserComputationData;
    use crate::Dice;
    use crate::DiceDataBuilder;
    use crate::DiceTransactionUpdater;
    use crate::Key;

    /// Testing utility that can be used to build a specific `DiceComputation` where certain keys
    /// of computation mocked to return a specific result.
    ///
    /// TODO(bobyf): ideally, we want something where we don't have to use the specific keys
    /// but rather the computation function, like `mock.expect(|c| c.other_compute(4), "4 res")`
    pub struct DiceBuilder {
        builder: DiceDataBuilder,
        mocked: Vec<Box<dyn FnOnce(&DiceTransactionUpdater) -> anyhow::Result<()>>>,
    }

    impl DiceBuilder {
        pub fn new() -> Self {
            let builder = Dice::builder();

            Self {
                builder,
                mocked: Vec::new(),
            }
        }

        pub fn set_data(mut self, setter: impl FnOnce(&mut DiceDataBuilder)) -> Self {
            setter(&mut self.builder);
            self
        }

        /// mocks the call of compute for the key `expected_k` so that it returns `expected_res`
        pub fn mock_and_return<K>(mut self, expected_k: K, expected_res: K::Value) -> Self
        where
            K: Key,
        {
            self.mocked
                .push(box move |ctx| Ok(ctx.changed_to(vec![(expected_k, expected_res)])?));
            self
        }

        pub fn build(self, extra: UserComputationData) -> anyhow::Result<DiceTransactionUpdater> {
            let dice = self.builder.build(DetectCycles::Enabled);
            let ctx = dice.updater_with_data(extra);

            self.mocked.into_iter().try_for_each(|f| f(&ctx))?;
            Ok(ctx)
        }
    }
}
