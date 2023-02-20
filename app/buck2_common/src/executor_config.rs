/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

use allocative::Allocative;
use buck2_core::collections::sorted_map::SortedMap;
use derive_more::Display;
use dupe::Dupe;
use internment_tweaks::Intern;
use internment_tweaks::StaticInterner;
use once_cell::sync::Lazy;

#[derive(Debug, Eq, Hash, PartialEq, Clone, Dupe, Allocative)]
pub struct LocalExecutorOptions {}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Dupe, Display, Allocative)]
pub struct RemoteExecutorUseCase(Intern<String>);

impl RemoteExecutorUseCase {
    pub fn new(use_case: String) -> Self {
        static USE_CASE_INTERNER: StaticInterner<String> = StaticInterner::new();
        Self(USE_CASE_INTERNER.intern(use_case))
    }

    pub fn as_str(&self) -> &'static str {
        self.0.deref_static().as_str()
    }

    /// The "buck2-default" use case. This is meant to be used when no use case is configured. It's
    /// not meant to be used for convenience when a use case is not available where it's needed!
    pub fn buck2_default() -> Self {
        static USE_CASE: Lazy<RemoteExecutorUseCase> =
            Lazy::new(|| RemoteExecutorUseCase::new("buck2-default".to_owned()));
        *USE_CASE
    }
}

// The derived PartialEq (which uses pointer equality on the interned data) is still correct.
#[allow(clippy::derive_hash_xor_eq)]
impl Hash for RemoteExecutorUseCase {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

#[derive(Debug, Default, Eq, PartialEq, Clone, Hash, Allocative)]
pub struct RemoteExecutorOptions {
    pub re_action_key: Option<String>,
    pub re_max_input_files_bytes: Option<u64>,
}

/// The actual executor portion of a RemoteEnabled executor. It's possible for a RemoteEnabled
/// executor to wrap a local executor, which is a glorified way of saying "this is a local executor
/// with a RE backend for caching".
#[derive(Debug, Eq, PartialEq, Clone, Hash, Allocative)]
pub enum RemoteEnabledExecutor {
    Local(LocalExecutorOptions),
    Remote(RemoteExecutorOptions),
    Hybrid {
        local: LocalExecutorOptions,
        remote: RemoteExecutorOptions,
        level: HybridExecutionLevel,
    },
}

#[derive(Debug, Eq, PartialEq, Clone, Hash, Allocative)]
pub enum Executor {
    /// This executor only runs local commands.
    Local(LocalExecutorOptions),

    /// This executor interacts with a RE backend. It may use that to read or write to caches, or
    /// to execute commands.
    RemoteEnabled {
        executor: RemoteEnabledExecutor,
        re_properties: SortedMap<String, String>,
        re_use_case: RemoteExecutorUseCase,
        cache_upload_behavior: CacheUploadBehavior,
        // FIXME: Make caching configurable here.
    },
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Dupe, Hash, Allocative)]
pub enum PathSeparatorKind {
    Unix,
    Windows,
}

impl PathSeparatorKind {
    pub fn system_default() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Dupe, Hash, Allocative)]
pub enum CacheUploadBehavior {
    Enabled { max_bytes: Option<u64> },
    Disabled,
}

impl Default for CacheUploadBehavior {
    fn default() -> Self {
        Self::Disabled
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Allocative)]
pub struct CommandExecutorConfig {
    pub executor: Executor,
    pub path_separator: PathSeparatorKind,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Dupe, Hash, Allocative)]
pub enum HybridExecutionLevel {
    /// Expose both executors but only run it in one preferred executor.
    Limited,
    /// Expose both executors, fallback to the non-preferred executor if execution on the preferred
    /// executor doesn't provide a successful response. By default, we fallback only on errors (i.e.
    /// the infra failed), but not on failures (i.e. the job exited with 1). If
    /// `fallback_on_failure` is set, then we also fallback on failures.
    Fallback { fallback_on_failure: bool },
    /// Race both executors.
    Full {
        fallback_on_failure: bool,
        low_pass_filter: bool,
    },
}

impl CommandExecutorConfig {
    pub fn testing_local() -> Arc<CommandExecutorConfig> {
        Arc::new(CommandExecutorConfig {
            executor: Executor::Local(LocalExecutorOptions {}),
            path_separator: PathSeparatorKind::system_default(),
        })
    }
}
