/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::BTreeMap;

use buck2_data::DiceKeyState;
use buck2_data::DiceStateSnapshot;
use gazebo::prelude::*;
use superconsole::Component;

use crate::subscribers::superconsole::SuperConsoleConfig;

pub(crate) struct DiceState {
    key_states: BTreeMap<String, DiceKeyState>,
}

impl DiceState {
    pub(crate) fn new() -> Self {
        Self {
            key_states: BTreeMap::new(),
        }
    }

    pub(crate) fn update(&mut self, update: &DiceStateSnapshot) {
        for (k, v) in &update.key_states {
            self.key_states.insert(k.clone(), v.clone());
        }
    }
}

#[derive(Debug)]
pub(crate) struct DiceComponent;

impl Component for DiceComponent {
    fn draw_unchecked(
        &self,
        state: &superconsole::State,
        _dimensions: superconsole::Dimensions,
        mode: superconsole::DrawMode,
    ) -> anyhow::Result<superconsole::Lines> {
        let config = state.get::<SuperConsoleConfig>()?;
        let state = state.get::<DiceState>()?;

        if !config.enable_dice {
            return Ok(vec![]);
        }

        let mut lines = vec!["Dice Key States".to_owned()];

        let header = format!("  {:<42}  {:>12}  {:>12}", "  Key", "Pending", "Finished");
        let header_len = header.len();
        lines.push(header);
        lines.push("-".repeat(header_len));
        for (k, v) in &state.key_states {
            // We aren't guaranteed to get a final DiceStateUpdate and so we just assume all dice nodes that we
            // know about finished so that the final rendering doesn't look silly.
            let (pending, finished) = match mode {
                superconsole::DrawMode::Normal => (v.started - v.finished, v.finished),
                superconsole::DrawMode::Final => (0, v.started),
            };
            lines.push(format!(
                "    {:<40} |{:>12} |{:>12}",
                // Dice key states are all ascii
                if k.len() > 40 { &k[..40] } else { k },
                pending,
                finished
            ));
        }
        lines.push("-".repeat(header_len));
        lines.into_try_map(|v| vec![v].try_into())
    }
}
