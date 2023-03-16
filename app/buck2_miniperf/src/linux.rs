/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::env;
use std::fs::File;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

use anyhow::Context as _;
use buck2_miniperf_proto::MiniperfCounter;
use buck2_miniperf_proto::MiniperfOutput;
use perf_event::events::Hardware;
use perf_event::Builder;

/// First argument is an output path to write output data into. The rest is the command to execute.
pub fn main() -> anyhow::Result<()> {
    let mut args = env::args_os();
    args.next().context("No argv0")?;

    // In an ideal world, we would like this to be a pipe. Unfortunately, we can't do that, because
    // to get a pipe here, we'd have to have it not be CLOEXEC (because we need to exec *this*
    // binary). However, we're spawned by a server that creates many such processes concurrently,
    // so that means CLOEXEC must be set when creating the pipe, then unset between fork and exec.
    // To do this while retaining posix_spawn (which is quite a bit faster than fork + exec), we
    // need to dup the FD (which clears CLOEXEC), but the Rust wrapper around posix_spawn
    // (`Command`) does not expose that.
    let out = args.next().context("No output path")?;

    // NOTE: Kernel is not enabled here: we want to report only userspace cycles.
    let mut user_counter = Builder::new()
        .kind(Hardware::INSTRUCTIONS)
        .inherit(true)
        .enable_on_exec()
        .build()?;

    let status = args.next().context("No process to run").and_then(|bin| {
        Command::new(bin)
            .args(args)
            .status()
            .map_err(anyhow::Error::from)
    });

    let value = user_counter
        .read_count_and_time()
        .context("Error reading user_counter")?;

    let output = MiniperfOutput {
        raw_exit_code: status.map(|s| s.into_raw()).map_err(|e| e.to_string()),
        user_instructions: MiniperfCounter {
            count: value.count,
            time_enabled: value.time_enabled,
            time_running: value.time_running,
        },
    };

    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&out)
        .with_context(|| format!("Failed to open `{:?}`", out))?;

    bincode::serialize_into(&mut file, &output)
        .with_context(|| format!("Failed to write to `{:?}`", out))?;

    file.flush()
        .with_context(|| format!("Failed to flush to `{:?}`", out))?;

    Ok(())
}