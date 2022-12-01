/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::cell::RefCell;
use std::sync::Arc;

use buck2_core::build_file_path::BuildFilePath;
use buck2_core::bzl::ImportPath;
use buck2_core::target::TargetLabel;
use buck2_interpreter::extra::ExtraContext;
use buck2_interpreter::package_imports::ImplicitImport;
use buck2_node::nodes::eval_result::EvaluationResult;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_node::nodes::unconfigured::TargetsMap;
use gazebo::prelude::*;
use starlark::environment::FrozenModule;
use starlark::values::OwnedFrozenValue;
use starlark_map::small_map;

use crate::attrs::coerce::ctx::BuildAttrCoercionContext;

impl From<ModuleInternals> for EvaluationResult {
    // TODO(cjhopman): Let's make this an `into_evaluation_result()` on ModuleInternals instead.
    fn from(internals: ModuleInternals) -> Self {
        let ModuleInternals {
            recorder,
            buildfile_path,
            imports,
            ..
        } = internals;
        EvaluationResult::new(buildfile_path, imports, recorder.take())
    }
}

/// ModuleInternals contains the module/package-specific information for
/// evaluating build files. Built-in functions that need access to
/// package-specific information or objects can get them by acquiring the
/// ModuleInternals.
pub struct ModuleInternals {
    attr_coercion_context: BuildAttrCoercionContext,
    buildfile_path: Arc<BuildFilePath>,
    /// Have you seen an oncall annotation yet
    oncall: RefCell<Option<Arc<String>>>,
    /// Directly imported modules.
    imports: Vec<ImportPath>,
    recorder: TargetsRecorder,
    package_implicits: Option<PackageImplicits>,
    default_visibility_to_public: bool,
    record_target_call_stacks: bool,
}

impl ExtraContext for ModuleInternals {
    type EvalResult = EvaluationResult;
}

pub(crate) struct PackageImplicits {
    import_spec: Arc<ImplicitImport>,
    env: FrozenModule,
}

impl PackageImplicits {
    pub(crate) fn new(import_spec: Arc<ImplicitImport>, env: FrozenModule) -> Self {
        Self { import_spec, env }
    }

    fn lookup(&self, name: &str) -> Option<OwnedFrozenValue> {
        self.env
            .get_option(self.import_spec.lookup_alias(name))
            .ok()
            .flatten()
    }
}

impl ModuleInternals {
    pub(crate) fn new(
        attr_coercion_context: BuildAttrCoercionContext,
        buildfile_path: Arc<BuildFilePath>,
        imports: Vec<ImportPath>,
        package_implicits: Option<PackageImplicits>,
        default_visibility_to_public: bool,
        record_target_call_stacks: bool,
    ) -> Self {
        Self {
            attr_coercion_context,
            buildfile_path,
            oncall: RefCell::new(None),
            imports,
            package_implicits,
            recorder: TargetsRecorder::new(),
            default_visibility_to_public,
            record_target_call_stacks,
        }
    }

    pub(crate) fn attr_coercion_context(&self) -> &BuildAttrCoercionContext {
        &self.attr_coercion_context
    }

    pub fn record(&self, target_node: TargetNode) -> anyhow::Result<()> {
        self.recorder.record(target_node)
    }

    pub(crate) fn recorded_is_empty(&self) -> bool {
        self.recorder.is_empty()
    }

    pub(crate) fn has_seen_oncall(&self) -> bool {
        self.oncall.borrow().is_some()
    }

    pub(crate) fn set_oncall(&self, name: &str) {
        *self.oncall.borrow_mut() = Some(Arc::new(name.to_owned()))
    }

    pub fn get_oncall(&self) -> Option<Arc<String>> {
        self.oncall.borrow().dupe()
    }

    pub(crate) fn target_exists(&self, name: &str) -> bool {
        (*self.recorder.targets.borrow()).contains_key(name)
    }

    pub fn buildfile_path(&self) -> &Arc<BuildFilePath> {
        &self.buildfile_path
    }

    pub(crate) fn get_package_implicit(&self, name: &str) -> Option<OwnedFrozenValue> {
        self.package_implicits
            .as_ref()
            .and_then(|implicits| implicits.lookup(name))
    }

    pub(crate) fn default_visibility_to_public(&self) -> bool {
        self.default_visibility_to_public
    }

    pub fn record_target_call_stacks(&self) -> bool {
        self.record_target_call_stacks
    }
}

// Records the targets declared when evaluating a build file.
struct TargetsRecorder {
    targets: RefCell<TargetsMap>,
}

#[derive(Debug, thiserror::Error)]
enum TargetsError {
    #[error("Attempted to register target {0} twice")]
    RegisteredTargetTwice(TargetLabel),
}

impl TargetsRecorder {
    fn new() -> Self {
        Self {
            targets: RefCell::new(TargetsMap::new()),
        }
    }

    fn is_empty(&self) -> bool {
        self.targets.borrow().is_empty()
    }

    fn record(&self, target_node: TargetNode) -> anyhow::Result<()> {
        let mut rules = self.targets.borrow_mut();
        match rules.entry(target_node.label().name().dupe()) {
            small_map::Entry::Vacant(o) => {
                o.insert(target_node);
                Ok(())
            }
            small_map::Entry::Occupied(_) => {
                Err(TargetsError::RegisteredTargetTwice(target_node.label().dupe()).into())
            }
        }
    }

    fn take(self) -> TargetsMap {
        self.targets.into_inner()
    }
}
