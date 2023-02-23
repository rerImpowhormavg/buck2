/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Trackers that records dependencies and reverse dependencies during execution of requested nodes

use std::sync::Arc;

use allocative::Allocative;
use dupe::Dupe;
use parking_lot::Mutex;

use crate::legacy::incremental::dep_trackers::internals::ComputedDep;
use crate::legacy::incremental::graph::dependencies::ComputedDependency;
use crate::legacy::incremental::graph::GraphNode;
use crate::legacy::incremental::graph::GraphNodeDyn;
use crate::legacy::incremental::IncrementalComputeProperties;
use crate::legacy::incremental::IncrementalEngine;
use crate::versions::VersionNumber;
use crate::HashSet;

/// The 'DepsTracker' is used to record dependencies of a particular compute node by calling
/// 'record' for each dependency, and then getting a list of 'Dependency's at the end by calling
/// 'collect_deps'.
#[derive(Allocative)]
struct RecordingDepsTracker {
    deps: HashSet<Box<dyn ComputedDependency>>,
}

impl RecordingDepsTracker {
    fn new() -> Self {
        Self {
            deps: HashSet::default(),
        }
    }

    /// records k with the given evaluator and engine
    fn record<K>(&mut self, v: VersionNumber, engine: Arc<IncrementalEngine<K>>, node: GraphNode<K>)
    where
        K: IncrementalComputeProperties,
    {
        self.deps.insert(box ComputedDep {
            engine: Arc::downgrade(&engine),
            version: v,
            node,
        });
    }

    fn collect_deps(self) -> HashSet<Box<dyn ComputedDependency>> {
        self.deps
    }
}

#[derive(Allocative)]
struct RecordingRdepsTracker {
    rdeps: Vec<Arc<dyn GraphNodeDyn>>,
}

impl RecordingRdepsTracker {
    fn new() -> Self {
        Self { rdeps: Vec::new() }
    }

    fn record(&mut self, dep: Arc<dyn GraphNodeDyn>) {
        self.rdeps.push(dep)
    }

    fn collect_rdeps(self) -> Vec<Arc<dyn GraphNodeDyn>> {
        self.rdeps
    }
}

#[derive(Allocative)]
struct BothRecordingDepTrackers {
    deps: RecordingDepsTracker,
    rdeps: RecordingRdepsTracker,
}

#[derive(Default)]
pub(crate) struct BothDeps {
    pub(crate) deps: HashSet<Box<dyn ComputedDependency>>,
    pub(crate) rdeps: Vec<Arc<dyn GraphNodeDyn>>,
}

impl BothDeps {
    pub(crate) fn only_one_dep<S: IncrementalComputeProperties>(
        version: VersionNumber,
        node: GraphNode<S>,
        incremental_engine: &Arc<IncrementalEngine<S>>,
    ) -> BothDeps {
        let dep: Box<dyn ComputedDependency> = box ComputedDep::<S> {
            engine: Arc::downgrade(incremental_engine),
            version,
            node: node.dupe(),
        };
        BothDeps {
            deps: HashSet::from_iter([dep]),
            rdeps: Vec::from_iter([node.into_dyn()]),
        }
    }
}

#[derive(Allocative)]
enum BothDepTrackersImpl {
    Noop,
    Recording(Mutex<BothRecordingDepTrackers>),
}

#[derive(Allocative)]
pub(crate) struct BothDepTrackers(BothDepTrackersImpl);

/// There are two variants, a 'Recording' tracker and a 'Noop' tracker. The 'Noop' tracker never
/// tracks any dependencies such that 'collect_deps' is always empty. The 'Recording' tracker will
/// actually track the dependencies.
impl BothDepTrackers {
    pub(crate) fn noop() -> BothDepTrackers {
        BothDepTrackers(BothDepTrackersImpl::Noop)
    }

    pub(crate) fn recording() -> BothDepTrackers {
        BothDepTrackers(BothDepTrackersImpl::Recording(Mutex::new(
            BothRecordingDepTrackers {
                deps: RecordingDepsTracker::new(),
                rdeps: RecordingRdepsTracker::new(),
            },
        )))
    }

    /// records k with the given evaluator and engine
    pub(crate) fn record<K>(
        &self,
        v: VersionNumber,
        engine: Arc<IncrementalEngine<K>>,
        node: GraphNode<K>,
    ) where
        K: IncrementalComputeProperties,
    {
        match &self.0 {
            BothDepTrackersImpl::Noop => {}
            BothDepTrackersImpl::Recording(recording) => {
                let mut recording = recording.lock();
                let BothRecordingDepTrackers { deps, rdeps } = &mut *recording;
                deps.record(v, engine, node.dupe());
                rdeps.record(node.into_dyn());
            }
        }
    }

    pub(crate) fn collect_deps(self) -> BothDeps {
        match self.0 {
            BothDepTrackersImpl::Noop => BothDeps::default(),
            BothDepTrackersImpl::Recording(recording) => {
                let BothRecordingDepTrackers { deps, rdeps } = recording.into_inner();
                let deps = deps.collect_deps();
                let rdeps = rdeps.collect_rdeps();
                BothDeps { deps, rdeps }
            }
        }
    }
}

mod internals {
    use std::any::type_name;
    use std::fmt;
    use std::fmt::Debug;
    use std::fmt::Display;
    use std::fmt::Formatter;
    use std::hash::Hash;
    use std::hash::Hasher;
    use std::sync::Arc;
    use std::sync::Weak;

    use allocative::Allocative;
    use async_trait::async_trait;
    use dupe::Dupe;
    use gazebo::cmp::PartialEqAny;

    use crate::api::error::DiceResult;
    use crate::introspection::graph::AnyKey;
    use crate::legacy::ctx::ComputationData;
    use crate::legacy::incremental::graph::GraphNode;
    use crate::legacy::incremental::graph::GraphNodeDyn;
    use crate::legacy::incremental::graph::ReadOnlyHistory;
    use crate::legacy::incremental::graph::VersionedGraphKeyRef;
    use crate::legacy::incremental::transaction_ctx::TransactionCtx;
    use crate::legacy::incremental::versions::MinorVersion;
    use crate::legacy::incremental::ComputedDependency;
    use crate::legacy::incremental::Dependency;
    use crate::legacy::incremental::IncrementalComputeProperties;
    use crate::legacy::incremental::IncrementalEngine;
    use crate::versions::VersionNumber;

    #[derive(Allocative)]
    pub(crate) struct ComputedDep<K: IncrementalComputeProperties> {
        pub(crate) engine: Weak<IncrementalEngine<K>>,
        pub(crate) version: VersionNumber,
        pub(crate) node: GraphNode<K>,
    }

    impl<K> ComputedDependency for ComputedDep<K>
    where
        K: IncrementalComputeProperties,
    {
        fn get_history(&self) -> ReadOnlyHistory {
            self.node.get_history()
        }

        fn into_dependency(self: Box<Self>) -> Box<dyn Dependency> {
            box Dep {
                engine: self.engine,
                k: self.node.key().clone(),
            }
        }

        fn get_key_equality(&self) -> (PartialEqAny, VersionNumber) {
            (PartialEqAny::new(self.node.key()), self.version)
        }

        fn hash(&self, mut state: &mut dyn Hasher) {
            self.node.key().hash(&mut state);
            self.version.hash(&mut state);
        }

        fn is_valid(&self) -> bool {
            self.node.is_valid()
        }
    }

    impl<K> Debug for ComputedDep<K>
    where
        K: IncrementalComputeProperties,
    {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
            write!(
                f,
                "ComputedDependency(({:?}={:?}) -> {:?}, version={:?})",
                type_name::<K::Key>(),
                self.node.key(),
                type_name::<K::Value>(),
                self.version,
            )
        }
    }

    #[derive(Allocative)]
    pub(crate) struct Dep<K>
    where
        K: IncrementalComputeProperties,
    {
        pub(crate) engine: Weak<IncrementalEngine<K>>,
        pub(crate) k: K::Key,
    }

    impl<K> Dep<K>
    where
        K: IncrementalComputeProperties,
    {
        pub(crate) fn engine(&self) -> Arc<IncrementalEngine<K>> {
            self.engine.upgrade().expect(
                "IncrementalEngine should not be destroyed because IncrementalEngine owns Dep",
            )
        }
    }

    #[async_trait]
    impl<K> Dependency for Dep<K>
    where
        K: IncrementalComputeProperties,
    {
        #[instrument(level = "info", skip(self, transaction_ctx, extra), fields(k = %self.k, version = %transaction_ctx.get_version()))]
        async fn recompute(
            &self,
            transaction_ctx: &Arc<TransactionCtx>,
            extra: &ComputationData,
        ) -> DiceResult<(Box<dyn ComputedDependency>, Arc<dyn GraphNodeDyn>)> {
            let res = K::recompute(&self.k, &self.engine(), transaction_ctx, extra).await?;

            Ok((
                box ComputedDep {
                    engine: self.engine.dupe(),
                    version: transaction_ctx.get_version(),
                    node: res.dupe(),
                },
                res.into_dyn(),
            ))
        }

        fn lookup_node(&self, v: VersionNumber, mv: MinorVersion) -> Option<Arc<dyn GraphNodeDyn>> {
            if let Some(node) = self
                .engine()
                .versioned_cache
                .get(VersionedGraphKeyRef::new(v, &self.k), mv)
                .unpack_match()
            {
                Some(node.dupe().into_dyn())
            } else {
                None
            }
        }

        fn dirty(&self, v: VersionNumber) {
            self.engine().dirty(self.k.clone(), v, false)
        }

        fn get_key_equality(&self) -> PartialEqAny {
            PartialEqAny::new(&self.k)
        }

        fn hash(&self, mut state: &mut dyn Hasher) {
            self.k.hash(&mut state)
        }

        fn introspect(&self) -> AnyKey {
            AnyKey::new(self.k.clone())
        }

        fn to_key_any(&self) -> &dyn std::any::Any {
            K::to_key_any(&self.k)
        }
    }

    impl<K> Debug for Dep<K>
    where
        K: IncrementalComputeProperties,
    {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
            write!(
                f,
                "Dependency(({:?}={:?}) -> {:?})",
                type_name::<K::Key>(),
                self.k,
                type_name::<K::Value>()
            )
        }
    }

    impl<K> Display for Dep<K>
    where
        K: IncrementalComputeProperties,
    {
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            write!(f, "Dependency({})", self.k)
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;
    use std::sync::Weak;

    pub(crate) use crate::legacy::incremental::dep_trackers::internals::ComputedDep;
    pub(crate) use crate::legacy::incremental::dep_trackers::internals::Dep;
    use crate::legacy::incremental::graph::GraphNode;
    use crate::legacy::incremental::graph::OccupiedGraphNode;
    use crate::legacy::incremental::IncrementalComputeProperties;
    use crate::legacy::incremental::IncrementalEngine;
    use crate::versions::VersionNumber;

    pub(crate) trait DepExt<K: IncrementalComputeProperties> {
        fn testing_new(engine: Weak<IncrementalEngine<K>>, k: K::Key) -> Self;
    }

    impl<K> DepExt<K> for Dep<K>
    where
        K: IncrementalComputeProperties,
    {
        fn testing_new(engine: Weak<IncrementalEngine<K>>, k: K::Key) -> Self {
            Dep { engine, k }
        }
    }

    pub(crate) trait ComputedDepExt<K: IncrementalComputeProperties> {
        fn testing_new(
            engine: Weak<IncrementalEngine<K>>,
            version: VersionNumber,
            node: Arc<OccupiedGraphNode<K>>,
        ) -> Self;
    }

    impl<K> ComputedDepExt<K> for ComputedDep<K>
    where
        K: IncrementalComputeProperties,
    {
        fn testing_new(
            engine: Weak<IncrementalEngine<K>>,
            version: VersionNumber,
            node: Arc<OccupiedGraphNode<K>>,
        ) -> Self {
            ComputedDep {
                engine,
                version,
                node: GraphNode::occupied(node),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dupe::Dupe;

    use crate::legacy::ctx::testing::ComputationDataExt;
    use crate::legacy::ctx::ComputationData;
    use crate::legacy::incremental::dep_trackers::BothDeps;
    use crate::legacy::incremental::dep_trackers::RecordingDepsTracker;
    use crate::legacy::incremental::dep_trackers::RecordingRdepsTracker;
    use crate::legacy::incremental::evaluator::testing::EvaluatorFn;
    use crate::legacy::incremental::evaluator::testing::EvaluatorUnreachable;
    use crate::legacy::incremental::graph::OccupiedGraphNode;
    use crate::legacy::incremental::history::CellHistory;
    use crate::legacy::incremental::testing::ComputedDependencyExt;
    use crate::legacy::incremental::IncrementalEngine;
    use crate::legacy::incremental::TransactionCtx;
    use crate::versions::VersionNumber;
    use crate::HashSet;
    use crate::ValueWithDeps;

    #[test]
    fn recording_rdeps_tracker_tracks_rdeps() {
        let mut rdeps_tracker = RecordingRdepsTracker::new();

        let node = Arc::new(OccupiedGraphNode::<EvaluatorFn<usize, usize>>::new(
            1337,
            2,
            CellHistory::verified(VersionNumber::new(0)),
        ));
        rdeps_tracker.record(node.dupe());
        let tracked = rdeps_tracker.collect_rdeps();

        assert_eq!(tracked.len(), 1);
    }

    #[tokio::test]
    async fn recording_deps_tracker_tracks_deps() -> anyhow::Result<()> {
        let mut deps_tracker = RecordingDepsTracker::new();
        // set up so that we have keys 2 and 3 with a history of VersionNumber(1)
        let fn_for_2_and_3 = |k| ValueWithDeps {
            value: k,
            both_deps: BothDeps::default(),
        };

        let engine = IncrementalEngine::new(EvaluatorFn::new(async move |k| fn_for_2_and_3(k)));

        let ctx = Arc::new(TransactionCtx::testing_new(VersionNumber::new(1)));

        let node1 = engine
            .eval_entry_versioned(&2, &ctx, ComputationData::testing_new())
            .await?;
        let node2 = engine
            .eval_entry_versioned(&3, &ctx, ComputationData::testing_new())
            .await?;

        deps_tracker.record(VersionNumber::new(1), engine.dupe(), node1);
        deps_tracker.record(VersionNumber::new(1), engine.dupe(), node2);

        let deps = deps_tracker.collect_deps();

        let expected = HashSet::from_iter([
            ComputedDependencyExt::<EvaluatorUnreachable<_, i32>>::testing_raw(
                2,
                VersionNumber::new(1),
                true,
            ),
            ComputedDependencyExt::<EvaluatorUnreachable<_, i32>>::testing_raw(
                3,
                VersionNumber::new(1),
                true,
            ),
        ]);
        assert_eq!(deps, expected);

        Ok(())
    }
}
