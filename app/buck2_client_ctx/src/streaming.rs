/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use async_trait::async_trait;
use futures::future;
use futures::future::Either;
use superconsole::Component;

use crate::client_ctx::ClientCommandContext;
use crate::common::CommonBuildConfigurationOptions;
use crate::common::CommonConsoleOptions;
use crate::common::CommonDaemonCommandOptions;
use crate::daemon::client::connect::BuckdConnectOptions;
use crate::daemon::client::BuckdClientConnector;
use crate::exit_result::ExitResult;
use crate::exit_result::FailureExitCode;
use crate::subscribers::get::get_console_with_root;
use crate::subscribers::get::try_get_build_id_writer;
use crate::subscribers::get::try_get_event_log_subscriber;
use crate::subscribers::get::try_get_re_log_subscriber;
use crate::subscribers::recorder::try_get_invocation_recorder;
use crate::subscribers::subscriber::EventSubscriber;
use crate::subscribers::superconsole::StatefulSuperConsole;
use crate::LSP_COMMAND_NAME;

fn default_subscribers<T: StreamingCommand>(
    cmd: &T,
    ctx: &ClientCommandContext,
) -> anyhow::Result<Vec<Box<dyn EventSubscriber>>> {
    let console_opts = cmd.console_opts();
    let mut subscribers = vec![];
    let root =
        StatefulSuperConsole::default_layout(T::COMMAND_NAME, cmd.extra_superconsole_component());

    // If we're running the LSP, do not show "Waiting for daemon..." if we do not get any spans.
    let show_waiting_message = T::COMMAND_NAME != LSP_COMMAND_NAME;

    if let Some(v) = get_console_with_root(
        console_opts.console_type,
        ctx.verbosity,
        show_waiting_message,
        ctx.replay_speed,
        root,
        console_opts.superconsole_config(),
        ctx.paths()?.isolation.clone(),
    )? {
        subscribers.push(v)
    }
    if let Some(event_log) =
        try_get_event_log_subscriber(cmd.event_log_opts(), cmd.sanitized_argv(), ctx)?
    {
        subscribers.push(event_log)
    }
    if let Some(re_log) = try_get_re_log_subscriber(ctx)? {
        subscribers.push(re_log)
    }
    if let Some(build_id_writer) = try_get_build_id_writer(cmd.event_log_opts(), ctx)? {
        subscribers.push(build_id_writer)
    }
    if let Some(recorder) = try_get_invocation_recorder(ctx, T::COMMAND_NAME, cmd.sanitized_argv())?
    {
        subscribers.push(recorder);
    }
    Ok(subscribers)
}

/// Trait to generalize the behavior of executable buck2 commands that rely on a server.
/// This trait is most helpful when the command wants a superconsole, to stream events, etc.
/// However, this is the most robustly tested of our code paths, and there is little cost to defaulting to it.
/// As a result, prefer to default to streaming mode unless there is a compelling reason not to
/// (e.g `status`)
#[async_trait]
pub trait StreamingCommand: Sized + Send + Sync {
    /// Give the command a name for printing, debugging, etc.
    const COMMAND_NAME: &'static str;

    /// Run the command.
    async fn exec_impl(
        self,
        buckd: BuckdClientConnector,
        matches: &clap::ArgMatches,
        ctx: ClientCommandContext,
    ) -> ExitResult;

    /// Should we only connect to existing servers (`true`), or spawn a new server if required (`false`).
    /// Defaults to `false`.
    fn existing_only() -> bool {
        false
    }

    fn console_opts(&self) -> &CommonConsoleOptions;

    fn event_log_opts(&self) -> &CommonDaemonCommandOptions;

    fn common_opts(&self) -> &CommonBuildConfigurationOptions;

    /// Allows a command to add additional superconsole components when superconsole is used.
    fn extra_superconsole_component(&self) -> Option<Box<dyn Component>> {
        None
    }

    fn sanitized_argv(&self) -> Vec<String> {
        std::env::args().collect()
    }
}

/// Just provides a common interface for buck subcommands for us to interact with here.
pub trait BuckSubcommand {
    fn exec(self, matches: &clap::ArgMatches, ctx: ClientCommandContext) -> ExitResult;
}

impl<T: StreamingCommand> BuckSubcommand for T {
    /// Actual call that runs a `StreamingCommand`.
    /// Handles all of the business of setting up a runtime, server, and subscribers.
    fn exec(self, matches: &clap::ArgMatches, ctx: ClientCommandContext) -> ExitResult {
        ctx.with_runtime(async move |mut ctx| {
            let work = async {
                let mut connect_options = BuckdConnectOptions {
                    existing_only: T::existing_only(),
                    subscribers: default_subscribers(&self, &ctx)?,
                };

                let buckd = match (ctx.replayer.take(), ctx.start_in_process_daemon.take()) {
                    (Some(replayer), _) => {
                        connect_options.replay(replayer.into_inner(), ctx.paths()?)?
                    }
                    (None, None) => ctx.connect_buckd(connect_options).await?,
                    (None, Some(start_in_process_daemon)) => {
                        // Start in-process daemon, wait until it is ready to accept connections.
                        start_in_process_daemon()?;

                        // Do not attempt to spawn a daemon if connect failed.
                        // Connect should not fail.
                        connect_options.existing_only = true;

                        ctx.connect_buckd(connect_options).await?
                    }
                };

                self.exec_impl(buckd, matches, ctx).await
            };

            // Race our work with a ctrl+c future. If we hit ctrl+c, then we'll drop the work
            // future. with_runtime sets up an AsyncCleanupContext that will allow drop
            // implementations within this future to clean up before we return from with_runtime.
            let exit = tokio::signal::ctrl_c();

            futures::pin_mut!(work);
            futures::pin_mut!(exit);

            match future::select(work, exit).await {
                Either::Left((res, _)) => res,
                Either::Right((_signal, _)) => ExitResult::from(FailureExitCode::SignalInterrupt),
            }
        })
    }
}
