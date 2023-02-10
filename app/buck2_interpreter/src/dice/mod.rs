/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! The dice module contains the interpreter's integration with dice. This
//! module contains the extension traits that we implement for `Arc<DiceCtx>`
//! (the implementations of the traits are in the submodules).
//!
//! Several of these extension traits provide implementations of our delegate/DI
//! traits that are themselves build on dice (ex DiceInterpreterFileOps
//! implements InterpreterFileOps by basically putting DefaultInterpreterFileOps
//! onto the dice graph).
use std::sync::Arc;

use buck2_common::package_listing::dice::HasPackageListingResolver;

use crate::extra::InterpreterConfiguror;

pub mod calculation;
mod interpreter;
pub mod interpreter_setup;
pub mod starlark_profiler;
pub mod starlark_types;

#[async_trait]
pub trait HasInterpreterContext {
    async fn get_interpreter_configuror(&self) -> anyhow::Result<Arc<dyn InterpreterConfiguror>>;
}

pub trait SetInterpreterContext {
    fn set_interpreter_context(
        &mut self,
        interpreter_configuror: Arc<dyn InterpreterConfiguror>,
    ) -> anyhow::Result<()>;
}
