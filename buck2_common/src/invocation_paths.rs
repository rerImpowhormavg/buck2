/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//!
//! Defines utilities to obtain the basic paths for buck2 client and the daemon.
//!

use std::borrow::Cow;

use allocative::Allocative;
use anyhow::Context;
use buck2_core::fs::paths::abs_norm_path::AbsNormPath;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::file_name::FileName;
use buck2_core::fs::paths::file_name::FileNameBuf;
use buck2_core::fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_core::fs::project::ProjectRelativePath;
use buck2_core::fs::project::ProjectRelativePathBuf;
use buck2_core::fs::project::ProjectRoot;
use once_cell::sync::Lazy;

use crate::daemon_dir::DaemonDir;
use crate::invocation_roots::InvocationRoots;
use crate::result::SharedResult;
use crate::result::ToSharedResultExt;

/// `~/.buck`.
#[allow(clippy::needless_borrow)] // False positive.
pub(crate) fn home_buck_dir() -> anyhow::Result<&'static AbsNormPath> {
    fn find_dir() -> anyhow::Result<AbsNormPathBuf> {
        let home = dirs::home_dir().context("Expected a HOME directory to be available")?;
        let home = AbsNormPathBuf::new(home).context("Expected an absolute HOME directory")?;
        Ok(home.join(FileName::new(".buck")?))
    }

    static DIR: Lazy<SharedResult<AbsNormPathBuf>> = Lazy::new(|| find_dir().shared_error());

    Ok(&Lazy::force(&DIR).as_ref()?)
}

#[derive(Clone, Allocative)]
pub struct InvocationPaths {
    pub roots: InvocationRoots,

    /// The isolation dir is a dir relative path used to create unique directories for
    /// all on-disk state relating to a daemon. This allows multiple daemons to run in
    /// the same project root.
    ///
    /// The daemon metadata directory is post-fixed with the isolation prefix
    /// (i.e `$HOME/.buck/buckd/<projectroot>/<isolationdir>`).
    /// The buck-out is `<projectroot>/buck-out/<isolationdir>/`
    ///
    /// Any on-disk state from the daemon (including build outputs and similar) should only
    /// be written or read from directories that include this component.
    ///
    /// This form of isolation is currently supported primarily for two uses:
    /// 1. testing - it allows us to run isolated daemons on a project for tests. This is
    /// particularly useful to allow a test in a project to recursively invoke buck, but also
    /// useful to write tests against a project's macros and rules and using a project's real
    /// configuration.
    /// 2. generally to support recursive buck invocations. while our ideal may be that these
    /// eventually are not allowed, the most pragmatic approach currently is to support them
    /// but push them into isolated, temporary daemons.
    pub isolation: FileNameBuf,
}

impl InvocationPaths {
    pub fn daemon_dir(&self) -> anyhow::Result<DaemonDir> {
        #[cfg(windows)]
        let root_relative: Cow<ForwardRelativePath> = {
            use buck2_core::fs::paths::forward_rel_path::ForwardRelativePathNormalizer;

            // Get drive letter, network share name, etc.
            // Network share contains '\' therefore it needs to be normalized.
            let prefix = self.roots.project_root.root().windows_prefix()?;
            let stripped_path = ForwardRelativePathNormalizer::normalize_path(
                self.roots.project_root.root().strip_windows_prefix()?,
            )?;
            Cow::Owned(ForwardRelativePathNormalizer::normalize_path(&prefix)?.join(stripped_path))
        };
        #[cfg(not(windows))]
        let root_relative: Cow<ForwardRelativePath> = self
            .roots
            .project_root
            .root()
            .strip_prefix(AbsNormPath::new("/")?)?;
        // TODO(cjhopman): We currently place all buckd info into a directory owned by the user.
        // This is broken when multiple users try to share the same checkout.
        //
        // **This is different than the behavior of buck1.**
        //
        // In buck1, the buck daemon is shared across users. Due to the fact that `buck run`
        // will run whatever command is returned by the daemon, buck1 has a privilege escalation
        // vulnerability.
        //
        // There's a couple ways we could resolve this:
        // 1. Use a shared .buckd information directory and have the client verify the identity of
        // the server before doing anything with it. If the identity is different, kill it and
        // start a new one.
        // 2. Keep user-owned .buckd directory, use some other mechanism to move ownership of
        // output directories between different buckd instances.
        let home_buck_dir = home_buck_dir()?;

        let prefix = "buckd";

        let mut ret = AbsNormPathBuf::with_capacity(
            home_buck_dir.as_os_str().len()
                + 1
                + prefix.len()
                + 1
                + root_relative.as_str().len()
                + 1
                + self.isolation.as_str().len(),
            home_buck_dir,
        );

        ret.push(ForwardRelativePath::new(prefix)?);
        ret.push(root_relative.as_ref());
        ret.push(&self.isolation);

        Ok(DaemonDir { path: ret })
    }

    pub fn cell_root(&self) -> &AbsNormPath {
        &self.roots.cell_root
    }

    pub fn project_root(&self) -> &ProjectRoot {
        &self.roots.project_root
    }

    pub fn log_dir(&self) -> AbsNormPathBuf {
        self.buck_out_path()
            .join(ForwardRelativePath::unchecked_new("log"))
    }

    pub fn re_logs_dir(&self) -> AbsNormPathBuf {
        self.buck_out_path()
            .join(ForwardRelativePath::unchecked_new("re_logs"))
    }

    pub fn build_count_dir(&self) -> AbsNormPathBuf {
        self.buck_out_path()
            .join(ForwardRelativePath::unchecked_new("build_count"))
    }

    pub fn dice_dump_dir(&self) -> AbsNormPathBuf {
        self.buck_out_path()
            .join(ForwardRelativePath::unchecked_new("dice_dump"))
    }

    pub fn buck_out_dir_prefix() -> &'static ProjectRelativePath {
        ProjectRelativePath::unchecked_new("buck-out")
    }

    pub fn buck_out_dir(&self) -> ProjectRelativePathBuf {
        Self::buck_out_dir_prefix().join(&self.isolation)
    }

    pub fn buck_out_path(&self) -> AbsNormPathBuf {
        self.roots.project_root.root().join(&self.buck_out_dir())
    }

    /// Directory containing on-disk cache
    pub fn cache_dir(&self) -> ProjectRelativePathBuf {
        self.buck_out_dir()
            .join(ForwardRelativePath::unchecked_new("cache"))
    }

    pub fn cache_dir_path(&self) -> AbsNormPathBuf {
        self.roots.project_root.root().join(&self.cache_dir())
    }

    /// Subdirectory of `cache_dir` responsible for storing materializer state
    pub fn materializer_state_path(&self) -> AbsNormPathBuf {
        self.cache_dir_path()
            .join(self.materializer_state_dir_name())
    }

    pub fn materializer_state_dir_name(&self) -> &FileName {
        FileName::unchecked_new("materializer_state")
    }

    pub fn valid_cache_dirs(&self) -> Vec<&FileName> {
        vec![self.materializer_state_dir_name()]
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use buck2_core::fs::paths::abs_norm_path::AbsNormPath;
    use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
    use buck2_core::fs::paths::file_name::FileNameBuf;
    use buck2_core::fs::paths::forward_rel_path::ForwardRelativePath;
    use buck2_core::fs::project::ProjectRelativePathBuf;
    use buck2_core::fs::project::ProjectRoot;

    use crate::invocation_paths::InvocationPaths;
    use crate::invocation_roots::InvocationRoots;

    #[test]
    fn test_paths() {
        let cell_root = if cfg!(windows) {
            "C:\\my\\project\\root\\cell"
        } else {
            "/my/project/root/cell"
        };
        let project_root = if cfg!(windows) {
            "C:\\my\\project"
        } else {
            "/my/project"
        };
        let paths = InvocationPaths {
            roots: InvocationRoots {
                cell_root: AbsNormPathBuf::try_from(cell_root.to_owned()).unwrap(),
                project_root: ProjectRoot::new(
                    AbsNormPathBuf::try_from(project_root.to_owned()).unwrap(),
                ),
            },
            isolation: FileNameBuf::unchecked_new("isolation"),
        };

        let expected_path = if cfg!(windows) {
            ".buck\\buckd\\C\\my\\project\\isolation"
        } else {
            ".buck/buckd/my/project/isolation"
        };
        assert_eq!(
            paths.daemon_dir().unwrap().path.as_os_str(),
            AbsNormPathBuf::try_from(
                dirs::home_dir().expect("Expected a HOME directory to be available")
            )
            .expect("Expected an absolute HOME directory")
            .join_normalized(ForwardRelativePath::unchecked_new(expected_path))
            .unwrap()
            .as_os_str()
        );

        let expected_path = if cfg!(windows) {
            "C:\\my\\project\\root\\cell"
        } else {
            "/my/project/root/cell"
        };
        assert_eq!(paths.cell_root().as_os_str(), OsStr::new(expected_path));
        let expected_path = if cfg!(windows) {
            "C:\\my\\project"
        } else {
            "/my/project"
        };
        assert_eq!(
            paths.project_root().root().as_os_str(),
            AbsNormPath::new(expected_path).unwrap().as_os_str()
        );

        assert_eq!(
            paths.buck_out_dir(),
            ProjectRelativePathBuf::unchecked_new("buck-out/isolation".to_owned())
        );
        let expected_path = if cfg!(windows) {
            "C:\\my\\project\\buck-out\\isolation"
        } else {
            "/my/project/buck-out/isolation"
        };
        assert_eq!(paths.buck_out_path().as_os_str(), OsStr::new(expected_path));

        let expected_path = if cfg!(windows) {
            "C:\\my\\project\\buck-out\\isolation\\log"
        } else {
            "/my/project/buck-out/isolation/log"
        };
        assert_eq!(paths.log_dir().as_os_str(), OsStr::new(expected_path));
        let expected_path = if cfg!(windows) {
            "C:\\my\\project\\buck-out\\isolation\\dice_dump"
        } else {
            "/my/project/buck-out/isolation/dice_dump"
        };
        assert_eq!(paths.dice_dump_dir().as_os_str(), OsStr::new(expected_path));

        assert_eq!(
            paths.cache_dir(),
            ProjectRelativePathBuf::unchecked_new("buck-out/isolation/cache".to_owned())
        );

        let expected_path = if cfg!(windows) {
            "C:\\my\\project\\buck-out\\isolation\\cache\\materializer_state"
        } else {
            "/my/project/buck-out/isolation/cache/materializer_state"
        };
        assert_eq!(
            paths.materializer_state_path().as_os_str(),
            OsStr::new(expected_path),
        );
    }
}
