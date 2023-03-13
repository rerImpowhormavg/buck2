/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use allocative::Allocative;
use gazebo::variants::UnpackVariants;

use crate::build::BuildTargetResult;

#[derive(Clone, Debug, derive_more::Display, UnpackVariants, Allocative)]
pub enum BxlBuildResult {
    None,
    #[display(fmt = "build result")]
    Built(BuildTargetResult),
}

impl BxlBuildResult {
    pub fn new(result: Option<BuildTargetResult>) -> Self {
        match result {
            Some(result) => Self::Built(result),
            None => Self::None,
        }
    }
}
