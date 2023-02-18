/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

use allocative::Allocative;
use dupe::Dupe;
use futures::FutureExt;
use tokio::sync::oneshot;

use crate::api::error::DiceError;
use crate::api::error::DiceResult;
use crate::api::key::Key;
use crate::api::user_data::UserComputationData;
use crate::impls::core::state::StateRequest;
use crate::impls::ctx::PerComputeCtx;
use crate::impls::key::DiceKey;
use crate::impls::key::DiceKeyDynExt;
use crate::impls::value::DiceComputedValue;
use crate::impls::value::DiceValue;
use crate::DiceModern;
use crate::HashMap;

// TODO fill this more
#[derive(Allocative)]
pub(crate) struct TransactionUpdater {
    dice: Arc<DiceModern>,
    scheduled_changes: Changes,
    user_data: Arc<UserComputationData>,
}

impl TransactionUpdater {
    pub(crate) fn new(dice: Arc<DiceModern>, user_data: Arc<UserComputationData>) -> Self {
        Self {
            dice: dice.dupe(),
            scheduled_changes: Changes::new(dice),
            user_data,
        }
    }

    /// Records a set of `Key`s as changed so that they, and any dependents will
    /// be recomputed on the next set of requests at the next version.
    pub(crate) fn changed<K, I>(&mut self, changed: I) -> DiceResult<()>
    where
        K: Key,
        I: IntoIterator<Item = K> + Send + Sync + 'static,
    {
        changed
            .into_iter()
            .try_for_each(|k| self.scheduled_changes.change(k, ChangeType::Invalidate))
    }

    /// Records a set of `Key`s as changed to a particular value so that any
    /// dependents will be recomputed on the next set of requests. The
    /// `Key`s themselves will be update to the new value such that they
    /// will not need to be recomputed as long as they aren't recorded to be
    /// `changed` again (or invalidated by other means). Calling this method
    /// does not in anyway alter the types of the key such that they
    /// permanently becomes a special "inject value only" key.
    pub(crate) fn changed_to<K, I>(&mut self, changed: I) -> DiceResult<()>
    where
        K: Key,
        I: IntoIterator<Item = (K, K::Value)> + Send + Sync + 'static,
    {
        changed.into_iter().try_for_each(|(k, new_value)| {
            self.scheduled_changes.change(
                k,
                ChangeType::UpdateValue(DiceValue::new(DiceComputedValue::<K>::new(new_value))),
            )
        })
    }

    /// Commit the changes registered via 'changed' and 'changed_to' to the current newest version.
    pub(crate) fn commit(self) -> impl Future<Output = PerComputeCtx> {
        let (tx, rx) = oneshot::channel();
        self.dice.state_handle.request(StateRequest::UpdateState {
            changes: self.scheduled_changes.changes.into_iter().collect(),
            resp: tx,
        });

        let dice = self.dice.dupe();

        rx.map(|transaction| PerComputeCtx::new(transaction.unwrap(), self.user_data, dice))
    }

    /// Commit the changes registered via 'changed' and 'changed_to' to the current newest version,
    /// replacing the user data with the given set
    pub(crate) fn commit_with_data(
        self,
        extra: UserComputationData,
    ) -> impl Future<Output = PerComputeCtx> {
        let (tx, rx) = oneshot::channel();
        self.dice.state_handle.request(StateRequest::UpdateState {
            changes: self.scheduled_changes.changes.into_iter().collect(),
            resp: tx,
        });

        let dice = self.dice.dupe();

        rx.map(|transaction| PerComputeCtx::new(transaction.unwrap(), Arc::new(extra), dice))
    }

    pub(crate) fn existing_state(&self) -> impl Future<Output = PerComputeCtx> {
        async move { unimplemented!("todo") }
    }
}

#[derive(Allocative)]
struct Changes {
    changes: HashMap<DiceKey, ChangeType>,
    dice: Arc<DiceModern>,
}

impl Changes {
    pub(crate) fn new(dice: Arc<DiceModern>) -> Self {
        Self {
            changes: HashMap::default(),
            dice,
        }
    }

    pub(crate) fn change<K: Key>(&mut self, key: K, change: ChangeType) -> DiceResult<()> {
        let key = self.dice.key_index.index(Cow::<K>::Owned(key));
        if self.changes.insert(key, change).is_some() {
            Err(DiceError::duplicate(
                self.dice.key_index.get(key).downcast::<K>().unwrap(),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Allocative, Debug)]
pub(crate) enum ChangeType {
    /// Just invalidate the key
    Invalidate,
    /// Update the key to the given value
    UpdateValue(DiceValue),
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use allocative::Allocative;
    use assert_matches::assert_matches;
    use async_trait::async_trait;
    use derive_more::Display;
    use dupe::Dupe;

    use crate::api::computations::DiceComputations;
    use crate::api::data::DiceData;
    use crate::api::key::Key;
    use crate::api::user_data::UserComputationData;
    use crate::impls::dice::DiceModern;
    use crate::impls::transaction::ChangeType;
    use crate::impls::transaction::TransactionUpdater;

    #[test]
    fn changes_are_recorded() -> anyhow::Result<()> {
        let dice = DiceModern::new(DiceData::new());
        let mut updater =
            TransactionUpdater::new(dice.dupe(), Arc::new(UserComputationData::new()));

        #[derive(Allocative, Clone, PartialEq, Eq, Hash, Debug, Display)]
        struct K(usize);

        #[async_trait]
        impl Key for K {
            type Value = usize;

            async fn compute(&self, _ctx: &DiceComputations) -> Self::Value {
                unimplemented!("test")
            }

            fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
                unimplemented!("test")
            }
        }

        updater.changed(vec![K(1), K(2)])?;

        updater.changed_to(vec![(K(3), 3), (K(4), 4)])?;

        assert_matches!(
            updater
                .scheduled_changes
                .changes
                .get(&dice.key_index.index(Cow::<K>::Owned(K(1)))),
            Some(ChangeType::Invalidate)
        );
        assert_matches!(
            updater
                .scheduled_changes
                .changes
                .get(&dice.key_index.index(Cow::<K>::Owned(K(2)))),
            Some(ChangeType::Invalidate)
        );

        assert_matches!(
        updater
            .scheduled_changes
            .changes
            .get(&dice.key_index.index(Cow::<K>::Owned(K(3)))),
        Some(ChangeType::UpdateValue(x)) if *x.0.downcast_ref::<usize>().unwrap() == 3
            );

        assert_matches!(
        updater
            .scheduled_changes
            .changes
            .get(&dice.key_index.index(Cow::<K>::Owned(K(4)))),
        Some(ChangeType::UpdateValue(x)) if *x.0.downcast_ref::<usize>().unwrap() == 4
            );

        assert!(updater.changed(vec![K(1)]).is_err());

        Ok(())
    }
}
