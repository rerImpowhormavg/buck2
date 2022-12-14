/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use allocative::Allocative;
use buck2_core::collections::ordered_set::OrderedSet;
use buck2_core::fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_execute::base_deferred_key::BaseDeferredKey;
use buck2_execute::execute::request::OutputType;
use buck2_execute::path::buck_out_path::BuckOutPath;
use buck2_interpreter::starlark_promise::StarlarkPromise;
use buck2_node::configuration::execution::ExecutionPlatformResolution;
use derivative::Derivative;
use gazebo::prelude::*;
use indexmap::IndexSet;
use starlark::codemap::FileSpan;
use starlark::environment::FrozenModule;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::DictOf;
use starlark::values::Heap;
use starlark::values::OwnedFrozenValue;
use starlark::values::Trace;
use starlark::values::Tracer;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueError;
use starlark::values::ValueTyped;
use thiserror::Error;

use crate::actions::artifact::Artifact;
use crate::actions::artifact::DeclaredArtifact;
use crate::actions::artifact::OutputArtifact;
use crate::actions::registry::ActionsRegistry;
use crate::actions::UnregisteredAction;
use crate::analysis::anon_targets::AnonTargetsRegistry;
use crate::artifact_groups::registry::ArtifactGroupRegistry;
use crate::artifact_groups::ArtifactGroup;
use crate::deferred::types::BaseKey;
use crate::deferred::types::DeferredId;
use crate::deferred::types::DeferredRegistry;
use crate::dynamic::registry::DynamicRegistry;
use crate::interpreter::rule_defs::artifact::StarlarkArtifactLike;
use crate::interpreter::rule_defs::artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::artifact::StarlarkOutputArtifact;
use crate::interpreter::rule_defs::artifact::ValueAsArtifactLike;
use crate::interpreter::rule_defs::rule::FrozenRuleCallable;

#[derive(Derivative, Trace, Allocative)]
#[derivative(Debug)]
pub struct AnalysisRegistry<'v> {
    #[derivative(Debug = "ignore")]
    deferred: DeferredRegistry,
    #[derivative(Debug = "ignore")]
    actions: ActionsRegistry,
    #[derivative(Debug = "ignore")]
    artifact_groups: ArtifactGroupRegistry,
    #[derivative(Debug = "ignore")]
    dynamic: DynamicRegistry,
    anon_targets: AnonTargetsRegistry<'v>,
    analysis_value_storage: AnalysisValueStorage<'v>,
}

#[derive(Error, Debug)]
enum DeclaredArtifactError {
    #[error("Can't declare an artifact with an empty filename component")]
    DeclaredEmptyFileName,
}

impl<'v> AnalysisRegistry<'v> {
    pub fn new_from_owner(
        owner: BaseDeferredKey,
        execution_platform: ExecutionPlatformResolution,
    ) -> Self {
        Self::new_from_owner_and_deferred(
            owner.dupe(),
            execution_platform,
            DeferredRegistry::new(BaseKey::Base(owner)),
        )
    }

    pub(crate) fn new_from_owner_and_deferred(
        owner: BaseDeferredKey,
        execution_platform: ExecutionPlatformResolution,
        deferred: DeferredRegistry,
    ) -> Self {
        AnalysisRegistry {
            deferred,
            actions: ActionsRegistry::new(owner.dupe(), execution_platform.dupe()),
            artifact_groups: ArtifactGroupRegistry::new(),
            dynamic: DynamicRegistry::new(owner),
            anon_targets: AnonTargetsRegistry::new(execution_platform),
            analysis_value_storage: AnalysisValueStorage::new(),
        }
    }

    pub(crate) fn set_action_key(&mut self, action_key: Arc<str>) {
        self.actions.set_action_key(action_key);
    }

    /// Reserves a path in an output directory. Doesn't declare artifact,
    /// but checks that there is no previously declared artifact with a path
    /// which is in conflict with claimed `path`.
    pub(crate) fn claim_output_path(&mut self, path: &ForwardRelativePath) -> anyhow::Result<()> {
        self.actions.claim_output_path(path)
    }

    pub(crate) fn declare_dynamic_output(
        &mut self,
        path: BuckOutPath,
        output_type: OutputType,
    ) -> DeclaredArtifact {
        self.actions.declare_dynamic_output(path, output_type)
    }

    pub(crate) fn declare_output(
        &mut self,
        prefix: Option<&str>,
        filename: &str,
        output_type: OutputType,
    ) -> anyhow::Result<DeclaredArtifact> {
        // We want this artifact to be a file/directory inside the current context, which means
        // things like `..` and the empty path `.` can be bad ideas. The `::new` method checks for those
        // things and fails if they are present.

        if filename == "." || filename == "" {
            return Err(DeclaredArtifactError::DeclaredEmptyFileName.into());
        }

        let path = ForwardRelativePath::new(filename)?.to_owned();
        let prefix = match prefix {
            None => None,
            Some(x) => Some(ForwardRelativePath::new(x)?.to_owned()),
        };
        self.actions.declare_artifact(prefix, path, output_type)
    }

    /// Takes a string or artifact/output artifact and converts it into an output artifact
    ///
    /// This is handy for functions like `ctx.actions.write` where it's nice to just let
    /// the user give us a string if they want as the output name.
    ///
    /// This function can declare new artifacts depending on the input.
    /// If there is no error, it returns a wrapper around the artifact (ArtifactDeclaration) and the corresponding OutputArtifact
    ///
    /// The valid types for `value` and subsequent actions are as follows:
    ///  - `str`: A new file is declared with this name.
    ///  - `StarlarkOutputArtifact`: The original artifact is returned
    ///  - `StarlarkArtifact`/`StarlarkDeclaredArtifact`: If the artifact is already bound, an error is raised. Otherwise we proceed with the original artifact.
    pub(crate) fn get_or_declare_output<'v2>(
        &mut self,
        eval: &Evaluator<'v2, '_>,
        value: Value<'v2>,
        param_name: &str,
        output_type: OutputType,
    ) -> anyhow::Result<(ArtifactDeclaration<'v2>, OutputArtifact)> {
        let declaration_location = eval.call_stack_top_location();
        let heap = eval.heap();
        if let Some(path) = value.unpack_str() {
            let artifact = self.declare_output(None, path, output_type)?;
            Ok((
                ArtifactDeclaration {
                    artifact: ArtifactDeclarationKind::DeclaredArtifact(artifact.dupe()),
                    declaration_location,
                    heap,
                },
                artifact.as_output().dupe(),
            ))
        } else if let Some(output) = StarlarkOutputArtifact::unpack_value(value) {
            let output_artifact = output.artifact();
            output_artifact.ensure_output_type(output_type)?;
            Ok((
                ArtifactDeclaration {
                    artifact: ArtifactDeclarationKind::DeclaredArtifact((*output_artifact).dupe()),
                    declaration_location,
                    heap,
                },
                output_artifact,
            ))
        } else if let Some(artifact) = value.as_artifact() {
            let output_artifact = artifact.output_artifact()?;
            output_artifact.ensure_output_type(output_type)?;
            Ok((
                ArtifactDeclaration {
                    artifact: ArtifactDeclarationKind::Artifact(value, artifact),
                    declaration_location,
                    heap,
                },
                output_artifact,
            ))
        } else {
            Err(ValueError::IncorrectParameterTypeNamed(param_name.to_owned()).into())
        }
    }

    pub(crate) fn register_action<A: UnregisteredAction + 'static>(
        &mut self,
        inputs: IndexSet<ArtifactGroup>,
        outputs: IndexSet<OutputArtifact>,
        action: A,
        associated_value: Option<Value<'v>>,
    ) -> anyhow::Result<()> {
        let id = self
            .actions
            .register(&mut self.deferred, inputs, outputs, action)?;
        if let Some(value) = associated_value {
            self.analysis_value_storage.set_value(id, value);
        }
        Ok(())
    }

    pub(crate) fn create_transitive_set(
        &mut self,
        definition: Value<'v>,
        value: Option<Value<'v>>,
        children: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let set = self.artifact_groups.create_transitive_set(
            definition,
            value,
            children,
            &mut self.deferred,
            eval,
        )?;

        let key = set.key().deferred_key().id();
        let set = eval.heap().alloc_complex(set);

        self.analysis_value_storage.set_value(key, set);

        Ok(set)
    }

    pub(crate) fn register_dynamic_output(
        &mut self,
        dynamic: IndexSet<Artifact>,
        inputs: IndexSet<Artifact>,
        outputs: IndexSet<OutputArtifact>,
        attributes_lambda: Value<'v>,
    ) -> anyhow::Result<()> {
        let id = self
            .dynamic
            .register(dynamic, inputs, outputs, &mut self.deferred)?;
        self.analysis_value_storage.set_value(id, attributes_lambda);
        Ok(())
    }

    pub(crate) fn register_anon_target(
        &mut self,
        promise: ValueTyped<'v, StarlarkPromise<'v>>,
        rule: ValueTyped<'v, FrozenRuleCallable>,
        attributes: DictOf<'v, &'v str, Value<'v>>,
    ) -> anyhow::Result<()> {
        self.anon_targets.register(promise, rule, attributes)
    }

    pub(crate) fn get_promises(&mut self) -> Option<AnonTargetsRegistry<'v>> {
        self.anon_targets.get_promises()
    }

    pub(crate) fn assert_no_promises(&self) -> anyhow::Result<()> {
        self.anon_targets.assert_no_promises()
    }

    /// You MUST pass the same module to both the first function and the second one.
    /// It requires both to get the lifetimes to line up.
    pub fn finalize(
        self,
        env: &'v Module,
    ) -> impl FnOnce(Module) -> anyhow::Result<(FrozenModule, DeferredRegistry)> {
        let AnalysisRegistry {
            mut deferred,
            dynamic,
            actions,
            artifact_groups,
            anon_targets: _,
            analysis_value_storage,
        } = self;
        analysis_value_storage.write_to_module(env);
        move |env| {
            let frozen_env = env.freeze()?;
            let analysis_value_fetcher = AnalysisValueFetcher {
                frozen_module: Some(frozen_env.dupe()),
            };
            actions.ensure_bound(&mut deferred, &analysis_value_fetcher)?;
            artifact_groups.ensure_bound(&mut deferred, &analysis_value_fetcher)?;
            dynamic.ensure_bound(&mut deferred, &analysis_value_fetcher)?;
            Ok((frozen_env, deferred))
        }
    }
}

enum ArtifactDeclarationKind<'v> {
    Artifact(Value<'v>, &'v dyn StarlarkArtifactLike),
    DeclaredArtifact(DeclaredArtifact),
}

pub struct ArtifactDeclaration<'v> {
    artifact: ArtifactDeclarationKind<'v>,
    declaration_location: Option<FileSpan>,
    heap: &'v Heap,
}

impl<'v> ArtifactDeclaration<'v> {
    pub fn into_declared_artifact(
        self,
        associated_artifacts: Arc<OrderedSet<ArtifactGroup>>,
    ) -> Value<'v> {
        match self.artifact {
            ArtifactDeclarationKind::Artifact(v, a) => {
                if associated_artifacts.is_empty() {
                    v
                } else {
                    a.allocate_artifact_with_extended_associated_artifacts(
                        self.heap,
                        &associated_artifacts,
                    )
                }
            }
            ArtifactDeclarationKind::DeclaredArtifact(d) => self.heap.alloc(
                StarlarkDeclaredArtifact::new(self.declaration_location, d, associated_artifacts),
            ),
        }
    }
}

/// Store `Value<'v>` values for actions registered in an implementation function
///
/// Threading lifetimes through the various action registries is kind of a pain. So instead,
/// store the starlark values in this struct, using the `DeferredId` as the key.
///
/// These values eventually are written into the mutable `Module`, and a wrapper is
/// made available to get the `OwnedFrozenValue` back out after that `Module` is frozen.
///
/// Note that this object has internal mutation and is only expected to live for the duration
/// of impl function execution.
///
/// At the end of impl function execution, `write_to_module` should be called to ensure
/// that the values are written the top level of the `Module`.
#[derive(Debug, Allocative)]
struct AnalysisValueStorage<'v> {
    values: HashMap<DeferredId, Value<'v>>,
}

unsafe impl<'v> Trace<'v> for AnalysisValueStorage<'v> {
    fn trace(&mut self, tracer: &Tracer<'v>) {
        for v in self.values.values_mut() {
            tracer.trace(v)
        }
    }
}

/// Simple fetcher that fetches the values written in `AnalysisValueStorage::write_to_module`
///
/// These values are pulled from the `FrozenModule` that results from `env.freeze()`.
/// This is used by the action registry to make an `OwnedFrozenValue` available to
/// Actions' register function.
#[derive(Default)]
pub(crate) struct AnalysisValueFetcher {
    frozen_module: Option<FrozenModule>,
}

impl<'v> AnalysisValueStorage<'v> {
    fn new() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    /// Write all of the values to `module` using an internal name
    fn write_to_module(&self, module: &'v Module) {
        for (id, v) in self.values.iter() {
            let starlark_key = format!("$action_key_{}", id);
            module.set(&starlark_key, *v);
        }
    }

    /// Add a value to the internal hash map that maps ids -> values
    fn set_value(&mut self, id: DeferredId, value: Value<'v>) {
        self.values.insert(id, value);
    }
}

impl AnalysisValueFetcher {
    /// Get the `OwnedFrozenValue` that corresponds to a `DeferredId`, if present
    pub(crate) fn get(&self, id: DeferredId) -> anyhow::Result<Option<OwnedFrozenValue>> {
        match &self.frozen_module {
            None => Ok(None),
            Some(module) => {
                let starlark_key = format!("$action_key_{}", id);
                // This return `Err` is the symbol is private.
                // It is never private, but error is better than panic.
                module.get_option(&starlark_key)
            }
        }
    }
}
