/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::io::Write;

use async_trait::async_trait;
use buck2_build_api::calculation::load_patterns;
use buck2_build_api::calculation::Calculation;
use buck2_cli_proto::ClientContext;
use buck2_core::configuration::data::ConfigurationData;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::ctx::ServerCommandDiceContext;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::pattern::target_platform_from_client_context;
use buck2_server_ctx::pattern::PatternParser;
use indent_write::io::IndentWriter;

use crate::AuditCommandCommonOptions;
use crate::AuditSubcommand;

#[derive(Debug, clap::Parser, serde::Serialize, serde::Deserialize)]
#[clap(
    name = "audit-execution-platform-resolution",
    about = "prints out information about execution platform resolution"
)]
pub struct AuditExecutionPlatformResolutionCommand {
    #[clap(flatten)]
    common_opts: AuditCommandCommonOptions,

    #[clap(name = "TARGET_PATTERNS", help = "Patterns to analyze")]
    patterns: Vec<String>,
}

#[async_trait]
impl AuditSubcommand for AuditExecutionPlatformResolutionCommand {
    async fn server_execute(
        &self,
        server_ctx: Box<dyn ServerCommandContextTrait>,
        mut stdout: PartialResultDispatcher<buck2_cli_proto::StdoutBytes>,
        client_ctx: ClientContext,
    ) -> anyhow::Result<()> {
        server_ctx.with_dice_ctx(
            async move |server_ctx, ctx| {
                let pattern_parser = PatternParser::new(
                    &ctx,
                    server_ctx.working_dir(),
                ).await?;

                let mut configured_patterns = Vec::new();
                let mut target_patterns = Vec::new();
                for pat in self.patterns.iter() {
                    if let Some((target, cfg_str)) = pat.split_once(' ') {
                        let cfg_str = match cfg_str.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
                            Some(s) => s,
                            None => {
                                return Err(anyhow::anyhow!(
                                    "Expected a configuration of the form `(//a:config-<hash>)`, but didn't see surrounding parens"
                                ));
                            }
                        };
                        let cfg = ConfigurationData::lookup_from_string(cfg_str)?;
                        let target = pattern_parser
                            .parse_pattern::<TargetPatternExtra>(target)?
                            .as_target_label(target)?;
                        configured_patterns.push(target.configure(cfg));
                    } else {
                        target_patterns.push(pattern_parser.parse_pattern::<TargetPatternExtra>(pat)?);
                    }
                }

                let loaded_patterns = load_patterns(&ctx, target_patterns).await?;
                let target_platform = target_platform_from_client_context(
                    &client_ctx,
                    server_ctx,
                    &ctx,
                )
                .await?;

                for (_, targets) in loaded_patterns.into_iter() {
                    for (_, node) in targets? {
                        configured_patterns.push(
                            ctx.get_configured_target(node.label(), target_platform.as_ref())
                                .await?,
                        );
                    }
                }

                let mut stdout = stdout.as_writer();

                for configured_target in configured_patterns {
                    let configured_node = ctx.get_configured_target_node(&configured_target).await?;
                    let configured_node = configured_node.require_compatible()?;
                    writeln!(stdout, "{}:", configured_target)?;
                    let resolution = configured_node.execution_platform_resolution();
                    match resolution.platform() {
                        Ok(platform) => {
                            writeln!(stdout, "  Execution platform: {}", platform.id())?;
                            writeln!(stdout, "    Execution platform configuration: {}", platform.cfg())?;
                            writeln!(stdout, "    Execution deps:")?;
                            for execution_dep in configured_node.exec_deps() {
                                writeln!(stdout, "      {}", execution_dep.label())?;
                            }
                            writeln!(stdout, "    Toolchain deps:")?;
                            for toolchain_dep in configured_node.toolchain_deps() {
                                writeln!(stdout, "      {}", toolchain_dep.label())?;
                            }
                            for (label, reason) in resolution.skipped() {
                                writeln!(stdout, "    Skipped {}", label)?;
                                writeln!(IndentWriter::new("      ", &mut stdout), "{:#}", reason)?;
                            }
                        }
                        Err(e) => writeln!(stdout, "{}", e)?,
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
