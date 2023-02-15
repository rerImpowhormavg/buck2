/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt;

use buck2_common::cas_digest::CasDigest;
use buck2_common::cas_digest::CasDigestParseError;
use buck2_common::cas_digest::TrackedCasDigest;
use buck2_common::digest_config::DigestConfig;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DigestConversionError {
    #[error("Error parsing digest: `{}`", digest)]
    ParseError {
        digest: String,

        #[source]
        error: CasDigestParseError,
    },
}

pub type ReDigest = remote_execution::TDigest;

pub type GrpcDigest = remote_execution::Digest;

pub trait CasDigestFromReExt: Sized {
    fn from_re(x: &ReDigest, cfg: DigestConfig) -> Result<Self, DigestConversionError>;
    fn from_grpc(x: &GrpcDigest, cfg: DigestConfig) -> Result<Self, DigestConversionError>;
}

pub trait CasDigestToReExt {
    fn to_re(&self) -> ReDigest;
    fn to_grpc(&self) -> GrpcDigest;
}

impl<Kind> CasDigestFromReExt for CasDigest<Kind> {
    fn from_re(digest: &ReDigest, _cfg: DigestConfig) -> Result<Self, DigestConversionError> {
        Ok(Self::new_sha1(
            Self::parse_digest_sha1_without_size(digest.hash.as_bytes()).map_err(|error| {
                DigestConversionError::ParseError {
                    digest: digest.to_string(),
                    error,
                }
            })?,
            digest.size_in_bytes as u64,
        ))
    }

    fn from_grpc(digest: &GrpcDigest, _cfg: DigestConfig) -> Result<Self, DigestConversionError> {
        Ok(Self::new_sha1(
            Self::parse_digest_sha1_without_size(digest.hash.as_bytes()).map_err(|error| {
                DigestConversionError::ParseError {
                    digest: format!("{}:{}", digest.hash, digest.size_bytes),
                    error,
                }
            })?,
            digest.size_bytes as u64,
        ))
    }
}

pub trait CasDigestConversionResultExt {
    fn as_display(&self) -> &dyn fmt::Display;
}

impl<Kind> CasDigestConversionResultExt for Result<CasDigest<Kind>, DigestConversionError> {
    fn as_display(&self) -> &dyn fmt::Display {
        match self {
            Self::Ok(ref v) => v as _,
            Self::Err(DigestConversionError::ParseError { ref digest, .. }) => digest as _,
        }
    }
}

impl<Kind> CasDigestToReExt for TrackedCasDigest<Kind> {
    fn to_re(&self) -> ReDigest {
        self.data().to_re()
    }

    fn to_grpc(&self) -> GrpcDigest {
        self.data().to_grpc()
    }
}

impl<Kind> CasDigestToReExt for CasDigest<Kind> {
    fn to_re(&self) -> ReDigest {
        ReDigest {
            hash: hex::encode(self.digest()),
            size_in_bytes: self.size() as i64,
            ..Default::default()
        }
    }

    fn to_grpc(&self) -> GrpcDigest {
        GrpcDigest {
            hash: hex::encode(self.digest()),
            size_bytes: self.size() as i64,
        }
    }
}

pub trait FileDigestFromProtoExt {
    fn from_proto_message<M: prost::Message>(m: &M) -> Self;
}

impl<Kind> FileDigestFromProtoExt for CasDigest<Kind> {
    fn from_proto_message<M: prost::Message>(m: &M) -> Self {
        let mut m_encoded = Vec::new();
        m.encode(&mut m_encoded)
            .unwrap_or_else(|e| unreachable!("Protobuf messages are always encodeable: {}", e));
        Self::from_bytes_sha1(m_encoded.as_slice())
    }
}
