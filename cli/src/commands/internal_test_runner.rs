/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_client_ctx::client_ctx::ClientCommandContext;
use clap::Parser;
use tokio::runtime::Runtime;

#[derive(Debug, Parser)]
#[clap(about = "run the internal test runner")]
pub(crate) struct InternalTestRunnerCommand {
    #[cfg(unix)]
    #[clap(flatten)]
    unix_runner: buck2_test_runner::unix::Buck2TestRunnerUnix,

    #[cfg(not(unix))]
    #[clap(flatten)]
    tcp_runner: buck2_test_runner::tcp::Buck2TestRunnerTcp,
}

impl InternalTestRunnerCommand {
    pub(crate) fn exec(
        self,
        _matches: &clap::ArgMatches,
        _ctx: ClientCommandContext,
    ) -> anyhow::Result<()> {
        let runtime = Runtime::new().expect("Failed to create Tokio runtime");
        runtime.block_on(async move {
            #[cfg(unix)]
            {
                self.unix_runner.run().await
            }
            #[cfg(not(unix))]
            {
                self.tcp_runner.run().await
            }
        })
    }
}
