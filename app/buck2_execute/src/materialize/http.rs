/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use allocative::Allocative;
use anyhow::Context as _;
use buck2_common::file_ops::FileDigest;
use buck2_common::file_ops::TrackedFileDigest;
use buck2_core::fs::fs_util;
use buck2_core::fs::project::ProjectRelativePath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::is_open_source;
use dupe::Dupe;
use futures::future::Future;
use futures::StreamExt;
use reqwest::Client;
use reqwest::RequestBuilder;
use reqwest::Response;
use reqwest::StatusCode;
use sha1::Digest;
use sha1::Sha1;
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Clone, Dupe, Allocative)]
pub enum Checksum {
    Sha1(Arc<str>),
    Sha256(Arc<str>),
    Both { sha1: Arc<str>, sha256: Arc<str> },
}

impl Checksum {
    pub fn sha1(&self) -> Option<&str> {
        match self {
            Self::Sha1(sha1) => Some(sha1),
            Self::Sha256(..) => None,
            Self::Both { sha1, .. } => Some(sha1),
        }
    }

    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::Sha1(..) => None,
            Self::Sha256(sha256) => Some(sha256),
            Self::Both { sha256, .. } => Some(sha256),
        }
    }
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error(
        "HTTP {} Error ({}) when querying URL: {}. Response text: {}",
        http_error_label(*.status),
        .status,
        .url,
        .text
    )]
    HttpErrorStatus {
        status: StatusCode,
        url: String,
        text: String,
    },

    #[error(
        "HTTP Transfer Error when querying URL: {}. Failed before receiving headers.",
        .url,
    )]
    HttpHeadersTransferError {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error(
        "HTTP Transfer Error when querying URL: {}. Failed after {} bytes",
        .url,
        .received
    )]
    HttpTransferError {
        received: u64,
        url: String,
        #[source]
        source: reqwest::Error,
    },
}

impl HttpError {
    /// Decide whether to retry this HTTP error. If we got a response but the server errored or
    /// told us to come back later, we retry. If we didn't get a response, then we retry only if we
    /// suceeded in connecting (so as to ensure we don't waste time retrying when the domain
    /// portion of the URL is just wrong or when we don't have the right TLS credentials).
    ///
    /// NOTE: not retrying *any* connect errors may not be ideal, but we dont get access to more
    /// detail with Reqwest. To fix this we should migrate to raw Hyper (which probably wouldn't be
    /// a bad idea anyway).
    fn is_retryable(&self) -> bool {
        match self {
            Self::HttpErrorStatus { status, .. } => {
                status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS
            }
            Self::HttpHeadersTransferError { source, .. }
            | Self::HttpTransferError { source, .. } => !source.is_connect(),
        }
    }
}

fn http_error_label(status: StatusCode) -> &'static str {
    if status.is_server_error() {
        return "Server";
    }

    if status.is_client_error() {
        return "Client";
    }

    "Unknown"
}

#[derive(Debug, Error)]
enum HttpHeadError {
    #[error("Error performing a http_head request")]
    HttpError(#[from] HttpError),
}

#[derive(Debug, Error)]
enum HttpDownloadError {
    #[error("Error performing a http_download request")]
    HttpError(#[from] HttpError),

    #[error("Invalid {0} digest. Expected {1}, got {2}. URL: {3}")]
    InvalidChecksum(&'static str, String, String, String),

    #[error(transparent)]
    IoError(anyhow::Error),
}

trait AsHttpError {
    fn as_http_error(&self) -> Option<&HttpError>;
}

impl AsHttpError for HttpHeadError {
    fn as_http_error(&self) -> Option<&HttpError> {
        match self {
            Self::HttpError(e) => Some(e),
        }
    }
}

impl AsHttpError for HttpDownloadError {
    fn as_http_error(&self) -> Option<&HttpError> {
        match self {
            Self::HttpError(e) => Some(e),
            Self::InvalidChecksum(..) | Self::IoError(..) => None,
        }
    }
}

pub fn http_client() -> anyhow::Result<Client> {
    let mut builder = Client::builder();

    if !is_open_source() {
        // Buck v1 doesn't honor the `$HTTPS_PROXY` variables. That is useful because
        // we don't want internal users fetching from the web while building,
        // and some machines might have them misconfigured.
        //
        // However, for open source, we definitely want to support proxies properly.
        builder = builder.no_proxy();
    }

    builder.build().context("Error creating http client")
}

async fn http_dispatch(req: RequestBuilder, url: &str) -> Result<Response, HttpError> {
    let response = req
        .send()
        .await
        .map_err(|source| HttpError::HttpHeadersTransferError {
            url: url.to_owned(),
            source,
        })?;

    let status = response.status();

    if !status.is_success() {
        let text = match response.text().await {
            Ok(t) => t,
            Err(e) => format!("Error decoding response text: {}", e),
        };

        return Err(HttpError::HttpErrorStatus {
            status,
            url: url.to_owned(),
            text,
        });
    }

    Ok(response)
}

pub async fn http_head(client: &Client, url: &str) -> anyhow::Result<Response> {
    Ok(http_retry(|| async {
        let response = http_dispatch(client.head(url), url).await?;
        Result::<_, HttpHeadError>::Ok(response)
    })
    .await?)
}

pub async fn http_download(
    client: &Client,
    fs: &ProjectRoot,
    path: &ProjectRelativePath,
    url: &str,
    checksum: &Checksum,
    executable: bool,
) -> anyhow::Result<TrackedFileDigest> {
    let abs_path = fs.resolve(path);
    if let Some(dir) = abs_path.parent() {
        fs_util::create_dir_all(fs.resolve(dir))?;
    }

    Ok(http_retry(|| async {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.to_string())
            .with_context(|| format!("open({})", abs_path))
            .map_err(HttpDownloadError::IoError)?;

        let response = http_dispatch(client.get(url), url).await?;

        let mut stream = response.bytes_stream();
        let mut buf_writer = std::io::BufWriter::new(file);

        // We always build a SHA1 hash, as it'll be used for the file digest. We optionally build a
        // sha256 hasher if a sha256 hash was provided for validation.
        let mut sha1_hasher = Sha1::new();
        let mut sha256_hasher_and_expected =
            checksum.sha256().map(|sha256| (Sha256::new(), sha256));

        let mut file_len = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| HttpError::HttpTransferError {
                received: file_len,
                url: url.to_owned(),
                source,
            })?;
            buf_writer
                .write(&chunk)
                .with_context(|| format!("write({})", abs_path))
                .map_err(HttpDownloadError::IoError)?;
            sha1_hasher.update(&chunk);
            if let Some((sha256_hasher, ..)) = &mut sha256_hasher_and_expected {
                sha256_hasher.update(&chunk);
            }
            file_len += chunk.len() as u64;
        }
        buf_writer
            .flush()
            .with_context(|| format!("flush({})", abs_path))
            .map_err(HttpDownloadError::IoError)?;

        // Form the SHA1, and verify any fingerprints that were provided. Note that, by construction,
        // we always require at least one, since one can't construct a Checksum that has neither SHA1
        // nor SHA256
        let download_sha1 = hex::encode(sha1_hasher.finalize().as_slice());

        if let Some(expected_sha1) = checksum.sha1() {
            if expected_sha1 != download_sha1 {
                return Err(HttpDownloadError::InvalidChecksum(
                    "sha1",
                    expected_sha1.to_owned(),
                    download_sha1,
                    url.to_owned(),
                ));
            }
        }

        if let Some((sha256_hasher, expected_sha256)) = sha256_hasher_and_expected {
            let download_sha256 = hex::encode(sha256_hasher.finalize().as_slice());
            if expected_sha256 != download_sha256 {
                return Err(HttpDownloadError::InvalidChecksum(
                    "sha256",
                    expected_sha256.to_owned(),
                    download_sha256,
                    url.to_owned(),
                ));
            }
        }

        if executable {
            fs.set_executable(path)
                .map_err(HttpDownloadError::IoError)?;
        }

        Result::<_, HttpDownloadError>::Ok(TrackedFileDigest::new(FileDigest::new_sha1(
            FileDigest::parse_digest_sha1_without_size(download_sha1.as_bytes()).unwrap(),
            file_len,
        )))
    })
    .await?)
}

async fn http_retry<Exec, F, T, E>(exec: Exec) -> Result<T, E>
where
    Exec: Fn() -> F,
    E: AsHttpError + std::fmt::Display,
    F: Future<Output = Result<T, E>>,
{
    let mut backoff = [0, 2, 4, 8].into_iter().peekable();

    while let Some(duration) = backoff.next() {
        tokio::time::sleep(Duration::from_secs(duration)).await;

        let res = exec().await;

        let http_error = res.as_ref().err().and_then(|err| err.as_http_error());

        if let Some(http_error) = http_error {
            if http_error.is_retryable() {
                if let Some(b) = backoff.peek() {
                    tracing::warn!(
                        "Retrying a HTTP error after {} seconds: {:#}",
                        b,
                        http_error
                    );
                    continue;
                }
            }
        }

        return res;
    }

    unreachable!("The loop above will exit before we get to the end")
}
