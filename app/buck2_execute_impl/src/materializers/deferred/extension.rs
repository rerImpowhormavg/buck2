/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Debug;
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_execute::materialize::materializer::DeferredMaterializerEntry;
use buck2_execute::materialize::materializer::DeferredMaterializerExtensions;
use chrono::DateTime;
use chrono::Duration;
use chrono::TimeZone;
use chrono::Utc;
use derivative::Derivative;
use derive_more::Display;
use dupe::Dupe;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::materializers::deferred::clean_stale::CleanStaleArtifacts;
use crate::materializers::deferred::io_handler::create_ttl_refresh;
use crate::materializers::deferred::ArtifactMaterializationMethod;
use crate::materializers::deferred::ArtifactMaterializationStage;
use crate::materializers::deferred::ArtifactTree;
use crate::materializers::deferred::DefaultIoHandler;
use crate::materializers::deferred::DeferredMaterializer;
use crate::materializers::deferred::DeferredMaterializerCommandProcessor;
use crate::materializers::deferred::MaterializerCommand;
use crate::materializers::deferred::WithPathsIterator;

pub(super) trait ExtensionCommand<T>: Debug + Sync + Send + 'static {
    fn execute(
        self: Box<Self>,
        tree: &mut ArtifactTree,
        processor: &mut DeferredMaterializerCommandProcessor<T>,
    );
}

#[derive(Debug, Display)]
enum PathData {
    #[display(fmt = "materialized (ts={:?})", "_0")]
    Materialized(DateTime<Utc>),

    #[display(fmt = "declared: {}", "_0")]
    Declared(Arc<ArtifactMaterializationMethod>),
}

impl DeferredMaterializerEntry for PathData {}

#[derive(Derivative)]
#[derivative(Debug)]
struct Iterate {
    /// This is for debug commands so we use an unbounded channel to avoid locking up the
    /// materializer command thread.
    #[derivative(Debug = "ignore")]
    sender: UnboundedSender<(ProjectRelativePathBuf, Box<dyn DeferredMaterializerEntry>)>,
}

impl ExtensionCommand<DefaultIoHandler> for Iterate {
    fn execute(
        self: Box<Self>,
        tree: &mut ArtifactTree,
        _processor: &mut DeferredMaterializerCommandProcessor<DefaultIoHandler>,
    ) {
        for (path, data) in tree.iter().with_paths() {
            let path_data = match &data.stage {
                ArtifactMaterializationStage::Declared { method, .. } => {
                    PathData::Declared(method.dupe())
                }
                ArtifactMaterializationStage::Materialized {
                    last_access_time, ..
                } => {
                    // drop nano-seconds
                    let timestamp = Utc
                        .timestamp_opt(last_access_time.timestamp(), 0)
                        .single()
                        .unwrap();
                    PathData::Materialized(timestamp)
                }
            };

            match self.sender.send((path, box path_data as _)) {
                Ok(..) => {}
                Err(..) => break, // No use sending more if the client disconnected.
            }
        }
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
struct RefreshTtls {
    sender: Sender<Option<JoinHandle<anyhow::Result<()>>>>,
    min_ttl: i64,
}

impl ExtensionCommand<DefaultIoHandler> for RefreshTtls {
    fn execute(
        self: Box<Self>,
        tree: &mut ArtifactTree,
        processor: &mut DeferredMaterializerCommandProcessor<DefaultIoHandler>,
    ) {
        let task = create_ttl_refresh(
            tree,
            &processor.io.re_client_manager,
            Duration::seconds(self.min_ttl),
        )
        .map(|f| processor.rt.spawn(f));
        let _ignored = self.sender.send(task);
    }
}

#[async_trait]
impl DeferredMaterializerExtensions for DeferredMaterializer {
    fn iterate(
        &self,
    ) -> anyhow::Result<
        BoxStream<'static, (ProjectRelativePathBuf, Box<dyn DeferredMaterializerEntry>)>,
    > {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.command_sender
            .send(MaterializerCommand::Extension(box Iterate { sender } as _))?;
        Ok(UnboundedReceiverStream::new(receiver).boxed())
    }

    async fn refresh_ttls(&self, min_ttl: i64) -> anyhow::Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.command_sender.send(MaterializerCommand::Extension(
            box RefreshTtls { sender, min_ttl } as _,
        ))?;
        match receiver.await.context("No response from materializer")? {
            Some(task) => task
                .await
                .context("Refresh task aborted")?
                .context("Refresh failed")?,
            None => {}
        };
        Ok(())
    }

    async fn clean_stale_artifacts(
        &self,
        keep_since_time: DateTime<Utc>,
        dry_run: bool,
        tracked_only: bool,
    ) -> anyhow::Result<String> {
        let (sender, recv) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::Extension(box CleanStaleArtifacts {
                keep_since_time,
                dry_run,
                tracked_only,
                sender,
            }))?;
        recv.await?.await
    }

    fn queue_size(&self) -> usize {
        self.command_sender.counters.queue_size()
    }
}
