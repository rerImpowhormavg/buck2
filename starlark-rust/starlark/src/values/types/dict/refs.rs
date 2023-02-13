/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::cell::RefCell;
use std::cell::RefMut;
use std::ops::Deref;
use std::ops::DerefMut;

use gazebo::cell::ARef;

use crate::coerce::coerce;
use crate::values::dict::value::DictGen;
use crate::values::dict::value::FrozenDictData;
use crate::values::dict::Dict;
use crate::values::type_repr::StarlarkTypeRepr;
use crate::values::FrozenValue;
use crate::values::UnpackValue;
use crate::values::Value;
use crate::values::ValueError;
use crate::values::ValueLike;

/// Borrowed `Dict`.
pub struct DictRef<'v> {
    pub(crate) aref: ARef<'v, Dict<'v>>,
}

/// Mutably borrowed `Dict`.
pub struct DictMut<'v> {
    pub(crate) aref: RefMut<'v, Dict<'v>>,
}

/// Reference to frozen `Dict`.
pub struct FrozenDictRef {
    dict: &'static FrozenDictData,
}

impl<'v> DictRef<'v> {
    /// Downcast the value to a dict.
    pub fn from_value(x: Value<'v>) -> Option<DictRef<'v>> {
        if x.unpack_frozen().is_some() {
            x.downcast_ref::<DictGen<FrozenDictData>>()
                .map(|x| DictRef {
                    aref: ARef::new_ptr(coerce(&x.0)),
                })
        } else {
            let ptr = x.downcast_ref::<DictGen<RefCell<Dict<'v>>>>()?;
            Some(DictRef {
                aref: ARef::new_ref(ptr.0.borrow()),
            })
        }
    }
}

impl<'v> DictMut<'v> {
    /// Downcast the value to a mutable dict reference.
    #[inline]
    pub fn from_value(x: Value<'v>) -> anyhow::Result<DictMut> {
        #[derive(thiserror::Error, Debug)]
        #[error("Value is not dict, value type: `{0}`")]
        struct NotDictError(&'static str);

        #[cold]
        #[inline(never)]
        fn error<'v>(x: Value<'v>) -> anyhow::Error {
            if x.downcast_ref::<DictGen<FrozenDictData>>().is_some() {
                ValueError::CannotMutateImmutableValue.into()
            } else {
                NotDictError(x.get_type()).into()
            }
        }

        let ptr = x.downcast_ref::<DictGen<RefCell<Dict<'v>>>>();
        match ptr {
            None => Err(error(x)),
            Some(ptr) => match ptr.0.try_borrow_mut() {
                Ok(x) => Ok(DictMut { aref: x }),
                Err(_) => Err(ValueError::MutationDuringIteration.into()),
            },
        }
    }
}

impl FrozenDictRef {
    /// Downcast to frozen dict.
    pub fn from_frozen_value(x: FrozenValue) -> Option<FrozenDictRef> {
        x.downcast_ref::<DictGen<FrozenDictData>>()
            .map(|x| FrozenDictRef { dict: &x.0 })
    }

    /// Get value by a string key.
    pub fn get_str(&self, key: &str) -> Option<FrozenValue> {
        self.dict.get_str(key)
    }

    /// Iterate over dict entries.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (FrozenValue, FrozenValue)> {
        self.dict.iter()
    }
}

impl<'v> Deref for DictRef<'v> {
    type Target = Dict<'v>;

    fn deref(&self) -> &Self::Target {
        &self.aref
    }
}

impl<'v> Deref for DictMut<'v> {
    type Target = Dict<'v>;

    fn deref(&self) -> &Self::Target {
        &self.aref
    }
}

impl<'v> DerefMut for DictMut<'v> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.aref
    }
}

impl<'v> StarlarkTypeRepr for DictRef<'v> {
    fn starlark_type_repr() -> String {
        Dict::<'v>::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for DictRef<'v> {
    fn expected() -> String {
        "dict".to_owned()
    }

    fn unpack_value(value: Value<'v>) -> Option<DictRef<'v>> {
        DictRef::from_value(value)
    }
}
