/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use buck2_core::collections::ordered_map::OrderedMap;
use buck2_core::configuration::transition::applied::TransitionApplied;
use buck2_core::configuration::transition::id::TransitionId;
use buck2_core::configuration::Configuration;
use buck2_core::configuration::ConfigurationData;
use buck2_core::target::label::TargetLabel;
use dupe::Dupe;

use crate::attrs::configuration_context::AttrConfigurationContext;

pub fn configuration_ctx() -> impl AttrConfigurationContext {
    struct TestAttrConfigurationContext(Configuration, Configuration, ConfigurationData);
    impl AttrConfigurationContext for TestAttrConfigurationContext {
        fn cfg(&self) -> Configuration {
            self.0.dupe()
        }

        fn matches<'a>(&'a self, label: &TargetLabel) -> Option<&'a ConfigurationData> {
            match label.to_string().as_ref() {
                "root//other:config" => Some(&self.2),
                _ => None,
            }
        }

        fn exec_cfg(&self) -> Configuration {
            self.1.dupe()
        }

        fn platform_cfg(&self, _label: &TargetLabel) -> anyhow::Result<Configuration> {
            panic!("not used in tests")
        }

        fn resolved_transitions(&self) -> &OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>> {
            panic!("not used in tests")
        }
    }

    TestAttrConfigurationContext(
        Configuration::testing_new(),
        Configuration::from_platform(
            "cfg_for//:testing_exec".to_owned(),
            ConfigurationData {
                constraints: BTreeMap::new(),
                buckconfigs: BTreeMap::new(),
            },
        )
        .unwrap(),
        ConfigurationData {
            constraints: BTreeMap::new(),
            buckconfigs: BTreeMap::new(),
        },
    )
}
