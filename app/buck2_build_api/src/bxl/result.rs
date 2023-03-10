/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use allocative::Allocative;
use buck2_core::fs::buck_out_path::BuckOutPath;
use indexmap::IndexSet;

use crate::artifact_groups::ArtifactGroup;
use crate::bxl::build_result::BxlBuildResult;
use crate::deferred::types::DeferredId;
use crate::deferred::types::DeferredLookup;
use crate::deferred::types::DeferredTable;

/// The result of evaluating a bxl function
#[derive(Allocative)]
pub enum BxlResult {
    /// represents that the bxl function has no built results
    None {
        output_loc: BuckOutPath,
        error_loc: BuckOutPath,
    },
    /// a bxl that deals with builds
    BuildsArtifacts {
        output_loc: BuckOutPath,
        error_loc: BuckOutPath,
        built: Vec<BxlBuildResult>,
        artifacts: Vec<ArtifactGroup>,
        deferred: DeferredTable,
    },
}

impl BxlResult {
    pub fn new(
        output_loc: BuckOutPath,
        error_loc: BuckOutPath,
        ensured_artifacts: IndexSet<ArtifactGroup>,
        deferred: DeferredTable,
    ) -> Self {
        if ensured_artifacts.is_empty() {
            Self::None {
                output_loc,
                error_loc,
            }
        } else {
            Self::BuildsArtifacts {
                output_loc,
                error_loc,
                built: vec![],
                artifacts: ensured_artifacts.into_iter().collect(),
                deferred,
            }
        }
    }

    /// looks up an 'Deferred' given the id
    pub fn lookup_deferred(&self, id: DeferredId) -> anyhow::Result<DeferredLookup<'_>> {
        match self {
            BxlResult::None { .. } => Err(anyhow::anyhow!("Bxl never attempted to build anything")),
            BxlResult::BuildsArtifacts { deferred, .. } => deferred.lookup_deferred(id),
        }
    }

    pub fn get_output_loc(&self) -> &BuckOutPath {
        match self {
            BxlResult::None { output_loc, .. } => output_loc,
            BxlResult::BuildsArtifacts { output_loc, .. } => output_loc,
        }
    }

    pub fn get_error_loc(&self) -> &BuckOutPath {
        match self {
            BxlResult::None { error_loc, .. } => error_loc,
            BxlResult::BuildsArtifacts { error_loc, .. } => error_loc,
        }
    }
}
