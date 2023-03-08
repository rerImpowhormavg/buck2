/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! A task stored by Dice that is shared for all transactions at the same version
use std::any::Any;
use std::cell::UnsafeCell;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::task::Waker;

use allocative::Allocative;
use dupe::Dupe;
use dupe::IterDupedExt;
use dupe::OptionDupedExt;
use futures::task::AtomicWaker;
use hashbrown::HashSet;
use parking_lot::Mutex;
use slab::Slab;
use tokio::task::JoinHandle;
use triomphe::Arc;

use crate::api::error::DiceResult;
use crate::impls::key::DiceKey;
use crate::impls::key::ParentKey;
use crate::impls::task::handle::DiceTaskHandle;
use crate::impls::task::handle::TaskState;
use crate::impls::task::promise::DicePromise;
use crate::impls::task::state::AtomicDiceTaskState;
use crate::impls::value::DiceComputedValue;

///
/// 'DiceTask' is approximately a copy of Shared and Weak from std, but with some custom special
/// record keeping to allow us to track the waiters as DiceKeys.
///
/// 'std::future::Weak' is akin to 'DiceTask', and each 'DicePromise' is a strong reference to it
/// akin to a 'std::future::Shared'.
///
/// The DiceTask is always completed by a thread whose future is the 'JoinHandle'. The thread
/// reports updates to the state of the future via 'DiceTaskHandle'. Simplifying the future
/// implementation in that no poll will ever be doing real work. No Wakers sleeping will be awoken
/// unless the task is ready.
/// The task is not the "standard states" of Pending, Polling, etc as stored by Shared future,
/// but instead we store the Dice specific states so that its useful when we dump the state.
/// Wakers are tracked with their corresponding DiceKeys, allowing us to track the rdeps and see
/// which key is waiting on what
///
/// We can explicitly track cancellations by tracking the Waker drops.
///
/// Memory size difference:
/// DiceTask <-> Weak: DiceTask holds an extra JoinHandle which is a single ptr.
/// DiceTask now holds a 'triomphe::Arc' instead of 'std::Arc' which is slightly more efficient as it
/// doesn't require weak ptr handling. This is just so that we have the JoinHandle so we can abort
/// when canceled, but we could choose to change the implementation by moving cancellation
/// notification into the DiceTaskInternal
#[derive(Allocative)]
pub(crate) struct DiceTask {
    pub(super) internal: Arc<DiceTaskInternal>,
    /// The spawned task that is responsible for completing this task.
    #[allocative(skip)]
    pub(super) spawned: Option<JoinHandle<Box<dyn Any + Send>>>,
}

#[derive(Allocative)]
pub(super) struct DiceTaskInternal {
    /// The internal progress state of the task
    pub(super) state: AtomicDiceTaskState,
    /// Other DiceTasks that are awaiting the completion of this task.
    ///
    /// We hold a pair DiceKey and Waker.
    /// Compared to 'Shared', which just holds a standard 'Waker', the Waker itself is now an
    /// AtomicWaker, which is an extra AtomicUsize, so this is marginally larger than the standard
    /// Shared future.
    pub(super) dependants: Mutex<Option<Slab<(ParentKey, Arc<AtomicWaker>)>>>,
    /// The value if finished computing
    #[allocative(skip)] // TODO should measure this
    maybe_value: UnsafeCell<Option<DiceResult<DiceComputedValue>>>,
}

impl DiceTask {
    /// `k` depends on this task, returning a `DicePromise` that will complete when this task
    /// completes
    pub(crate) fn depended_on_by(&self, k: ParentKey) -> DicePromise {
        if self.internal.state.is_ready(Ordering::Acquire) {
            DicePromise::ready(triomphe_dupe(&self.internal))
        } else {
            let mut wakers = self.internal.dependants.lock();
            match wakers.deref_mut() {
                None => {
                    assert!(
                        self.internal.state.is_ready(Ordering::SeqCst),
                        "invalid state where deps are taken before state is ready"
                    );
                    DicePromise::ready(triomphe_dupe(&self.internal))
                }
                Some(ref mut wakers) => {
                    let waker = Arc::new(AtomicWaker::new());
                    let id = wakers.insert((k, triomphe_dupe(&waker)));

                    DicePromise::pending(id, triomphe_dupe(&self.internal), waker)
                }
            }
        }
    }

    /// Get the value if already complete, or complete it. Note that `f` may run even if the result
    /// is not used.
    pub(crate) fn get_or_complete(
        &self,
        f: impl FnOnce() -> DiceResult<DiceComputedValue>,
    ) -> DiceResult<DiceComputedValue> {
        if let Some(res) = self.internal.read_value() {
            res
        } else {
            match self.internal.state.report_project() {
                TaskState::Continue => {}
                TaskState::Finished => {
                    return self
                        .internal
                        .read_value()
                        .expect("task finished must mean result is ready");
                }
            }

            let value = f();

            self.internal.set_value(value)
        }
    }

    pub(crate) fn inspect_waiters(&self) -> Option<Vec<ParentKey>> {
        self.internal
            .dependants
            .lock()
            .deref()
            .as_ref()
            .map(|deps| deps.iter().map(|(_, (k, _))| *k).collect())
    }
}

impl DiceTaskInternal {
    pub(super) fn drop_waiter(&self, slab: usize) {
        let mut deps = self.dependants.lock();
        match deps.deref_mut() {
            None => {}
            Some(ref mut deps) => {
                deps.remove(slab);
            }
        }
    }

    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: AtomicDiceTaskState::default(),
            dependants: Mutex::new(Some(Slab::new())),
            maybe_value: UnsafeCell::new(None),
        })
    }

    pub(super) fn read_value(&self) -> Option<DiceResult<DiceComputedValue>> {
        if self.state.is_ready(Ordering::Acquire) {
            Some(
                unsafe {
                    // SAFETY: main thread only writes this before setting state to `READY`
                    &*self.maybe_value.get()
                }
                .as_ref()
                .duped()
                .expect("result should be present"),
            )
        } else {
            None
        }
    }

    pub(super) fn set_value(
        &self,
        value: DiceResult<DiceComputedValue>,
    ) -> DiceResult<DiceComputedValue> {
        match self.state.sync() {
            TaskState::Continue => {}
            TaskState::Finished => {
                return self
                    .read_value()
                    .expect("task finished must mean result is ready");
            }
        };

        let prev_exist = unsafe {
            // SAFETY: no tasks read the value unless state is converted to `READY`
            &mut *self.maybe_value.get()
        }
        .replace(value.dupe())
        .is_some();
        assert!(
            !prev_exist,
            "invalid state where somehow value was already written"
        );

        self.state.report_ready();
        self.wake_deps();

        value
    }

    pub(super) fn wake_deps(&self) {
        let mut deps = self
            .dependants
            .lock()
            .take()
            .expect("Invalid state where deps where taken already");

        deps.drain().for_each(|(_k, waker)| waker.wake());
    }
}

// our use of `UnsafeCell` is okay to be send and sync.
// Each unsafe block around its access has comments explaining the invariants.
unsafe impl Send for DiceTaskInternal {}
unsafe impl Sync for DiceTaskInternal {}

fn triomphe_dupe<T>(t: &Arc<T>) -> Arc<T> {
    t.clone() // triomphe arc is actually dupe
}
