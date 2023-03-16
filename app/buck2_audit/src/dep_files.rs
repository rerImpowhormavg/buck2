/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::borrow::Cow;
use std::io::Write;

use anyhow::Context as _;
use async_trait::async_trait;
use buck2_build_api::actions::impls::dep_files::get_dep_files;
use buck2_build_api::actions::impls::dep_files::DepFilesKey;
use buck2_build_api::actions::impls::dep_files::StoredFingerprints;
use buck2_build_api::calculation::Calculation;
use buck2_build_api::deferred::base_deferred_key::BaseDeferredKey;
use buck2_cli_proto::ClientContext;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_core::category::Category;
use buck2_core::directory::Directory;
use buck2_core::directory::DirectoryIterator;
use buck2_core::target::name::TargetName;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::ctx::ServerCommandDiceContext;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::pattern::parse_patterns_from_cli_args;
use buck2_server_ctx::pattern::target_platform_from_client_context;

use crate::AuditCommandCommonOptions;
use crate::AuditSubcommand;

#[derive(Debug, clap::Parser, serde::Serialize, serde::Deserialize)]
#[clap(
    name = "audit-dep-files",
    about = "prints out the select files for a command"
)]
pub struct AuditDepFilesCommand {
    #[clap(flatten)]
    common_opts: AuditCommandCommonOptions,

    #[clap(help = "Target to query dep files for")]
    pattern: String,

    #[clap(help = "Action category")]
    category: String,

    #[clap(help = "Action identifier")]
    identifier: Option<String>,
}

#[async_trait]
impl AuditSubcommand for AuditDepFilesCommand {
    async fn server_execute(
        &self,
        server_ctx: Box<dyn ServerCommandContextTrait>,
        mut stdout: PartialResultDispatcher<buck2_cli_proto::StdoutBytes>,
        client_ctx: ClientContext,
    ) -> anyhow::Result<()> {
        server_ctx
            .with_dice_ctx(async move |server_ctx, ctx| {
                let cells = ctx.get_cell_resolver().await?;

                let target_platform = target_platform_from_client_context(
                    Some(&client_ctx),
                    &cells,
                    server_ctx.working_dir(),
                )
                .await?;

                let label = parse_patterns_from_cli_args::<TargetName>(
                    &[buck2_data::TargetPattern {
                        value: self.pattern.clone(),
                    }],
                    &cells,
                    &ctx.get_legacy_configs().await?,
                    server_ctx.working_dir(),
                )?
                .into_iter()
                .next()
                .context("Parsing patterns returned nothing")?
                .as_target_label(&self.pattern)?;

                let label = ctx
                    .get_configured_target(&label, target_platform.as_ref())
                    .await?;

                let category = Category::try_from(self.category.as_str())?;

                let key = DepFilesKey::new(
                    BaseDeferredKey::TargetLabel(label),
                    category,
                    self.identifier.clone(),
                );

                let state = get_dep_files(&key).context("Failed to find dep files")?;

                let dep_files = state
                    .read_dep_files(
                        &ctx.get_artifact_fs().await?,
                        ctx.per_transaction_data().get_materializer().as_ref(),
                    )
                    .await
                    .context("Failed to read dep files")?
                    .context("Dep fils have expired")?;

                let fingerprints = state.locked_compute_fingerprints(
                    Cow::Owned(dep_files),
                    true,
                    ctx.global_data().get_digest_config(),
                );

                let dirs = match &*fingerprints {
                    StoredFingerprints::Digests(..) => {
                        // This is bit awkward but this only for testing right now so that's OK
                        return Err(anyhow::anyhow!("Fingerprints were stored as digests!"));
                    }
                    StoredFingerprints::Dirs(dirs) => dirs,
                };

                let mut stdout = stdout.as_writer();

                for (path, ..) in dirs
                    .untagged
                    .ordered_walk()
                    .with_paths()
                    .filter_map(|(p, e)| Some((p, e.into_leaf()?)))
                {
                    writeln!(stdout, "untagged\t{}", path)?;
                }

                for (tag, dir) in dirs.tagged.iter() {
                    for (path, ..) in dir
                        .ordered_walk()
                        .with_paths()
                        .filter_map(|(p, e)| Some((p, e.into_leaf()?)))
                    {
                        writeln!(stdout, "{}\t{}", tag, path)?;
                    }
                }

                Ok(())
            })
            .await
    }

    fn common_opts(&self) -> &AuditCommandCommonOptions {
        &self.common_opts
    }
}
