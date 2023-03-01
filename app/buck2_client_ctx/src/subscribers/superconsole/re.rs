/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_event_observer::re_state::ReState;
use superconsole::Component;

use crate::subscribers::superconsole::SuperConsoleConfig;

/// Draw the test summary line above the `timed_list`
#[derive(Debug)]
pub struct ReHeader;

impl ReHeader {
    pub fn boxed() -> Box<dyn Component> {
        Box::new(Self)
    }
}

impl Component for ReHeader {
    fn draw_unchecked(
        &self,
        state: &superconsole::State,
        _dimensions: superconsole::Dimensions,
        mode: superconsole::DrawMode,
    ) -> anyhow::Result<superconsole::Lines> {
        let config = state.get::<SuperConsoleConfig>()?;
        let re = state.get::<ReState>()?;
        re.render(config.enable_detailed_re, mode)
    }
}
