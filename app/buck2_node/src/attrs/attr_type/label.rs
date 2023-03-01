/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use allocative::Allocative;
use buck2_core::provider::label::ProvidersLabel;
use dupe::Dupe;

use crate::attrs::attr_type::attr_literal::AttrLiteral;
use crate::attrs::configuration_context::AttrConfigurationContext;
use crate::attrs::configured_attr::ConfiguredAttr;

#[derive(Debug, Eq, PartialEq, Hash, Clone, Dupe, Allocative)]
pub struct LabelAttrType;

impl LabelAttrType {
    pub(crate) fn configure(
        ctx: &dyn AttrConfigurationContext,
        label: &ProvidersLabel,
    ) -> anyhow::Result<AttrLiteral<ConfiguredAttr>> {
        Ok(AttrLiteral::Label(Box::new(ctx.configure_target(label))))
    }
}
