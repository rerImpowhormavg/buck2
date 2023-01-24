/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt;
use std::fmt::Display;

use allocative::Allocative;
use anyhow::Context;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use derive_more::Display;
use derive_more::From;
use gazebo::coerce::Coerce;
use starlark::any::ProvidesStaticType;
use starlark::starlark_complex_value;
use starlark::starlark_simple_value;
use starlark::starlark_type;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::StarlarkDocs;

use crate::bxl::starlark_defs::nodes::unconfigured::StarlarkTargetNode;

#[derive(
    Debug,
    Clone,
    Coerce,
    Trace,
    Freeze,
    ProvidesStaticType,
    NoSerialize,
    Allocative
)]
#[repr(C)]
pub struct StarlarkTargetNodeCoercedAttributesGen<V> {
    pub(super) inner: V,
}

impl<V: Display> Display for StarlarkTargetNodeCoercedAttributesGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Traversal({})", self.inner)
    }
}

starlark_complex_value!(pub StarlarkTargetNodeCoercedAttributes);

impl<'v, V: ValueLike<'v> + 'v> StarlarkValue<'v> for StarlarkTargetNodeCoercedAttributesGen<V>
where
    Self: ProvidesStaticType,
{
    starlark_type!("starlark_attributes");

    fn iterate<'a>(
        &'a self,
        heap: &'v Heap,
    ) -> anyhow::Result<Box<dyn Iterator<Item = Value<'v>> + 'a>>
    where
        'v: 'a,
    {
        let starlark_target_node = self
            .inner
            .downcast_ref::<StarlarkTargetNode>()
            .context("invalid inner")?;
        let target_node = &starlark_target_node.0;
        Ok(box target_node
            .attrs(AttrInspectOptions::All)
            .map(|a| heap.alloc((a.name, StarlarkCoercedAttr::from(a.value.clone())))))
    }
}

#[derive(Debug, Display, ProvidesStaticType, From, Allocative, StarlarkDocs)]
#[derive(NoSerialize)] // TODO probably should be serializable the same as how queries serialize
#[display(fmt = "{:?}", self)]
#[starlark_docs(directory = "bxl")]
pub struct StarlarkCoercedAttr(pub CoercedAttr);

starlark_simple_value!(StarlarkCoercedAttr);

/// Coerced attr from an unconfigured target node.
impl<'v> StarlarkValue<'v> for StarlarkCoercedAttr {
    starlark_type!("coerced_attr");
}
