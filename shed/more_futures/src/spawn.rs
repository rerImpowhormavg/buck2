/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! The future that is spawned, but has various more strict cancellation behaviour than
//! tokio's JoinHandle
//!

use std::any::Any;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use allocative::Allocative;
use futures::future::BoxFuture;
use futures::future::Future;
use futures::FutureExt;
use pin_project::pin_project;
use thiserror::Error;
use tracing::Instrument;
use tracing::Span;

use crate::cancellable_future::CancellableFuture;
use crate::cancellable_future::StrongRefCount;
use crate::cancellable_future::WeakRefCount;
use crate::instrumented_shared::SharedEvents;
use crate::instrumented_shared::SharedEventsFuture;
use crate::spawner::Spawner;

#[derive(Debug, Error, Copy, Clone)]
pub enum WeakFutureError {
    #[error("Join Error")]
    JoinError,

    #[error("Cancelled")]
    Cancelled,
}

/// A unit of computation within Dice. Futures to the result of this computation should be obtained
/// via this task struct
#[derive(Allocative)]
pub struct WeakJoinHandle<T: Clone> {
    #[allocative(skip)] // TODO(nga): `Shared` requires `Clone`.
    join_handle: SharedEventsFuture<BoxFuture<'static, T>>,
    #[allocative(skip)]
    guard: WeakRefCount,
}

impl<T: Send + Sync + Clone + 'static> WeakJoinHandle<T> {
    /// Return `None` if the task has been canceled.
    pub fn pollable(&self) -> Option<StrongJoinHandle<SharedEventsFuture<BoxFuture<'static, T>>>> {
        self.guard.upgrade().map(|inner| StrongJoinHandle {
            guard: inner,
            fut: self.join_handle.clone(),
        })
    }
}

impl<T: Send + Sync + Clone + 'static> WeakJoinHandle<Result<T, WeakFutureError>> {
    pub fn into_completion_observer(self) -> CompletionObserver<T> {
        CompletionObserver { inner: self }
    }
}

/// The actual pollable future that returns the result of the task. This keeps the future alive.
#[pin_project]
pub struct StrongJoinHandle<F> {
    guard: StrongRefCount,
    #[pin]
    fut: F,
}

impl<F> StrongJoinHandle<F> {
    fn map<F2>(self, map: impl FnOnce(F) -> F2) -> StrongJoinHandle<F2> {
        StrongJoinHandle {
            guard: self.guard,
            fut: map(self.fut),
        }
    }
}

impl<T> StrongJoinHandle<SharedEventsFuture<BoxFuture<'static, T>>>
where
    T: Clone,
{
    fn weak_handle(&self) -> WeakJoinHandle<T> {
        WeakJoinHandle {
            join_handle: self.fut.clone(),
            guard: self.guard.downgrade(),
        }
    }
}

impl<F: Future> StrongJoinHandle<F> {
    pub fn inner(&self) -> &F {
        &self.fut
    }
}

impl<F, T> Future for StrongJoinHandle<F>
where
    F: Future<Output = Result<T, WeakFutureError>>,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // When we have a StrongJoinHandle, we expect the future to not have been cancelled.
        let this = self.project();
        this.fut.poll(cx).map(|r| r.unwrap())
    }
}

#[pin_project]
pub struct CompletionObserver<T: Clone> {
    inner: WeakJoinHandle<Result<T, WeakFutureError>>,
}

impl<T: Clone> Future for CompletionObserver<T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.inner.join_handle.poll_unpin(cx).map(|_res| ())
    }
}

/// Spawn a cancellable future. The preamble is a non-cancellable portion that can come before.
pub fn spawn_task<T, S, P>(
    future: T,
    preamble: P,
    spawner: &dyn Spawner<S>,
    ctx: &S,
    span: Span,
) -> (
    WeakJoinHandle<Result<T::Output, WeakFutureError>>,
    StrongJoinHandle<SharedEventsFuture<BoxFuture<'static, Result<T::Output, WeakFutureError>>>>,
)
where
    T: Future + Send + 'static,
    T::Output: Any + Clone + Send + 'static,
    P: Future<Output = ()> + Send + 'static,
{
    let strong = spawn_inner(future, preamble, spawner, ctx, span).map(|f| f.instrumented_shared());
    (strong.weak_handle(), strong)
}

/// Spawn a cancellable future. The preamble is a non-cancellable portion that can come before.
fn spawn_inner<T, S, P>(
    future: T,
    preamble: P,
    spawner: &dyn Spawner<S>,
    ctx: &S,
    span: Span,
) -> StrongJoinHandle<BoxFuture<'static, Result<T::Output, WeakFutureError>>>
where
    T: Future + Send + 'static,
    T::Output: Any + Send + 'static,
    P: Future<Output = ()> + Send + 'static,
{
    // For Ready<()> and BoxFuture<()> futures we get these sizes:
    // future alone: 196/320 bits
    // future + no-op preamble via async block: 448/704 bits
    // future + no-op preamble via FuturesExt::then: 256/384 bits
    // future + no-op preamble + instrument: 512/640 bits

    // As the spawner is going to take a boxed future and erase its concrete type,
    // we can have different future types for different scenarios in order to
    // minimize the size of them.
    //
    // While we could feasibly distinguish the no-op preamble case, one extra pointer
    // is an okay cost for the simpler api (for now).
    let (future, guard) = CancellableFuture::new_refcounted(future);
    let future = future.map(|v| box v as _);
    let future = preamble.then(|_| future);
    let future = if span.is_disabled() {
        future.boxed()
    } else {
        future.instrument(span).boxed()
    };

    let task = spawner.spawn(ctx, future.boxed());
    let task = task
        .map(|v| {
            v.map_err(|_e: tokio::task::JoinError| WeakFutureError::JoinError)?
                .downcast::<Option<T::Output>>()
                .expect("Spawned task returned the wrong type")
                .ok_or(WeakFutureError::Cancelled)
        })
        .boxed();

    StrongJoinHandle { guard, fut: task }
}

/// Spawn a cancellable future.
pub fn spawn_dropcancel<T, S>(
    future: T,
    spawner: &dyn Spawner<S>,
    ctx: &S,
    span: Span,
) -> StrongJoinHandle<BoxFuture<'static, Result<T::Output, WeakFutureError>>>
where
    T: Future + Send + 'static,
    T::Output: Any + Send + 'static,
{
    spawn_inner(future, futures::future::ready(()), spawner, ctx, span)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::oneshot;

    use super::*;
    use crate::spawner::TokioSpawner;

    #[derive(Default)]
    struct MockCtx;

    #[tokio::test]
    async fn test_cancellation() {
        let (release_task, recv_release_task) = oneshot::channel();
        let (notify_success, recv_success) = oneshot::channel();

        let sp = Arc::new(TokioSpawner::default());

        let (_task, poll) = spawn_task(
            async move {
                recv_release_task.await.unwrap();
                notify_success.send(()).unwrap();
            },
            futures::future::ready(()),
            sp.as_ref(),
            &MockCtx::default(),
            tracing::debug_span!("test"),
        );

        // Throw away the strong handle.
        drop(poll);

        // Now, release the task. In all likelihood it will have already exited, but
        let _ignored = release_task.send(());

        // The task should never get to sending in notify_success since all its referenced had been
        // dropped at that point, but it *should* drop the channel itself.
        recv_success.await.unwrap_err();
    }

    #[tokio::test]
    async fn test_spawn() {
        let sp = Arc::new(TokioSpawner::default());
        let fut = async { "Hello world!" };

        let (_task, poll) = spawn_task(
            fut,
            futures::future::ready(()),
            sp.as_ref(),
            &MockCtx::default(),
            tracing::debug_span!("test"),
        );

        let res = poll.await;
        assert_eq!(res, "Hello world!");
    }
}
