/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//!
//! # Cell
//! A 'Cell' is sub-project within the main project for Buck. All files
//! reachable by Buck is belongs to a single Cell.
//! Cells can be sub-directories of other cells, but that makes that
//! sub-directory part of the sub-cell and no longer part of the parent cell.
//! For example, let's say there's cells 'parent-cell' and 'sub-cell' declared
//! in folders of the same names.
//! ```text
//!  parent-cell
//! +-- folder1
//! +-- folder2
//! +-- sub-cell
//! |   +-- folder3
//! ```
//! All files part of `folder1` and `folder2` will be part of 'parent-cell'.
//! Anything part of `sub-cell`, including `folder3`, are only part of the
//! 'sub-cell'.
//!
//! For users, each Cell is identified by 'CellAlias's. A 'CellAlias' is a
//! human-readable name that contains alphanumeric characters and underscores.
//! (i.e. shouldn't contain any special characters like `/`). Something like `1`
//! is a valid identifier, though not we do not suggest such naming as it's not
//! very descriptive.
//!
//! It's possible that in certain cell contexts, some Cells are not reachable by
//! any 'CellAlias'. However, in the global context, every Cell will be
//! reachable by at least one 'CellAlias'.
//!
//! ## Cell Alias
//! The cell alias appears within a fully qualified target with the syntax
//! `<cell alias>//<target label>`. For example, in `foo//some:target`, `foo` is
//! the cell alias. Examples like `foo/bar//some:target` has an invalid cell
//! alias of `foo/bar` since special characters are forbidden.
//!
//! The 'CellAlias' is specified via configuration files per cell. A
//! configuration specifies these with the syntax `<cell alias>=<relative path
//! to cell>`. We allow a many to one mapping from 'CellAlias' to Cell.
//!
//! Each Cell may give different aliases to the same cell. The 'CellAlias' will
//! be resolved based on the contextual cell that the alias appears in.
//! e.g. `mycell//foo:bar` build file will have any aliases that appears within
//! it be resolved using the aliases defined in `mycell` cell.
//!
//! Cells may omit declaring aliases for cells that exists globally. This means
//! that there will be no alias for those cells, and hence render those cells
//! inaccessible from the cell context that doesn't declare them.
//!
//! ### The Empty Cell Alias
//! The empty cell alias is a special alias injected by Buck to represent the
//! current contextual cell. That means, inside `mycell` cell, references to the
//! 'CellAlias' `""` will resolve to the `mycell` cell.
//!
//! ## Cell Name
//! Each Cell is uniquely identifier globally via a one to one mapping to a
//! 'CellName'. A 'CellName' is a canonicalized, human-readable name that
//! corresponds to a 'CellInstance'. The cell name is inferred from the global
//! list of 'CellAlias's available, by picking the first alias for each cell
//! path based on lexicogrpahic ordering of the aliases. The 'CellName' is
//! subject to the same character restrictions as 'CellAlias'.
//!
//! # Resolving Cells
//! Cells are represented by 'CellInstance'. The 'CellResolver' is able to
//! resolve 'CellNames' to 'CellInstance's. It is also able to find the
//! containing Cell given a path. 'CellAlias' can be resolved with an
//! 'CellAliasResolver'. Each 'CellInstance' contains a 'CellAliasResolver' for
//! the cell alias mapping for that particular cell.
//!
//! e.g.
//! ```
//! use buck2_core::fs::project_rel_path::{ProjectRelativePath, ProjectRelativePathBuf};
//! use buck2_core::fs::paths::forward_rel_path::ForwardRelativePathBuf;
//! use buck2_core::cells::{CellResolver, CellAlias};
//! use std::convert::TryFrom;
//! use maplit::hashmap;
//! use buck2_core::cells::cell_root_path::CellRootPathBuf;
//! use buck2_core::cells::name::CellName;
//! use buck2_core::cells::testing::CellResolverExt;
//! use dupe::Dupe;
//!
//! let cell_config = ForwardRelativePathBuf::try_from(".buckconfig".to_owned())?;
//! let fbsource = ProjectRelativePath::new("")?;
//! let fbcode = ProjectRelativePath::new("fbcode")?;
//!
//! let cells = CellResolver::with_names_and_paths_with_alias(&[
//!     (CellName::testing_new("fbsource"), CellRootPathBuf::new(fbsource.to_buf()), hashmap![
//!         CellAlias::new("fbsource".to_owned()) => CellName::testing_new("fbsource"),
//!         CellAlias::new("fbcode".to_owned()) => CellName::testing_new("fbcode"),
//!     ]),
//!     (CellName::testing_new("fbcode"), CellRootPathBuf::new(fbcode.to_buf()), hashmap![
//!         CellAlias::new("fbcode".to_owned()) => CellName::testing_new("fbcode"),
//!         CellAlias::new("fbsource".to_owned()) => CellName::testing_new("fbsource"),
//!     ])
//! ]);
//!
//! let fbsource_cell_name = cells.find(ProjectRelativePath::new("something/in/fbsource")?)?.dupe();
//! assert_eq!(fbsource_cell_name, CellName::testing_new("fbsource"));
//!
//! let fbcode_cell_name = cells.find(ProjectRelativePath::new("fbcode/something/in/fbcode")?)?.dupe();
//! assert_eq!(fbcode_cell_name, CellName::testing_new("fbcode"));
//!
//! let fbsource_cell = cells.get(fbsource_cell_name)?;
//! assert_eq!(fbsource_cell.name(), CellName::testing_new("fbsource"));
//! let fbcode_cell = cells.get(fbcode_cell_name)?;
//! assert_eq!(fbcode_cell.name(), CellName::testing_new("fbcode"));
//!
//! let fbsource_aliases = fbsource_cell.cell_alias_resolver();
//! assert_eq!(fbsource_aliases.resolve("")?, CellName::testing_new("fbsource"));
//! assert_eq!(fbsource_aliases.resolve("fbsource")?, CellName::testing_new("fbsource"));
//! assert_eq!(fbsource_aliases.resolve("fbcode")?, CellName::testing_new("fbcode"));
//!
//! let fbcode_aliases = fbcode_cell.cell_alias_resolver();
//! assert_eq!(fbcode_aliases.resolve("")?, CellName::testing_new("fbcode"));
//! assert_eq!(fbcode_aliases.resolve("fbsource")?, CellName::testing_new("fbsource"));
//! assert_eq!(fbcode_aliases.resolve("fbcode")?, CellName::testing_new("fbcode"));
//!
//! # anyhow::Ok(())
//! ```
//!

pub mod build_file_cell;
pub mod cell_path;
pub mod cell_root_path;
pub mod name;
pub mod paths;
pub(crate) mod sequence_trie_allocative;

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Display;
use std::hash::Hash;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context;
use derivative::Derivative;
use derive_more::Display;
use dupe::Dupe;
use dupe::OptionDupedExt;
use gazebo::prelude::*;
use itertools::Itertools;
use sequence_trie::SequenceTrie;
use thiserror::Error;

use crate::cells::cell_path::CellPath;
use crate::cells::cell_path::CellPathRef;
use crate::cells::cell_root_path::CellRootPath;
use crate::cells::cell_root_path::CellRootPathBuf;
use crate::cells::name::CellName;
use crate::fs::paths::abs_norm_path::AbsNormPath;
use crate::fs::paths::abs_norm_path::AbsNormPathBuf;
use crate::fs::paths::abs_path::AbsPath;
use crate::fs::paths::file_name::FileNameBuf;
use crate::fs::project::ProjectRoot;
use crate::fs::project_rel_path::ProjectRelativePath;
use crate::fs::project_rel_path::ProjectRelativePathBuf;
use crate::package::PackageLabel;

/// Errors from cell creation
#[derive(Error, Debug)]
enum CellError {
    #[error("Cell paths `{1}` and `{2}` had the same alias `{0}`.")]
    DuplicateAliases(CellAlias, CellRootPathBuf, CellRootPathBuf),
    #[error("Cell paths `{1}` and `{2}` had the same cell name `{0}`.")]
    DuplicateNames(CellName, CellRootPathBuf, CellRootPathBuf),
    #[error("cannot find the cell at current path `{0}`. Known roots are `<{}>`", .1.join(", "))]
    UnknownCellPath(ProjectRelativePathBuf, Vec<String>),
    #[error("unknown cell alias: `{0}`. In cell `{1}`, known aliases are: `{}`", .2.iter().join(", "))]
    UnknownCellAlias(CellAlias, CellName, Vec<CellAlias>),
    #[error("cell aliases cannot contain empty alias")]
    EmptyAlias,
    #[error("unknown cell name: `{0}`. known cell names are `{}`", .1.iter().join(", "))]
    UnknownCellName(CellName, Vec<CellName>),
    #[error(
        "Cell name `{0}` should be an alias for an existing cell, but `{1}` isn't a known alias"
    )]
    AliasOnlyCell(CellAlias, CellAlias),
}

/// A 'CellInstance', contains a 'CellName' and a path for that cell.
#[derive(Clone, Debug, Display, Dupe, PartialEq, Eq, Allocative)]
#[display(fmt = "{}", "_0.name")]
pub struct CellInstance(Arc<CellData>);

/// A 'CellAlias' is a user-provided string name that maps to a 'CellName'.
/// The mapping of 'CellAlias' to 'CellName' is specific to the current cell so
/// that the same 'CellAlias' may map to different 'CellName's depending on what
/// the current 'CellInstance' is that references the 'CellAlias'.
#[derive(
    Clone, Debug, Display, Hash, Eq, PartialEq, Ord, PartialOrd, Allocative
)]
pub struct CellAlias(String);

impl CellAlias {
    pub fn new(alias: String) -> CellAlias {
        CellAlias(alias)
    }
}

impl Borrow<str> for CellAlias {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// A 'CellAliasResolver' is unique to a 'CellInstance'.
/// It is responsible for resolving all 'CellAlias' encountered within the
/// 'CellInstance' into the global canonical 'CellName's
#[derive(Clone, Dupe, Debug, PartialEq, Eq, Allocative)]
pub struct CellAliasResolver {
    /// Current cell name.
    current: CellName,
    aliases: Arc<HashMap<CellAlias, CellName>>,
}

impl CellAliasResolver {
    /// Create an instance of `CellAliasResolver`. The special alias `""` must be present, or
    /// this will fail
    pub fn new(
        current: CellName,
        aliases: Arc<HashMap<CellAlias, CellName>>,
    ) -> anyhow::Result<CellAliasResolver> {
        if aliases.contains_key("") {
            Err(anyhow::Error::new(CellError::EmptyAlias))
        } else {
            Ok(CellAliasResolver { current, aliases })
        }
    }

    /// resolves a 'CellAlias' into its corresponding 'CellName'
    pub fn resolve<T: ?Sized>(&self, alias: &T) -> anyhow::Result<CellName>
    where
        CellAlias: Borrow<T>,
        T: Hash + Eq + Display,
    {
        if alias == CellAlias::new("".to_owned()).borrow() {
            return Ok(self.current);
        }
        self.aliases.get(alias).duped().ok_or_else(|| {
            anyhow::Error::new(CellError::UnknownCellAlias(
                CellAlias::new(alias.to_string()),
                self.current,
                self.aliases.keys().cloned().collect(),
            ))
        })
    }

    /// finds the 'CellName' for the current cell (with the alias `""`. See module docs)
    pub fn resolve_self(&self) -> CellName {
        self.resolve("").expect("The alias \"\" to be valid")
    }

    pub fn mappings(&self) -> impl Iterator<Item = (&CellAlias, CellName)> {
        self.aliases.iter().map(|(alias, name)| (alias, *name))
    }
}

#[derive(Derivative, PartialEq, Eq, Allocative)]
#[derivative(Debug)]
struct CellData {
    /// the fully canonicalized 'CellName'
    name: CellName,
    /// the project relative path to this 'CellInstance'
    path: CellRootPathBuf,
    /// a list of potential buildfile names for this cell (e.g. 'BUCK', 'TARGETS',
    /// 'TARGET.v2'). The candidates are listed in priority order, buck will use
    /// the first one it encounters in a directory.
    buildfiles: Vec<FileNameBuf>,
    #[derivative(Debug = "ignore")]
    /// the aliases of this specific cell
    aliases: CellAliasResolver,
}

impl CellInstance {
    fn new(
        name: CellName,
        path: CellRootPathBuf,
        buildfiles: Vec<FileNameBuf>,
        aliases: CellAliasResolver,
    ) -> CellInstance {
        CellInstance(Arc::new(CellData {
            name,
            path,
            buildfiles,
            aliases,
        }))
    }

    /// Get the name of the cell, as supplied in `cell_name//foo:bar`.
    #[inline]
    pub fn name(&self) -> CellName {
        self.0.name.dupe()
    }

    /// Get the path of the cell, where it is routed.
    #[inline]
    pub fn path(&self) -> &CellRootPath {
        &self.0.path
    }

    // Get the name of build files for the cell.
    #[inline]
    pub fn buildfiles(&self) -> &[FileNameBuf] {
        &self.0.buildfiles
    }

    #[inline]
    pub fn cell_alias_resolver(&self) -> &CellAliasResolver {
        &self.0.aliases
    }
}

/// Resolves 'CellName's into 'CellInstance's.
// TODO(bobyf) we need to check if cells changed
#[derive(Clone, Dupe, PartialEq, Eq, Debug, Allocative)]
pub struct CellResolver(Arc<CellResolverInternals>);

#[derive(PartialEq, Eq, Debug, Allocative)]
struct CellResolverInternals {
    cells: HashMap<CellName, CellInstance>,
    #[allocative(visit = crate::cells::sequence_trie_allocative::visit_sequence_trie)]
    path_mappings: SequenceTrie<FileNameBuf, CellName>,
}

impl CellResolver {
    // Make this public till we start parsing config files from cells
    pub fn new(
        cells: HashMap<CellName, CellInstance>,
        path_mappings: SequenceTrie<FileNameBuf, CellName>,
    ) -> CellResolver {
        CellResolver(Arc::new(CellResolverInternals {
            cells,
            path_mappings,
        }))
    }

    pub fn contains(&self, cell: CellName) -> bool {
        self.0.cells.contains_key(&cell)
    }

    /// Get a `Cell` from the `CellMap`
    pub fn get(&self, cell: CellName) -> anyhow::Result<&CellInstance> {
        self.0.cells.get(&cell).ok_or_else(|| {
            anyhow::Error::new(CellError::UnknownCellName(
                cell,
                self.0.cells.keys().copied().collect(),
            ))
        })
    }

    pub fn root_cell(&self) -> CellName {
        self.find(ProjectRelativePath::new("").unwrap())
            .expect("Should have had a cell at the project root.")
    }

    pub fn root_cell_instance(&self) -> &CellInstance {
        self.get(self.root_cell())
            .expect("Should have had a root cell")
    }

    pub fn root_cell_cell_alias_resolver(&self) -> &CellAliasResolver {
        self.root_cell_instance().cell_alias_resolver()
    }

    /// Get a `CellName` from a path by finding the best matching cell path that
    /// is a prefix of the current path relative to the project root. e.g. `fbcode/foo/bar` matches
    /// cell path `fbcode`.
    pub fn find<P: AsRef<ProjectRelativePath> + ?Sized>(
        &self,
        path: &P,
    ) -> anyhow::Result<CellName> {
        self.0
            .path_mappings
            .get_ancestor(path.as_ref().iter())
            .copied()
            .ok_or_else(|| {
                anyhow::Error::new(CellError::UnknownCellPath(
                    path.as_ref().to_buf(),
                    self.0
                        .path_mappings
                        .keys()
                        .map(|p| p.iter().join("/"))
                        .collect(),
                ))
            })
    }

    pub fn get_cell_path<P: AsRef<ProjectRelativePath> + ?Sized>(
        &self,
        path: &P,
    ) -> anyhow::Result<CellPath> {
        let path = path.as_ref();
        let cell = self.find(path)?;
        let instance = self.get(cell)?;
        let relative = path.strip_prefix(instance.path().as_project_relative_path())?;
        Ok(CellPath::new(cell, relative.to_owned().into()))
    }

    pub fn get_cell_path_from_abs_path(
        &self,
        path: &AbsPath,
        fs: &ProjectRoot,
    ) -> anyhow::Result<CellPath> {
        let abs_path = AbsPath::new(path)?;
        self.get_cell_path(&fs.relativize_any(abs_path)?)
    }

    pub fn cells(&self) -> impl Iterator<Item = (CellName, &CellInstance)> {
        self.0
            .cells
            .iter()
            .map(|(name, instance)| (*name, instance))
    }

    /// Resolves a cell alias and a cell relative path into an absolute path.
    /// `cwd` is used to perform contextual resolution and figure out which
    /// cell mapping to use (i.e., map from alias to cell name).
    pub fn resolve_cell_relative_path(
        &self,
        cell_alias: &str,
        cell_relative_path: &str,
        project_filesystem: &ProjectRoot,
        cwd: &AbsNormPath,
    ) -> anyhow::Result<AbsNormPathBuf> {
        // We expect this to always succeed as long as the client connects to the
        // appropriate daemon.
        let proj_relative_path = project_filesystem
            .relativize(cwd)
            .with_context(|| format!("Error relativizing cwd (`{}`)", cwd))?;
        let context_cell_name = self.find(&proj_relative_path)?;
        let context_cell = self.get(context_cell_name)?;

        let resolved_cell_name = context_cell.cell_alias_resolver().resolve(cell_alias)?;
        let cell = self.get(resolved_cell_name)?;
        let cell_absolute_path = project_filesystem.resolve(cell.path().as_project_relative_path());
        cell_absolute_path.join_normalized(cell_relative_path)
    }

    /// Resolves a given 'Package' to the 'ProjectRelativePath' that points to
    /// the 'Package'
    ///
    /// ```
    /// use buck2_core::cells::{CellResolver };
    /// use buck2_core::fs::project_rel_path::{ProjectRelativePath, ProjectRelativePathBuf};
    /// use std::convert::TryFrom;
    /// use buck2_core::cells::cell_path::CellPath;
    /// use buck2_core::cells::cell_root_path::CellRootPathBuf;
    /// use buck2_core::cells::name::CellName;
    /// use buck2_core::cells::paths::CellRelativePathBuf;
    /// use buck2_core::cells::testing::CellResolverExt;
    ///
    /// let cell_path = ProjectRelativePath::new("my/cell")?;
    /// let cells = CellResolver::of_names_and_paths(
    ///     CellName::testing_new("mycell"),
    ///     &[
    ///         (CellName::testing_new("mycell"), CellRootPathBuf::new(cell_path.to_buf()))
    ///     ]);
    ///
    /// let cell_path = CellPath::new(
    ///     CellName::testing_new("mycell"),
    ///     CellRelativePathBuf::unchecked_new("some/path".to_owned()));
    ///
    /// assert_eq!(
    ///     cells.resolve_path(cell_path.as_ref())?,
    ///     ProjectRelativePathBuf::unchecked_new("my/cell/some/path".into()),
    /// );
    ///
    /// # anyhow::Ok(())
    /// ```
    pub fn resolve_path(&self, cell_path: CellPathRef) -> anyhow::Result<ProjectRelativePathBuf> {
        Ok(self.get(cell_path.cell())?.path().join(cell_path.path()))
    }

    /// resolves a given 'Package' to the 'ProjectRelativePath' that points to
    /// the 'Package'
    ///
    /// ```
    /// use buck2_core::cells::CellResolver;
    /// use buck2_core::fs::project_rel_path::{ProjectRelativePath, ProjectRelativePathBuf};
    /// use buck2_core::fs::paths::forward_rel_path::{ForwardRelativePathBuf, ForwardRelativePath};
    /// use buck2_core::package::PackageLabel;
    /// use std::convert::TryFrom;
    /// use buck2_core::cells::cell_root_path::CellRootPathBuf;
    /// use buck2_core::cells::name::CellName;
    /// use buck2_core::cells::paths::CellRelativePath;
    /// use buck2_core::cells::testing::CellResolverExt;
    ///
    /// let cell_path = ProjectRelativePath::new("my/cell")?;
    ///
    /// let cells = CellResolver::of_names_and_paths(
    ///     CellName::testing_new("mycell"),
    ///     &[
    ///         (CellName::testing_new("mycell"), CellRootPathBuf::new(cell_path.to_buf()))
    ///     ]);
    ///
    /// let pkg = PackageLabel::new(
    ///     CellName::testing_new("mycell"),
    ///     CellRelativePath::unchecked_new("somepkg"),
    /// );
    ///
    /// assert_eq!(
    ///     cells.resolve_package(pkg)?,
    ///     ProjectRelativePathBuf::unchecked_new("my/cell/somepkg".into()),
    /// );
    ///
    /// # anyhow::Ok(())
    /// ```
    pub fn resolve_package(&self, pkg: PackageLabel) -> anyhow::Result<ProjectRelativePathBuf> {
        self.resolve_path(pkg.as_cell_path())
    }
}

/// Aggregates cell information as we parse cell configs and keeps state to
/// generate a final 'CellResolver'
#[derive(Debug)]
pub struct CellsAggregator {
    cell_infos: HashMap<CellRootPathBuf, CellAggregatorInfo>,
}

fn default_buildfiles() -> Vec<FileNameBuf> {
    (["BUCK.v2", "BUCK"][..]).map(|&n| FileNameBuf::try_from(n.to_owned()).unwrap())
}

#[derive(Default, Debug)]
struct CellAggregatorInfo {
    /// The name to use for this alias.
    /// So that it is predictable, we always use the first name we encounter,
    /// so the root file can choose what the alias is called.
    name: Option<CellName>,
    /// All the aliases known by this cell.
    alias_mapping: HashMap<CellAlias, CellRootPathBuf>,
    /// The build file name in this if it's been set. If it hasn't we'll use the
    /// default `["BUCK.v2", "BUCK"]` when building the resolver.
    buildfiles: Option<Vec<FileNameBuf>>,
}

impl CellAggregatorInfo {
    fn add_alias_mapping(&mut self, from: CellAlias, to: CellRootPathBuf) -> anyhow::Result<()> {
        let old = self.alias_mapping.insert(from.clone(), to.clone());
        if let Some(old) = old {
            if old != to {
                return Err(CellError::DuplicateAliases(from, old, to).into());
            }
        }
        Ok(())
    }
}

impl CellsAggregator {
    pub fn new() -> Self {
        Self {
            cell_infos: HashMap::new(),
        }
    }

    fn cell_info(&mut self, cell_path: CellRootPathBuf) -> &mut CellAggregatorInfo {
        self.cell_infos
            .entry(cell_path)
            .or_insert_with(CellAggregatorInfo::default)
    }

    /// Adds a cell configuration entry
    pub fn add_cell_entry(
        &mut self,
        cell_root: CellRootPathBuf,
        parsed_alias: CellAlias,
        alias_path: CellRootPathBuf,
    ) -> anyhow::Result<()> {
        let name = &mut self.cell_info(alias_path.clone()).name;
        if name.is_none() {
            *name = Some(CellName::unchecked_new(&parsed_alias.0)?);
        }
        self.cell_info(cell_root)
            .add_alias_mapping(parsed_alias, alias_path)
    }

    /// Adds a cell alias configuration entry
    pub fn add_cell_alias(
        &mut self,
        cell_root: CellRootPathBuf,
        parsed_alias: CellAlias,
        alias_destination: CellAlias,
    ) -> anyhow::Result<CellRootPathBuf> {
        let cell_info = self.cell_info(cell_root);
        let alias_path = match cell_info.alias_mapping.get(&alias_destination) {
            None => return Err(CellError::AliasOnlyCell(parsed_alias, alias_destination).into()),
            Some(alias_path) => alias_path.clone(),
        };
        cell_info.add_alias_mapping(parsed_alias, alias_path.clone())?;
        Ok(alias_path)
    }

    pub fn set_buildfiles(&mut self, cell_root: CellRootPathBuf, buildfiles: Vec<FileNameBuf>) {
        let mut cell_info = self.cell_info(cell_root);
        cell_info.buildfiles = Some(buildfiles);
    }

    fn get_cell_name_from_path(&self, path: &CellRootPath) -> anyhow::Result<CellName> {
        self.cell_infos
            .get(path)
            .and_then(|info| info.name)
            .ok_or_else(|| {
                anyhow::anyhow!(CellError::UnknownCellPath(
                    path.as_project_relative_path().to_buf(),
                    self.cell_infos
                        .keys()
                        .map(|p| p.as_str().to_owned())
                        .collect()
                ))
            })
    }

    /// Creates the 'CellResolver' from all the entries that were aggregated
    pub fn make_cell_resolver(&self) -> anyhow::Result<CellResolver> {
        let mut cell_mappings = HashMap::new();
        let mut cell_path_mappings = SequenceTrie::new();

        for (cell_path, cell_info) in &self.cell_infos {
            let mut aliases_for_cell = HashMap::new();
            let cell_name = self.get_cell_name_from_path(cell_path)?;

            for (alias, path_for_alias) in &cell_info.alias_mapping {
                aliases_for_cell
                    .insert(alias.clone(), self.get_cell_name_from_path(path_for_alias)?);
            }

            let old = cell_mappings.insert(
                cell_name,
                CellInstance::new(
                    cell_name,
                    cell_path.clone(),
                    cell_info
                        .buildfiles
                        .clone()
                        .unwrap_or_else(default_buildfiles),
                    CellAliasResolver::new(cell_name, Arc::new(aliases_for_cell))?,
                ),
            );
            if let Some(old) = old {
                return Err(anyhow::anyhow!(CellError::DuplicateNames(
                    old.name(),
                    old.path().to_buf(),
                    cell_path.clone()
                )));
            }

            cell_path_mappings.insert(cell_path.iter(), cell_name);
        }

        Ok(CellResolver::new(cell_mappings, cell_path_mappings))
    }
}

// test helpers
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Arc;

    use sequence_trie::SequenceTrie;

    use super::default_buildfiles;
    use crate::cells::cell_root_path::CellRootPathBuf;
    use crate::cells::name::CellName;
    use crate::cells::CellAlias;
    use crate::cells::CellAliasResolver;
    use crate::cells::CellInstance;
    use crate::cells::CellResolver;
    pub trait CellResolverExt {
        /// Creates a new 'CellResolver' based on the given iterator of (cell
        /// name, cell path). The 'CellAliasResolver' of each cell is
        /// empty. i.e. no aliases are defined for any of the cells.
        fn of_names_and_paths(
            current: CellName,
            cells: &[(CellName, CellRootPathBuf)],
        ) -> CellResolver;

        fn with_names_and_paths_with_alias(
            cells: &[(CellName, CellRootPathBuf, HashMap<CellAlias, CellName>)],
        ) -> CellResolver;
    }

    impl CellResolverExt for CellResolver {
        fn of_names_and_paths(
            current: CellName,
            cells: &[(CellName, CellRootPathBuf)],
        ) -> CellResolver {
            let mut cell_mappings = HashMap::new();
            let mut path_mappings = SequenceTrie::new();

            for (name, path) in cells {
                cell_mappings.insert(
                    *name,
                    CellInstance::new(
                        *name,
                        path.clone(),
                        default_buildfiles(),
                        CellAliasResolver {
                            current,
                            aliases: Arc::new(Default::default()),
                        },
                    ),
                );

                path_mappings.insert(path.iter(), *name);
            }

            Self::new(cell_mappings, path_mappings)
        }

        fn with_names_and_paths_with_alias(
            cells: &[(CellName, CellRootPathBuf, HashMap<CellAlias, CellName>)],
        ) -> CellResolver {
            let mut cell_mappings = HashMap::new();
            let mut path_mappings = SequenceTrie::new();

            for (name, path, alias) in cells {
                let prev = cell_mappings.insert(
                    *name,
                    CellInstance::new(
                        *name,
                        path.clone(),
                        default_buildfiles(),
                        CellAliasResolver::new(*name, Arc::new(alias.clone())).unwrap(),
                    ),
                );
                assert!(prev.is_none());

                let prev = path_mappings.insert(path.iter(), *name);
                assert!(prev.is_none());
            }

            Self::new(cell_mappings, path_mappings)
        }
    }

    #[cfg(test)]
    #[test]
    fn test_of_names_and_paths() -> anyhow::Result<()> {
        use crate::fs::project_rel_path::ProjectRelativePathBuf;

        let cell_resolver = CellResolver::of_names_and_paths(
            CellName::testing_new("root"),
            &[(
                CellName::testing_new("foo"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("bar".into())),
            )],
        );

        let cell = cell_resolver.get(CellName::testing_new("foo"))?;
        assert_eq!(CellName::testing_new("foo"), cell.name());
        assert_eq!("bar", cell.path().as_str());

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cells::testing::CellResolverExt;
    use crate::fs::paths::forward_rel_path::ForwardRelativePath;
    use crate::fs::paths::forward_rel_path::ForwardRelativePathBuf;

    #[test]
    fn test_cells() -> anyhow::Result<()> {
        let cell1_path = CellRootPath::new(ProjectRelativePath::new("my/cell1")?);
        let cell2_path = CellRootPath::new(ProjectRelativePath::new("cell2")?);
        let cell3_path = CellRootPath::new(ProjectRelativePath::new("my/cell3")?);

        let cells = CellResolver::with_names_and_paths_with_alias(&[
            (
                CellName::testing_new("cell1"),
                cell1_path.to_buf(),
                hashmap![
                    CellAlias::new("cell1".to_owned()) => CellName::testing_new("cell1"),
                    CellAlias::new("cell2".to_owned()) => CellName::testing_new("cell2"),
                    CellAlias::new("cell3".to_owned()) => CellName::testing_new("cell3"),
                ],
            ),
            (
                CellName::testing_new("cell2"),
                cell2_path.to_buf(),
                hashmap![
                    CellAlias::new("cell2".to_owned()) => CellName::testing_new("cell2"),
                    CellAlias::new("cell1".to_owned()) => CellName::testing_new("cell1"),
                    CellAlias::new("cell3".to_owned()) => CellName::testing_new("cell3"),
                ],
            ),
            (
                CellName::testing_new("cell3"),
                cell3_path.to_buf(),
                hashmap![
                    CellAlias::new("z_cell3".to_owned()) => CellName::testing_new("cell3"),
                    CellAlias::new("z_cell1".to_owned()) => CellName::testing_new("cell1"),
                    CellAlias::new("z_cell2".to_owned()) => CellName::testing_new("cell2"),
                ],
            ),
        ]);

        {
            let cell1 = cells.get(CellName::testing_new("cell1")).unwrap();
            assert_eq!(cell1.path(), cell1_path);

            let aliases = cell1.cell_alias_resolver();
            assert_eq!(aliases.resolve("").unwrap(), CellName::testing_new("cell1"));
            assert_eq!(
                aliases.resolve("cell1").unwrap(),
                CellName::testing_new("cell1")
            );
            assert_eq!(
                aliases.resolve("cell2").unwrap(),
                CellName::testing_new("cell2")
            );
            assert_eq!(
                aliases.resolve("cell3").unwrap(),
                CellName::testing_new("cell3")
            );
        }

        {
            let cell2 = cells.get(CellName::testing_new("cell2")).unwrap();
            assert_eq!(cell2.path(), cell2_path);

            let aliases = cell2.cell_alias_resolver();
            assert_eq!(aliases.resolve("").unwrap(), CellName::testing_new("cell2"));
            assert_eq!(
                aliases.resolve("cell1").unwrap(),
                CellName::testing_new("cell1")
            );
            assert_eq!(
                aliases.resolve("cell2").unwrap(),
                CellName::testing_new("cell2")
            );
            assert_eq!(
                aliases.resolve("cell3").unwrap(),
                CellName::testing_new("cell3")
            );
        }

        {
            let cell3 = cells.get(CellName::testing_new("cell3")).unwrap();
            assert_eq!(cell3.path(), cell3_path);

            let aliases = cell3.cell_alias_resolver();
            assert_eq!(aliases.resolve("").unwrap(), CellName::testing_new("cell3"));
            assert_eq!(
                aliases.resolve("z_cell1").unwrap(),
                CellName::testing_new("cell1")
            );
            assert_eq!(
                aliases.resolve("z_cell2").unwrap(),
                CellName::testing_new("cell2")
            );
            assert_eq!(
                aliases.resolve("z_cell3").unwrap(),
                CellName::testing_new("cell3")
            );
        }

        assert_eq!(cells.find(cell1_path)?, CellName::testing_new("cell1"));
        assert_eq!(cells.find(cell2_path)?, CellName::testing_new("cell2"));
        assert_eq!(cells.find(cell3_path)?, CellName::testing_new("cell3"));
        assert_eq!(
            cells.find(
                &cell2_path
                    .as_project_relative_path()
                    .join(ForwardRelativePath::new("fake/cell3")?)
            )?,
            CellName::testing_new("cell2")
        );
        assert_eq!(
            cells.find(
                &cell3_path
                    .as_project_relative_path()
                    .join(ForwardRelativePath::new("more/foo")?)
            )?,
            CellName::testing_new("cell3")
        );
        assert!(cells.find(ProjectRelativePath::new("blah")?).is_err());

        assert_eq!(
            cells.get_cell_path(cell1_path)?,
            CellPath::new(
                CellName::testing_new("cell1"),
                ForwardRelativePathBuf::unchecked_new("".to_owned()).into()
            )
        );

        assert_eq!(
            cells.get_cell_path(cell2_path)?,
            CellPath::new(
                CellName::testing_new("cell2"),
                ForwardRelativePathBuf::unchecked_new("".to_owned()).into()
            )
        );

        assert_eq!(
            cells.get_cell_path(
                &cell2_path
                    .as_project_relative_path()
                    .join(ForwardRelativePath::new("fake/cell3")?)
            )?,
            CellPath::new(
                CellName::testing_new("cell2"),
                ForwardRelativePathBuf::unchecked_new("fake/cell3".to_owned()).into()
            )
        );

        Ok(())
    }

    #[test]
    fn test_duplicate_aliases() -> anyhow::Result<()> {
        let mut agg = CellsAggregator::new();

        let cell_root = CellRootPathBuf::new(ProjectRelativePathBuf::try_from("".to_owned())?);
        let alias_path =
            CellRootPathBuf::new(ProjectRelativePathBuf::try_from("random/path".to_owned())?);

        agg.add_cell_entry(
            cell_root.clone(),
            CellAlias::new("root".to_owned()),
            cell_root.clone(),
        )?;
        agg.add_cell_entry(
            cell_root.clone(),
            CellAlias::new("hello".to_owned()),
            alias_path.clone(),
        )?;
        agg.add_cell_entry(
            cell_root.clone(),
            CellAlias::new("cruel".to_owned()),
            alias_path.clone(),
        )?;
        agg.add_cell_entry(cell_root, CellAlias::new("world".to_owned()), alias_path)?;

        // We want the first alias to win (hello), rather than the lexiographically first (cruel)
        assert!(
            agg.make_cell_resolver()?
                .contains(CellName::testing_new("hello"))
        );
        assert!(
            !agg.make_cell_resolver()?
                .contains(CellName::testing_new("cruel"))
        );
        Ok(())
    }

    #[test]
    fn test_alias_only_error() -> anyhow::Result<()> {
        let mut agg = CellsAggregator::new();

        let cell_root = CellRootPathBuf::new(ProjectRelativePathBuf::try_from("".to_owned())?);
        assert!(
            agg.add_cell_alias(
                cell_root,
                CellAlias::new("root".to_owned()),
                CellAlias::new("does_not_exist".to_owned()),
            )
            .is_err()
        );
        Ok(())
    }
}
