/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;

use async_trait::async_trait;

use crate::legacy::ctx::ComputationData;
use crate::legacy::dice_futures::dice_task::DiceTask;
use crate::legacy::incremental::graph::GraphNode;
use crate::legacy::incremental::IncrementalComputeProperties;
use crate::legacy::incremental::IncrementalEngine;
use crate::TransactionCtx;
use crate::ValueWithDeps;
use crate::WeakDiceFutureHandle;

#[async_trait]
pub(crate) trait Recompute: IncrementalComputeProperties {
    /// Dice task executed in the `IncrementalEngine` for compute or recompute.
    type DiceTask: DiceTask;

    async fn recompute(
        key: &Self::Key,
        engine: &Arc<IncrementalEngine<Self>>,
        transaction_ctx: &Arc<TransactionCtx>,
        extra: &ComputationData,
    ) -> GraphNode<Self>;
}

#[async_trait]
pub(crate) trait Evaluator:
    IncrementalComputeProperties<DiceTask = WeakDiceFutureHandle<Self>>
{
    async fn eval(
        &self,
        k: &Self::Key,
        transaction_ctx: Arc<TransactionCtx>,
        extra: ComputationData,
    ) -> ValueWithDeps<Self::Value>;
}

#[cfg(test)]
pub(crate) mod testing {
    use std::fmt;
    use std::fmt::Debug;
    use std::fmt::Display;
    use std::future::Future;
    use std::hash::Hash;
    use std::pin::Pin;
    use std::sync::Arc;

    use allocative::Allocative;
    use async_trait::async_trait;
    use dupe::Dupe;
    use gazebo::prelude::*;

    use crate::api::error::DiceResult;
    use crate::api::storage_type::StorageType;
    use crate::legacy::ctx::ComputationData;
    use crate::legacy::incremental::evaluator::Evaluator;
    use crate::legacy::incremental::graph::storage_properties::StorageProperties;
    use crate::legacy::incremental::graph::GraphNode;
    use crate::legacy::incremental::Computable;
    use crate::legacy::incremental::IncrementalComputeProperties;
    use crate::legacy::incremental::IncrementalEngine;
    use crate::TransactionCtx;
    use crate::ValueWithDeps;
    use crate::WeakDiceFutureHandle;

    /// Evaluator which panics on attempt to evaluate.
    #[derive(Default_, Allocative)]
    #[allocative(bound = "")]
    pub(crate) struct EvaluatorUnreachable<K, V> {
        _kv: std::marker::PhantomData<fn(K) -> V>,
    }

    impl<K, V> fmt::Debug for EvaluatorUnreachable<K, V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("EvaluatorUnreachable")
                .finish_non_exhaustive()
        }
    }

    impl<
        K: Send + Sync + Hash + Eq + Display + Debug + Clone + Allocative + 'static,
        V: Send + Sync + PartialEq + Dupe + Allocative + 'static,
    > StorageProperties for EvaluatorUnreachable<K, V>
    {
        type Key = K;
        type Value = V;

        fn key_type_name() -> &'static str {
            "TestingUnreachable"
        }

        fn storage_type(&self) -> StorageType {
            unreachable!()
        }

        fn equality(&self, x: &Self::Value, y: &Self::Value) -> bool {
            x == y
        }

        fn validity(&self, _x: &Self::Value) -> bool {
            true
        }

        fn to_key_any(_key: &Self::Key) -> &dyn std::any::Any {
            unreachable!()
        }
    }

    #[async_trait]
    impl<
        K: Send + Sync + Hash + Eq + Display + Debug + Clone + Allocative + 'static,
        V: Send + Sync + PartialEq + Dupe + Allocative + 'static,
    > IncrementalComputeProperties for EvaluatorUnreachable<K, V>
    {
        type DiceTask = WeakDiceFutureHandle<Self>;

        async fn recompute(
            _key: &Self::Key,
            _engine: &Arc<IncrementalEngine<Self>>,
            _transaction_ctx: &Arc<TransactionCtx>,
            _extra: &ComputationData,
        ) -> DiceResult<GraphNode<Self>> {
            unreachable!()
        }
    }

    #[async_trait]
    impl<
        K: Send + Sync + Hash + Eq + Display + Debug + Clone + Allocative + 'static,
        V: Send + Sync + PartialEq + Dupe + Allocative + 'static,
    > Evaluator for EvaluatorUnreachable<K, V>
    {
        async fn eval(
            &self,
            _k: &Self::Key,
            _transaction_ctx: Arc<TransactionCtx>,
            _extra: ComputationData,
        ) -> ValueWithDeps<Self::Value> {
            unreachable!()
        }
    }

    /// Evaluator which invokes boxed closure on evaluation.
    #[derive(Allocative)]
    #[allocative(bound = "")]
    pub(crate) struct EvaluatorFn<K, V> {
        #[allocative(skip)]
        f: Pin<
            Box<
                dyn Fn(K) -> Pin<Box<dyn Future<Output = ValueWithDeps<V>> + Sync + Send + 'static>>
                    + Send
                    + Sync
                    + 'static,
            >,
        >,
    }

    impl<K, V> EvaluatorFn<K, V> {
        pub(crate) fn new<F, FUT>(f: F) -> Self
        where
            FUT: Future<Output = ValueWithDeps<V>> + Sync + Send + 'static,
            F: FnOnce(K) -> FUT + Clone + 'static + Sync + Send + 'static,
        {
            Self {
                f: Box::pin(move |k| {
                    let f = f.clone();
                    Box::pin(f(k))
                }),
            }
        }
    }

    impl<K, V> Debug for EvaluatorFn<K, V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("EvaluatorFn").finish_non_exhaustive()
        }
    }

    impl<
        K: Clone + Eq + Hash + Display + Debug + Send + Sync + Allocative + 'static,
        V: Dupe + PartialEq + Send + Sync + Allocative + 'static,
    > StorageProperties for EvaluatorFn<K, V>
    {
        type Key = K;
        type Value = V;

        fn key_type_name() -> &'static str {
            "EvaluatorFn"
        }

        fn storage_type(&self) -> StorageType {
            StorageType::LastN(1)
        }

        fn equality(&self, x: &Self::Value, y: &Self::Value) -> bool {
            x == y
        }

        fn validity(&self, _x: &Self::Value) -> bool {
            true
        }

        fn to_key_any(key: &Self::Key) -> &dyn std::any::Any {
            key
        }
    }

    #[async_trait]
    impl<K, V> IncrementalComputeProperties for EvaluatorFn<K, V>
    where
        K: Computable + 'static,
        V: Dupe + PartialEq + Allocative + Send + Sync + 'static,
    {
        type DiceTask = WeakDiceFutureHandle<Self>;

        async fn recompute(
            key: &K,
            engine: &Arc<IncrementalEngine<Self>>,
            transaction_ctx: &Arc<TransactionCtx>,
            extra: &ComputationData,
        ) -> DiceResult<GraphNode<Self>> {
            engine
                .eval_entry_versioned(key, transaction_ctx, extra.subrequest::<Self>(key)?)
                .await
        }
    }

    #[async_trait]
    impl<K, V> Evaluator for EvaluatorFn<K, V>
    where
        K: Clone + Eq + Hash + Display + Debug + Allocative + Send + Sync + 'static,
        V: Dupe + PartialEq + Allocative + Send + Sync + 'static,
    {
        async fn eval(
            &self,
            k: &K,
            _: Arc<TransactionCtx>,
            _extra: ComputationData,
        ) -> ValueWithDeps<V> {
            (self.f)(k.clone()).await
        }
    }
}
