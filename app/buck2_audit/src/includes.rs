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
use buck2_cli_proto::ClientContext;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::result::SharedResult;
use buck2_common::result::ToSharedResultExt;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::CellResolver;
use buck2_core::fs::fs_util;
use buck2_core::fs::paths::abs_norm_path::AbsNormPath;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::file_name::FileNameBuf;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::package::PackageLabel;
use buck2_interpreter::common::StarlarkModulePath;
use buck2_interpreter::file_loader::LoadedModule;
use buck2_interpreter_for_build::interpreter::calculation::InterpreterCalculation;
use buck2_node::nodes::eval_result::EvaluationResult;
use buck2_query::query::environment::LabeledNode;
use buck2_query::query::environment::NodeLabel;
use buck2_query::query::traversal::async_depth_first_postorder_traversal;
use buck2_query::query::traversal::AsyncNodeLookup;
use buck2_query::query::traversal::AsyncTraversalDelegate;
use buck2_query::query::traversal::ChildVisitor;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::ctx::ServerCommandDiceContext;
use derive_more::Display;
use dice::DiceComputations;
use dupe::Dupe;
use futures::stream::FuturesOrdered;
use futures::StreamExt;
use gazebo::prelude::*;
use indexmap::indexmap;
use itertools::Itertools;
use ref_cast::RefCast;
use serde::ser::SerializeMap;
use serde::Serialize;
use serde::Serializer;
use thiserror::Error;

use crate::AuditCommandCommonOptions;
use crate::AuditSubcommand;

#[derive(Debug, Error)]
enum AuditIncludesError {
    #[error("When loading buildfile for `{0}` found a mismatched buildfile name (`{1}`)")]
    WrongBuildfilePath(CellPath, FileNameBuf),
    #[error("invalid buildfile path `{0}`")]
    InvalidPath(CellPath),
}

#[derive(Debug, clap::Parser, serde::Serialize, serde::Deserialize)]
#[clap(
    name = "audit-includes",
    about = "list build file extensions imported at parse time."
)]
pub struct AuditIncludesCommand {
    #[clap(flatten)]
    common_opts: AuditCommandCommonOptions,

    /// Print json representation of outputs
    #[clap(long)]
    json: bool,

    #[clap(
        name = "BUILD_FILES",
        help = "Build files to audit. These are expected to be relative paths from the working dir cell."
    )]
    patterns: Vec<String>,
}

async fn get_transitive_includes(
    ctx: &DiceComputations,
    load_result: &EvaluationResult,
) -> anyhow::Result<Vec<ImportPath>> {
    // We define a simple graph of LoadedModules to traverse.
    #[derive(Clone, Dupe)]
    struct Node(LoadedModule);

    impl Node {
        fn import_path(&self) -> &ImportPath {
            self.0
                .path()
                .unpack_load_file()
                .expect("only visit imports so only bzl files are expected")
        }
    }

    #[derive(Display, Debug, Hash, Eq, PartialEq, Clone, RefCast)]
    #[repr(transparent)]
    struct NodeRef(ImportPath);

    impl NodeLabel for NodeRef {}

    impl LabeledNode for Node {
        type NodeRef = NodeRef;

        fn node_ref(&self) -> &NodeRef {
            NodeRef::ref_cast(self.import_path())
        }
    }

    struct Lookup<'a> {
        ctx: &'a DiceComputations,
    }

    #[async_trait]
    impl AsyncNodeLookup<Node> for Lookup<'_> {
        async fn get(&self, label: &NodeRef) -> anyhow::Result<Node> {
            Ok(Node(
                self.ctx
                    .get_loaded_module(StarlarkModulePath::LoadFile(&label.0))
                    .await?,
            ))
        }
    }

    struct Delegate {
        imports: Vec<ImportPath>,
    }

    #[async_trait]
    impl AsyncTraversalDelegate<Node> for Delegate {
        fn visit(&mut self, target: Node) -> anyhow::Result<()> {
            self.imports.push(target.import_path().clone());
            Ok(())
        }

        async fn for_each_child(
            &mut self,
            target: &Node,
            func: &mut dyn ChildVisitor<Node>,
        ) -> anyhow::Result<()> {
            for import in target.0.imports() {
                func.visit(NodeRef(import.clone()))?;
            }
            Ok(())
        }
    }

    let mut delegate = Delegate { imports: vec![] };
    let lookup = Lookup { ctx };

    async_depth_first_postorder_traversal(
        &lookup,
        load_result.imports().map(NodeRef::ref_cast),
        &mut delegate,
    )
    .await?;
    Ok(delegate.imports)
}

async fn load_and_collect_includes(
    ctx: &DiceComputations,
    path: &CellPath,
) -> SharedResult<Vec<ImportPath>> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!(AuditIncludesError::InvalidPath(path.clone())))?;
    let package = PackageLabel::new(parent.cell(), parent.path());
    let load_result = ctx.get_interpreter_results(package).await?;

    let buildfile_name = load_result.buildfile_path().filename();
    if buildfile_name
        != path
            .path()
            .file_name()
            .expect("checked that this has a parent above")
    {
        return Err(anyhow::anyhow!(AuditIncludesError::WrongBuildfilePath(
            path.clone(),
            buildfile_name.to_owned(),
        )))
        .shared_error();
    }

    Ok(get_transitive_includes(ctx, &load_result).await?)
}

fn resolve_path(
    cells: &CellResolver,
    fs: &ProjectRoot,
    current_cell_abs_path: &AbsNormPath,
    path: &str,
) -> anyhow::Result<CellPath> {
    // To match buck1, if the path is absolute we use it as-is, but if not it is treated
    // as relative to the working dir cell root (not the working dir).
    // The easiest way to consistently handle non-canonical paths
    // is to just resolve to absolute here, and then relativize.
    //
    // Note if the path is already absolute, this operation is a no-op.
    let path = current_cell_abs_path.as_abs_path().join(path);

    let abs_path = fs_util::canonicalize(&path)?;

    let project_path = fs.relativize(&abs_path)?;
    cells.get_cell_path(&project_path)
}

#[async_trait]
impl AuditSubcommand for AuditIncludesCommand {
    async fn server_execute(
        &self,
        server_ctx: Box<dyn ServerCommandContextTrait>,
        _client_ctx: ClientContext,
    ) -> anyhow::Result<()> {
        server_ctx
            .with_dice_ctx(async move |server_ctx, ctx| {
                let cells = ctx.get_cell_resolver().await?;
                let cwd = server_ctx.working_dir();
                let current_cell = cells.get(cells.find(cwd)?)?;
                let fs = server_ctx.project_root();
                let current_cell_abs_path =
                    fs.resolve(current_cell.path().as_project_relative_path());

                let futures: FuturesOrdered<_> = self
                    .patterns
                    .iter()
                    .unique()
                    .map(|path| {
                        let path = path.to_owned();
                        let ctx = ctx.dupe();
                        let cell_path = resolve_path(&cells, fs, &current_cell_abs_path, &path);
                        async move {
                            let load_result = try {
                                let cell_path = cell_path?;
                                load_and_collect_includes(&ctx, &cell_path).await?
                            };
                            (path, load_result)
                        }
                    })
                    .collect();

                let results: Vec<(_, SharedResult<Vec<_>>)> = futures.collect().await;
                // This is expected to not return any errors, and so we're not careful about not propagating it.
                let to_absolute_path = move |include: ImportPath| -> anyhow::Result<_> {
                    let include = include.path();
                    let cell = cells.get(include.cell())?;
                    let path = cell.path().join(include.path());
                    Ok(fs.resolve(&path))
                };
                let absolutize_paths =
                    |paths: Vec<ImportPath>| -> SharedResult<Vec<AbsNormPathBuf>> {
                        Ok(paths.into_try_map(&to_absolute_path)?)
                    };
                let results: Vec<(String, SharedResult<Vec<AbsNormPathBuf>>)> = results
                    .into_map(|(path, includes)| (path, includes.and_then(absolutize_paths)));

                let mut stdout = server_ctx.stdout()?;

                // For the printing of results, we don't need to propagate errors, just print
                // them. After we print the results, we'll propagate an error if there is one.
                if self.json {
                    let mut ser = serde_json::Serializer::pretty(&mut stdout);
                    // buck1 has a bug where it doesn't properly handle >1 arg when passed --json
                    // it also, sadly, prints just a single list of outputs for that case. we match
                    // buck1's behavior for 1 successful file and print a dictionary for multiple. This is
                    // unfortunate, but we hope that users can migrate to the equivalent query commands instead.
                    if let Some((_path, Ok(includes))) = results.as_singleton() {
                        includes.serialize(&mut ser)?
                    } else {
                        let mut map = ser.serialize_map(Some(results.len()))?;
                        for (path, includes) in &results {
                            match includes {
                                Ok(includes) => {
                                    map.serialize_entry(path, &indexmap! {"includes" => &includes})?
                                }
                                Err(e) => map.serialize_entry(
                                    path,
                                    &indexmap! {"$error" => format!("{:#}", e)},
                                )?,
                            }
                        }
                        map.end()?;
                    }

                    // flush a newline after serde output.
                    writeln!(stdout)?;
                } else {
                    for (path, includes) in &results {
                        match includes {
                            Ok(includes) => {
                                // intentionally add a blank line after the header
                                writeln!(stdout, "# {}\n", path)?;
                                for include in includes {
                                    // To match buck1, we print absolute paths.
                                    writeln!(stdout, "{}", include)?;
                                }
                            }
                            Err(e) => {
                                // intentionally add a blank line after the header
                                writeln!(stdout, "! {}\n", path)?;
                                writeln!(stdout, "{:#}", e)?;
                            }
                        }
                    }
                }

                // propagate the first error.
                for (_, result) in results {
                    result?;
                }

                Ok(())
            })
            .await
    }

    fn common_opts(&self) -> &AuditCommandCommonOptions {
        &self.common_opts
    }
}
