/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::time::Duration;
use std::time::SystemTime;

use buck2_common::executor_config::RemoteExecutorUseCase;
use remote_execution::ActionResultResponse;
use remote_execution::ExecuteResponse;
use remote_execution::TDirectory2;
use remote_execution::TExecutedActionMetadata;
use remote_execution::TFile;
use remote_execution::TTimestamp;

use crate::digest_config::DigestConfig;
use crate::execute::action_digest::ActionDigest;
use crate::execute::kind::CommandExecutionKind;
use crate::execute::result::CommandExecutionTimingData;
use crate::re::manager::ManagedRemoteExecutionClient;
use crate::re::streams::RemoteCommandStdStreams;

pub trait RemoteActionResult: Send + Sync {
    fn output_files(&self) -> &[TFile];
    fn output_directories(&self) -> &[TDirectory2];

    fn execution_kind(&self, digest: ActionDigest) -> CommandExecutionKind;

    fn timing(&self) -> CommandExecutionTimingData;

    fn std_streams(
        &self,
        client: &ManagedRemoteExecutionClient,
        use_case: RemoteExecutorUseCase,
        digest_config: DigestConfig,
    ) -> RemoteCommandStdStreams;

    /// The TTL given by RE for the outputs for this action.
    fn ttl(&self) -> i64;
}

impl RemoteActionResult for ExecuteResponse {
    fn output_files(&self) -> &[TFile] {
        &self.action_result.output_files
    }

    fn output_directories(&self) -> &[TDirectory2] {
        &self.action_result.output_directories
    }

    fn execution_kind(&self, digest: ActionDigest) -> CommandExecutionKind {
        CommandExecutionKind::Remote { digest }
    }

    fn timing(&self) -> CommandExecutionTimingData {
        timing_from_re_metadata(&self.action_result.execution_metadata)
    }

    fn std_streams(
        &self,
        client: &ManagedRemoteExecutionClient,
        use_case: RemoteExecutorUseCase,
        digest_config: DigestConfig,
    ) -> RemoteCommandStdStreams {
        RemoteCommandStdStreams::new(&self.action_result, client, use_case, digest_config)
    }

    fn ttl(&self) -> i64 {
        self.action_result_ttl
    }
}

impl RemoteActionResult for ActionResultResponse {
    fn output_files(&self) -> &[TFile] {
        &self.action_result.output_files
    }

    fn output_directories(&self) -> &[TDirectory2] {
        &self.action_result.output_directories
    }

    fn execution_kind(&self, digest: ActionDigest) -> CommandExecutionKind {
        CommandExecutionKind::ActionCache { digest }
    }

    fn timing(&self) -> CommandExecutionTimingData {
        let mut timing = timing_from_re_metadata(&self.action_result.execution_metadata);
        timing.wall_time = Duration::ZERO; // This was a cache hit so we didn't wait.
        timing
    }

    fn std_streams(
        &self,
        client: &ManagedRemoteExecutionClient,
        use_case: RemoteExecutorUseCase,
        digest_config: DigestConfig,
    ) -> RemoteCommandStdStreams {
        RemoteCommandStdStreams::new(&self.action_result, client, use_case, digest_config)
    }

    fn ttl(&self) -> i64 {
        self.ttl
    }
}

fn timing_from_re_metadata(meta: &TExecutedActionMetadata) -> CommandExecutionTimingData {
    let execution_time = meta
        .execution_completed_timestamp
        .saturating_duration_since(&meta.execution_start_timestamp);

    let re_queue_time = meta
        .last_queued_timestamp
        .saturating_duration_since(&meta.queued_timestamp);

    let start_time = SystemTime::UNIX_EPOCH
        + meta
            .execution_start_timestamp
            .saturating_duration_since(&TTimestamp::unix_epoch());

    CommandExecutionTimingData {
        wall_time: execution_time,
        re_queue_time: Some(re_queue_time),
        execution_time,
        start_time,
    }
}
