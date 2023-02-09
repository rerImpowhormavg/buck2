/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::any::Any;
use std::sync::Arc;

use allocative::Allocative;
use dupe::Dupe;
use gazebo::cmp::PartialEqAny;

use crate::api::key::Key;
use crate::impls::hash;

/// Type erased internal dice key
#[derive(
    Allocative, Eq, PartialEq, Clone, Copy, Dupe, Hash, Debug, Ord, PartialOrd
)]
pub(crate) struct DiceKey {
    /// represented as indexes into an internal index based map
    pub(crate) index: u32,
}

pub(crate) trait DiceKeyDyn: Allocative + Send + Sync + 'static {
    fn eq_any(&self) -> PartialEqAny;

    fn hash(&self) -> u64;

    fn as_any(&self) -> &dyn Any;
}

impl<K> DiceKeyDyn for K
where
    K: Key,
{
    fn eq_any(&self) -> PartialEqAny {
        PartialEqAny::new(self)
    }

    fn hash(&self) -> u64 {
        hash::key_hash(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) trait DiceKeyDynExt {
    fn downcast<K: Key>(self) -> Option<Arc<K>>;
}

impl DiceKeyDynExt for Arc<dyn DiceKeyDyn> {
    fn downcast<K: Key>(self) -> Option<Arc<K>> {
        if self.as_any().is::<K>() {
            Some(unsafe { Arc::from_raw(Arc::into_raw(self).cast()) })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use allocative::Allocative;
    use derive_more::Display;
    use dupe::Dupe;

    use crate::api::computations::DiceComputations;
    use crate::api::key::Key;
    use crate::impls::key::DiceKeyDyn;
    use crate::impls::key::DiceKeyDynExt;

    #[test]
    fn downcast_key_does_not_increase_refs() {
        #[derive(Allocative, Debug, Display, Clone, Dupe, Eq, PartialEq, Hash)]
        struct TestK;

        #[async_trait::async_trait]
        impl Key for TestK {
            type Value = ();

            async fn compute(&self, _ctx: &DiceComputations) -> Self::Value {
                unimplemented!("test")
            }

            fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
                unimplemented!("test")
            }
        }

        let erased: Arc<dyn DiceKeyDyn> = Arc::new(TestK);

        let downcast = erased.downcast::<TestK>();
        assert!(downcast.is_some());
        let downcast = downcast.unwrap();

        assert_eq!(&*downcast, &TestK);

        // no extra copies
        assert_eq!(Arc::strong_count(&downcast), 1);
    }
}
