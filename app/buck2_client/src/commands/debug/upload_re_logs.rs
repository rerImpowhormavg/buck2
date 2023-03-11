/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::process::Stdio;

use anyhow::Context;
use async_compression::tokio::bufread::ZstdEncoder;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::manifold;
use buck2_core::fs::async_fs_util;
use buck2_core::fs::paths::forward_rel_path::ForwardRelativePath;
use tokio::io::AsyncRead;
use tokio::io::BufReader;

#[derive(Debug, clap::Parser)]
#[clap(about = "upload RE logs")]
pub struct UploadReLogsCommand {
    #[clap(long)]
    session_id: String,
}

impl UploadReLogsCommand {
    pub fn exec(self, _matches: &clap::ArgMatches, ctx: ClientCommandContext) -> ExitResult {
        buck2_core::facebook_only();

        ctx.with_runtime(async move |ctx| {
            let re_logs_location = ctx.paths()?.re_logs_dir();
            let logs_path = re_logs_location
                .join(ForwardRelativePath::new(&self.session_id)?)
                .join(ForwardRelativePath::new("REClientFolly.log")?);
            let file = async_fs_util::open(&logs_path).await?;
            let mut encoder =
                ZstdEncoder::with_quality(BufReader::new(file), async_compression::Level::Default);

            self.upload_file(&mut encoder).await?
        })
    }

    async fn upload_file<'a, R>(&self, reader: &'a mut R) -> anyhow::Result<ExitResult>
    where
        R: AsyncRead + Unpin + ?Sized,
    {
        let bucket_path = &format!("{}.log.zst", self.session_id);

        let upload = manifold::upload_command(manifold::Bucket::ReLogs, bucket_path)?;

        // Do nothing if upload command could not be found
        match upload {
            None => Ok(ExitResult::success()),
            Some(mut upload) => {
                upload.stdin(Stdio::piped());

                // write compressed file to stdin
                let mut child = upload.spawn().context("Error spawning command")?;
                let mut stdin = child.stdin.take().context("Stdin was piped")?;
                tokio::io::copy(reader, &mut stdin)
                    .await
                    .context("Error writing to stdin")?;

                drop(stdin); // This tells the child process that there is no more data

                let exit_code = child.wait().await?.code();
                match exit_code {
                    None => Err(anyhow::anyhow!("No exit code returned")),
                    Some(code) => Ok(ExitResult::status_extended(code)),
                }
            }
        }
    }
}
