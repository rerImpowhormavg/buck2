/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::ops::Deref;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_interpreter::types::label::Label;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::values::dict::*;
use starlark::values::type_repr::DictType;
use starlark::values::Coerce;
use starlark::values::Freeze;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueError;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use thiserror::Error;

use crate::actions::artifact::artifact_type::Artifact;
use crate::interpreter::rule_defs::artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::ValueAsArtifactLike;
// Provider that signals a rule is installable (ex. android_binary)

#[derive(Debug, Error)]
enum InstallInfoProviderErrors {
    #[error("expected a label, got `{0}` (type `{1}`)")]
    ExpectedLabel(String, String),
}

#[internal_provider(install_info_creator)]
#[derive(Clone, Coerce, Debug, Freeze, Trace, ProvidesStaticType, Allocative)]
#[repr(C)]
#[freeze(validator = validate_install_info, bounds = "V: ValueLike<'freeze>")]
pub struct InstallInfoGen<V> {
    // Label for the installer
    #[provider(field_type = "Label")]
    installer: V,
    // list of files that need to be installed
    #[provider(field_type = "DictType<String, StarlarkArtifact>")]
    files: V,
}

impl FrozenInstallInfo {
    pub fn get_installer(&self) -> anyhow::Result<ConfiguredProvidersLabel> {
        let label = Label::from_value(self.installer.to_value())
            .ok_or_else(|| {
                InstallInfoProviderErrors::ExpectedLabel(
                    self.installer.to_value().to_repr(),
                    self.installer.to_value().get_type().to_owned(),
                )
            })?
            .label()
            .to_owned();
        Ok(label)
    }

    pub fn get_files(&self) -> anyhow::Result<SmallMap<&str, Artifact>> {
        let files = DictRef::from_value(self.files.to_value()).expect("Value is a Dict");
        let mut artifacts: SmallMap<&str, Artifact> = SmallMap::with_capacity(files.len());
        for (k, v) in files.iter() {
            artifacts.insert(
                k.unpack_str().expect("should be a string"),
                v.as_artifact()
                    .ok_or_else(|| anyhow::anyhow!("not an artifact"))?
                    .get_bound_artifact()?,
            );
        }
        Ok(artifacts)
    }
}

#[starlark_module]
fn install_info_creator(globals: &mut GlobalsBuilder) {
    fn InstallInfo<'v>(
        installer: ValueOf<'v, &'v Label>,
        files: ValueOf<'v, SmallMap<&'v str, Value<'v>>>,
    ) -> anyhow::Result<InstallInfo<'v>> {
        for v in files.typed.values() {
            v.as_artifact().ok_or(ValueError::IncorrectParameterType)?;
        }
        let files = files.value;
        let info = InstallInfo {
            installer: *installer,
            files,
        };
        validate_install_info(&info)?;
        Ok(info)
    }
}

fn validate_install_info<'v, V>(info: &InstallInfoGen<V>) -> anyhow::Result<()>
where
    V: ValueLike<'v>,
{
    let files = DictRef::from_value(info.files.to_value()).expect("Value is a Dict");
    for (k, v) in files.deref().iter() {
        let (artifact, other_artifacts) = v
            .as_artifact()
            .ok_or_else(|| anyhow::anyhow!("not an artifact"))?
            .get_bound_artifact_and_associated_artifacts()?;
        if !other_artifacts.is_empty() {
            return Err(anyhow::anyhow!(
                "File with key `{}`: `{}` should not have any associated artifacts",
                k,
                artifact
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use buck2_common::result::SharedResult;
    use buck2_interpreter_for_build::interpreter::testing::Tester;
    use buck2_interpreter_for_build::label::testing::label_creator;
    use indoc::indoc;

    use crate::interpreter::rule_defs::artifact::testing::artifactory;
    use crate::interpreter::rule_defs::provider::collection::tester::collection_creator;
    use crate::interpreter::rule_defs::register_rule_defs;

    fn tester() -> Tester {
        let mut tester = Tester::new().unwrap();
        tester.additional_globals(collection_creator);
        tester.additional_globals(artifactory);
        tester.additional_globals(label_creator);
        tester.additional_globals(register_rule_defs);
        tester
    }

    #[test]
    fn install_info_works_as_provider_key() -> SharedResult<()> {
        let content = indoc!(
            r#"
             installer_app = label("//foo:bar[quz]")
             c = create_collection([InstallInfo(installer=installer_app, files={}), DefaultInfo(), RunInfo()])
             def test():
                 assert_eq(True, contains_provider(c, InstallInfo))
             "#
        );
        let mut tester = tester();
        tester.run_starlark_bzl_test(content)
    }

    #[test]
    fn info_validator_succeeds_for_artifacts_without_additional_artifacts() -> SharedResult<()> {
        let content = indoc!(
            r#"
             a1 = source_artifact("foo/bar", "baz.h")
             a2 = bound_artifact("//:dep1", "dir/baz.h")
             installer_app = label("//foo:bar[quz]")
             c = create_collection([InstallInfo(installer=installer_app, files={"a1": a1, "a2": a2}), DefaultInfo(), RunInfo()])
             def test():
                 assert_eq(True, contains_provider(c, InstallInfo))
             "#
        );
        let mut tester = tester();
        tester.run_starlark_bzl_test(content)
    }
}
