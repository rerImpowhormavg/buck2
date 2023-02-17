/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;

use allocative::Allocative;
use derivative::Derivative;
use dupe::Dupe;

use crate::impls::ctx::PerLiveTransactionCtx;
use crate::versions::VersionNumber;
use crate::HashMap;

/// Tracks the currently in-flight versions for updates and reads to ensure
/// values are up to date.
#[derive(Allocative)]
pub(crate) struct VersionTracker {
    current: VersionNumber,
    /// Tracks the currently active versions and how many contexts are holding each of them.
    active_versions: HashMap<VersionNumber, ActiveVersionData>,
}

#[derive(Derivative, Allocative)]
#[derivative(Debug)]
struct ActiveVersionData {
    #[derivative(Debug = "ignore")]
    per_transaction_ctx: Arc<PerLiveTransactionCtx>,
    ref_count: usize,
}

impl VersionTracker {
    pub(crate) fn new() -> Self {
        VersionTracker {
            current: VersionNumber::ZERO,
            active_versions: HashMap::default(),
        }
    }

    /// hands out the current "latest" committed version's associated transaction context
    pub(crate) fn current(&mut self) -> Arc<PerLiveTransactionCtx> {
        let cur = self.current;

        let mut entry =
            self.active_versions
                .entry(cur)
                .or_insert_with_key(|_v| ActiveVersionData {
                    per_transaction_ctx: Arc::new(PerLiveTransactionCtx {}),
                    ref_count: 0,
                });

        entry.ref_count += 1;

        entry.per_transaction_ctx.dupe()
    }

    /// Requests the 'WriteVersion' that is intended to be used for updates to
    /// the incremental computations
    pub(crate) fn write(&mut self) -> VersionForWrites {
        VersionForWrites { tracker: self }
    }
}

pub(crate) struct VersionForWrites<'a> {
    tracker: &'a mut VersionTracker,
}

impl<'a> VersionForWrites<'a> {
    /// Commits the version write and increases the global version number
    pub(crate) fn commit(self) -> VersionNumber {
        self.tracker.current.inc();
        self.tracker.current
    }

    /// Undo the pending write to version
    pub(crate) fn undo(self) -> VersionNumber {
        self.tracker.current
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use crate::impls::core::versions::VersionTracker;
    use crate::versions::VersionNumber;

    #[test]
    fn simple_version_increases() {
        let mut vt = VersionTracker::new();
        let vg = vt.current();

        assert_matches!(
            vt.active_versions.get(&VersionNumber::new(0)), Some(active) if active.ref_count == 1
        );

        let vg = vt.current();

        assert_matches!(
            vt.active_versions.get(&VersionNumber::new(0)), Some(active) if active.ref_count == 2
        );
    }

    #[test]
    fn write_version_commits_and_undo() {
        let mut vt = VersionTracker::new();

        let v1 = vt.write();
        assert_eq!(v1.commit(), VersionNumber::new(1));

        let v1 = vt.write();
        assert_eq!(v1.undo(), VersionNumber::new(1));
    }
}
