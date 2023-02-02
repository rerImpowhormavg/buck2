/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#[cfg(test)]
mod tests;

use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;

use dupe::Dupe;

use crate::api::error::DiceResult;
use crate::api::key::Key;
use crate::ctx::DiceComputationImpl;
use crate::incremental::dep_trackers::BothDeps;
use crate::incremental::graph::GraphNode;
use crate::incremental::IncrementalEngine;
use crate::ProjectionKey;
use crate::StoragePropertiesForKey;

/// Computed value which is not directly visible to user.
///
/// The value can be accessed only via "projection" operation,
/// so projection result is recorded as a dependency
/// of a computation which requested the opaqued value,
/// but the opaque value key is not.
pub struct OpaqueValue<'a, K: Key> {
    /// Computed value.
    pub(crate) value: GraphNode<StoragePropertiesForKey<K>>,
    /// Computations which requested this value, parent of K.
    pub(crate) parent_computations: &'a Arc<DiceComputationImpl>,
    incremental_engine: Arc<IncrementalEngine<StoragePropertiesForKey<K>>>,
}

impl<'a, K> Debug for OpaqueValue<'a, K>
where
    K: Key,
    K::Value: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpaqueValue")
            .field("key", self.value.key())
            .field("value", self.value.val())
            .finish_non_exhaustive()
    }
}

impl<'a, K: Key> OpaqueValue<'a, K> {
    pub(crate) fn new(
        value: GraphNode<StoragePropertiesForKey<K>>,
        parent_computations: &'a Arc<DiceComputationImpl>,
        incremental_engine: Arc<IncrementalEngine<StoragePropertiesForKey<K>>>,
    ) -> OpaqueValue<'a, K> {
        OpaqueValue {
            value,
            parent_computations,
            incremental_engine,
        }
    }

    pub(crate) fn key(&self) -> &K {
        self.value.key()
    }

    pub(crate) fn as_both_deps(&self) -> BothDeps {
        BothDeps::only_one_dep(
            self.parent_computations.transaction_ctx.get_version(),
            self.value.dupe(),
            &self.incremental_engine,
        )
    }

    /// Get a value and record parent computation dependency on `K`.
    pub(crate) fn into_value(self) -> K::Value {
        let value = self.value.val().dupe();

        // Track dependencies.
        self.parent_computations.dep_trackers.record(
            self.parent_computations.transaction_ctx.get_version(),
            self.incremental_engine,
            self.value,
        );

        value
    }

    pub fn projection<P>(&self, projection_key: &P) -> DiceResult<P::Value>
    where
        P: ProjectionKey<DeriveFromKey = K>,
    {
        self.parent_computations
            .compute_projection_sync(self, projection_key)
    }
}
