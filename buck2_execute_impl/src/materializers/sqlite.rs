/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use buck2_common::external_symlink::ExternalSymlink;
use buck2_common::file_ops::FileDigest;
use buck2_common::file_ops::FileMetadata;
use buck2_common::file_ops::TrackedFileDigest;
use buck2_common::sqlite::KeyValueSqliteTable;
use buck2_core::directory::DirectoryEntry;
use buck2_core::fs::fs_util;
use buck2_core::fs::paths::abs_norm_path::AbsNormPath;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::file_name::FileName;
use buck2_core::fs::project::ProjectRelativePathBuf;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::Symlink;
use buck2_execute::execute::blocking::BlockingExecutor;
use chrono::DateTime;
use chrono::TimeZone;
use chrono::Utc;
use gazebo::prelude::*;
use itertools::Itertools;
use parking_lot::Mutex;
use rusqlite::Connection;
use thiserror::Error;

use crate::materializers::deferred::ArtifactMetadata;

/// Hand-maintained schema version for the materializer state sqlite db.
/// PLEASE bump this version if you are making a breaking change to the
/// materializer state sqlite db schema! If you forget to bump this version,
/// then you can fix forward by bumping the `buck2.sqlite_materializer_state_version`
/// buckconfig in the project root's .buckconfig.
pub const DB_SCHEMA_VERSION: u64 = 1;

pub type MaterializerState = Vec<(ProjectRelativePathBuf, (ArtifactMetadata, DateTime<Utc>))>;

#[derive(Error, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactMetadataSqliteConversionError {
    #[error("Internal error: expected field `{}` to be not null for artifact type '{}'", .field, .artifact_type)]
    ExpectedNotNull {
        field: String,
        artifact_type: String,
    },

    #[error("Internal error: found unknown value '{}' for enum `artifact_type`", .0)]
    UnknownArtifactType(String),
}

/// Sqlite representation of sha1. Can be converted directly into BLOB type
/// in rusqlite. Note we use a Vec and not a fixed length array here because
/// rusqlite can only convert to Vec.
pub(crate) type SqliteSha1 = Vec<u8>;

/// Sqlite representation of `ArtifactMetadata`. All datatypes used implement
/// rusqlite's `FromSql` trait.
#[derive(Debug)]
pub(crate) struct ArtifactMetadataSqliteEntry {
    pub artifact_type: String,
    pub digest_size: Option<u64>,
    pub digest_sha1: Option<SqliteSha1>,
    pub file_is_executable: Option<bool>,
    pub symlink_target: Option<String>,
}

impl ArtifactMetadataSqliteEntry {
    pub(crate) fn new(
        artifact_type: String,
        digest_size: Option<u64>,
        digest_sha1: Option<Vec<u8>>,
        file_is_executable: Option<bool>,
        symlink_target: Option<String>,
    ) -> Self {
        Self {
            artifact_type,
            digest_size,
            digest_sha1,
            file_is_executable,
            symlink_target,
        }
    }
}

impl From<ArtifactMetadata> for ArtifactMetadataSqliteEntry {
    fn from(metadata: ArtifactMetadata) -> Self {
        fn get_size_and_sha1(digest: TrackedFileDigest) -> (u64, Vec<u8>) {
            (digest.size(), digest.sha1().to_vec())
        }

        let (artifact_type, digest_size, digest_sha1, file_is_executable, symlink_target) =
            match metadata.0 {
                DirectoryEntry::Dir(digest) => {
                    let (digest_size, digest_sha1) = get_size_and_sha1(digest);
                    (
                        "directory",
                        Some(digest_size),
                        Some(digest_sha1),
                        None,
                        None,
                    )
                }
                DirectoryEntry::Leaf(ActionDirectoryMember::File(file_metadata)) => {
                    let (digest_size, digest_sha1) = get_size_and_sha1(file_metadata.digest);
                    (
                        "file",
                        Some(digest_size),
                        Some(digest_sha1),
                        Some(file_metadata.is_executable),
                        None,
                    )
                }
                DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink)) => (
                    "symlink",
                    None,
                    None,
                    None,
                    Some(symlink.target().as_str().to_owned()),
                ),
                DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(external_symlink)) => (
                    "external_symlink",
                    None,
                    None,
                    None,
                    Some(external_symlink.target_str().to_owned()),
                ),
            };

        Self {
            artifact_type: artifact_type.to_owned(),
            digest_size,
            digest_sha1,
            file_is_executable,
            symlink_target,
        }
    }
}

impl TryFrom<ArtifactMetadataSqliteEntry> for ArtifactMetadata {
    type Error = anyhow::Error;

    fn try_from(sqlite_entry: ArtifactMetadataSqliteEntry) -> anyhow::Result<Self> {
        fn digest(
            size: Option<u64>,
            sha1: Option<Vec<u8>>,
            artifact_type: &str,
        ) -> anyhow::Result<TrackedFileDigest> {
            let size = size.ok_or_else(|| {
                anyhow::anyhow!(ArtifactMetadataSqliteConversionError::ExpectedNotNull {
                    field: "size".to_owned(),
                    artifact_type: artifact_type.to_owned()
                })
            })?;
            let sha1 = sha1.ok_or_else(|| {
                anyhow::anyhow!(ArtifactMetadataSqliteConversionError::ExpectedNotNull {
                    field: "sha1".to_owned(),
                    artifact_type: artifact_type.to_owned()
                })
            })?;

            let file_digest = FileDigest::new_sha1(
                // Converts Vec<u8> to [u8; SHA1_LENGTH]
                sha1.try_into().map_err(|v: Vec<u8>| {
                    anyhow::anyhow!(
                        "Internal error: cannot vec of bytes of len {} to a sha1",
                        v.len()
                    )
                })?,
                size,
            );
            Ok(TrackedFileDigest::new(file_digest))
        }

        let metadata = match sqlite_entry.artifact_type.as_str() {
            "directory" => DirectoryEntry::Dir(digest(
                sqlite_entry.digest_size,
                sqlite_entry.digest_sha1,
                sqlite_entry.artifact_type.as_str(),
            )?),
            "file" => DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
                digest: digest(
                    sqlite_entry.digest_size,
                    sqlite_entry.digest_sha1,
                    sqlite_entry.artifact_type.as_str(),
                )?,
                is_executable: sqlite_entry.file_is_executable.ok_or_else(|| {
                    anyhow::anyhow!(ArtifactMetadataSqliteConversionError::ExpectedNotNull {
                        field: "file_is_executable".to_owned(),
                        artifact_type: sqlite_entry.artifact_type.clone()
                    })
                })?,
            })),
            "symlink" => {
                let symlink = Symlink::new(
                    sqlite_entry
                        .symlink_target
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                ArtifactMetadataSqliteConversionError::ExpectedNotNull {
                                    field: "symlink_target".to_owned(),
                                    artifact_type: sqlite_entry.artifact_type.clone()
                                }
                            )
                        })?
                        .into(),
                );
                DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(symlink)))
            }
            "external_symlink" => {
                let external_symlink = ExternalSymlink::new(
                    sqlite_entry
                        .symlink_target
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                ArtifactMetadataSqliteConversionError::ExpectedNotNull {
                                    field: "symlink_target".to_owned(),
                                    artifact_type: sqlite_entry.artifact_type.clone()
                                }
                            )
                        })?
                        .into(),
                    None,
                )?;
                DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(Arc::new(
                    external_symlink,
                )))
            }
            artifact_type => {
                return Err(anyhow::anyhow!(
                    ArtifactMetadataSqliteConversionError::UnknownArtifactType(
                        artifact_type.to_owned()
                    )
                ));
            }
        };
        Ok(Self(metadata))
    }
}

pub(crate) struct MaterializerStateSqliteTable {
    connection: Arc<Mutex<Connection>>,
}

impl MaterializerStateSqliteTable {
    const TABLE_NAME: &'static str = "materializer_state";

    pub fn new(connection: Arc<Mutex<Connection>>) -> Self {
        Self { connection }
    }

    pub(crate) fn create_table(&self) -> anyhow::Result<()> {
        let sql = format!(
            "CREATE TABLE {} (
                path                    TEXT NOT NULL PRIMARY KEY,
                artifact_type           TEXT CHECK(artifact_type IN ('directory','file','symlink','external_symlink')) NOT NULL,
                digest_size             INTEGER NULL DEFAULT NULL,
                digest_sha1             BLOB NULL DEFAULT NULL,
                file_is_executable      INTEGER NULL DEFAULT NULL,
                symlink_target          TEXT NULL DEFAULT NULL,
                last_access_time        INTEGER NOT NULL
            )",
            Self::TABLE_NAME,
        );
        tracing::trace!(sql = %sql, "creating table");
        self.connection
            .lock()
            .execute(&sql, [])
            .with_context(|| format!("creating sqlite table {}", Self::TABLE_NAME))?;
        Ok(())
    }

    pub(crate) fn insert(
        &self,
        path: ProjectRelativePathBuf,
        metadata: ArtifactMetadata,
        timestamp: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let entry: ArtifactMetadataSqliteEntry = metadata.into();
        let sql = format!(
            "INSERT INTO {} (path, artifact_type, digest_size, digest_sha1, file_is_executable, symlink_target, last_access_time) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            Self::TABLE_NAME
        );
        tracing::trace!(sql = %sql, entry = ?entry, "inserting into table");
        self.connection
            .lock()
            .execute(
                &sql,
                rusqlite::params![
                    path.as_str(),
                    entry.artifact_type,
                    entry.digest_size,
                    entry.digest_sha1,
                    entry.file_is_executable,
                    entry.symlink_target,
                    timestamp.timestamp(),
                ],
            )
            .with_context(|| format!("inserting into sqlite table {}", Self::TABLE_NAME))?;
        Ok(())
    }

    pub(crate) fn update_access_time(
        &self,
        path: ProjectRelativePathBuf,
        timestamp: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let sql = format!(
            "UPDATE {} SET last_access_time = (?1) WHERE path = (?2)",
            Self::TABLE_NAME,
        );
        tracing::trace!(sql = %sql, now = %timestamp, "updating last_access_time");
        self.connection
            .lock()
            .execute(
                &sql,
                rusqlite::params![timestamp.timestamp(), path.as_str()],
            )
            .with_context(|| format!("updating sqlite table {}", Self::TABLE_NAME))?;
        Ok(())
    }

    pub(crate) fn read_all(&self) -> anyhow::Result<MaterializerState> {
        let sql = format!(
            "SELECT path, artifact_type, digest_size, digest_sha1, file_is_executable, symlink_target, last_access_time FROM {}",
            Self::TABLE_NAME,
        );
        tracing::trace!(sql = %sql, "reading all from table");
        let connection = self.connection.lock();
        let mut stmt = connection.prepare(&sql)?;
        let result = stmt
            .query_map(
                [],
                |row| -> rusqlite::Result<(String, ArtifactMetadataSqliteEntry, i64)> {
                    Ok((
                        row.get(0)?,
                        ArtifactMetadataSqliteEntry::new(
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ),
                        row.get(6)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("reading from sqlite table {}", Self::TABLE_NAME))?;

        result
            .into_try_map(
                |(path, entry, last_access_time)| -> anyhow::Result<(ProjectRelativePathBuf, (ArtifactMetadata, DateTime<Utc>))> {
                    let path = ProjectRelativePathBuf::unchecked_new(path);
                    let metadata: ArtifactMetadata = entry.try_into()?;
                    let timestamp = Utc.timestamp_opt(last_access_time, 0).single().with_context(|| "invalid timestamp")?;
                    Ok((path, (metadata, timestamp)))
                },
            )
            .with_context(|| format!("error reading row of sqlite table {}", Self::TABLE_NAME))
    }

    pub(crate) fn delete(&self, paths: Vec<ProjectRelativePathBuf>) -> anyhow::Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "DELETE FROM {} WHERE path IN ({})",
            Self::TABLE_NAME,
            // According to rusqlite docs this is the best way to generate the right
            // number of query placeholders.
            itertools::repeat_n("?", paths.len()).join(","),
        );
        let rows_deleted = self
            .connection
            .lock()
            .execute(
                &sql,
                rusqlite::params_from_iter(paths.iter().map(|p| p.as_str())),
            )
            .with_context(|| format!("deleting from sqlite table {}", Self::TABLE_NAME))?;
        Ok(rows_deleted)
    }
}

#[derive(Error, Debug, PartialEq, Eq)]
enum MaterializerStateSqliteDbError {
    #[error("Path {} does not exist", .0)]
    PathDoesNotExist(AbsNormPathBuf),

    #[error("Expected versions {:?}. Found versions {:?} in sqlite db at {}", .expected, .found, .path)]
    VersionMismatch {
        expected: HashMap<String, Option<String>>,
        found: HashMap<String, Option<String>>,
        path: AbsNormPathBuf,
    },
}

/// DB that opens the sqlite connection to the materializer state db on disk and
/// holds all the sqlite tables we need for storing/querying materializer state
pub struct MaterializerStateSqliteDb {
    /// Table storing actual materializer state
    materializer_state_table: MaterializerStateSqliteTable,
    /// Table for holding any metadata used to check version match. When loading
    /// from an existing db, we check if the versions from this table match the
    /// versions this buck2 binary expects. If the versions don't match, we throw
    /// away the entire db and initialize a new one. If versions do match, then
    /// we try to read all state from `materializer_state_table`.
    versions_table: KeyValueSqliteTable,
    /// Table for logging any metadata not used to check version match, generally
    /// just for debugging purposes.
    metadata_table: KeyValueSqliteTable,
}

impl MaterializerStateSqliteDb {
    /// Given path to sqlite DB, opens and returns a new connection to the DB.
    pub fn open(path: &AbsNormPath) -> anyhow::Result<Self> {
        let connection = Arc::new(Mutex::new(Connection::open(path)?));
        let materializer_state_table = MaterializerStateSqliteTable::new(connection.dupe());
        let versions_table = KeyValueSqliteTable::new("versions".to_owned(), connection.dupe());
        let metadata_table = KeyValueSqliteTable::new("metadata".to_owned(), connection);
        Ok(Self {
            materializer_state_table,
            versions_table,
            metadata_table,
        })
    }

    const DB_FILENAME: &'static str = "db.sqlite";

    /// Given path to the sqlite DB, attempts to read `MaterializerState` from the DB. If we encounter
    /// any failure along the way, such as if the DB path does not exist, the sqlite read fails,
    /// or the DB has a different set of versions than the versions this buck2 expects, we
    /// throw away the existing DB and initialize a new DB. Returns (1) the connected sqlite DB and
    /// (2) the `MaterializerState` if loading was successful or the load error.
    /// The `Result<MaterializerState>` captures any failure encountered when attempting to load
    /// from the existing DB. These failures are expected if db doesn't exist or versions don't match.
    /// The outer `Result` captures any failure encountered when trying to delete the existing DB and
    /// create a new one.
    /// TODO(scottcao): pull this method into a shared trait once we add a another sqlite DB
    pub async fn load_or_initialize(
        materializer_state_dir: AbsNormPathBuf,
        versions: HashMap<String, Option<String>>,
        // Using `BlockingExecutor` out of convenience. This function should be called during startup
        // when there's not a lot of I/O so it shouldn't matter.
        io_executor: Arc<dyn BlockingExecutor>,
    ) -> anyhow::Result<(Self, anyhow::Result<MaterializerState>)> {
        io_executor
            .execute_io_inline(|| Self::load_or_initialize_impl(materializer_state_dir, versions))
            .await
    }

    pub fn load_or_initialize_impl(
        materializer_state_dir: AbsNormPathBuf,
        versions: HashMap<String, Option<String>>,
    ) -> anyhow::Result<(Self, anyhow::Result<MaterializerState>)> {
        let db_path = materializer_state_dir.join(FileName::unchecked_new(Self::DB_FILENAME));

        let result: anyhow::Result<(Self, MaterializerState)> = try {
            // try reading the existing db, if it exists.
            if !db_path.exists() {
                Err(anyhow::anyhow!(
                    MaterializerStateSqliteDbError::PathDoesNotExist(db_path.clone())
                ))?
            }

            let db = Self::open(&db_path)?;

            // First check that versions match
            let read_versions = db.versions_table.read_all()?;
            if read_versions != versions {
                Err(MaterializerStateSqliteDbError::VersionMismatch {
                    expected: versions.clone(),
                    found: read_versions,
                    path: db_path.clone(),
                })?;
            }

            let state = db.materializer_state_table().read_all()?;
            (db, state)
        };
        match result {
            Ok((db, state)) => Ok((db, Ok(state))),
            Err(e) => {
                // Loading failed. Initialize a new db from scratch.

                // Delete the existing materializer_state directory and create a new one.
                // We delete the entire directory and not just the db file because sqlite
                // can leave behind other files.
                if materializer_state_dir.exists() {
                    fs_util::remove_dir_all(&materializer_state_dir)?;
                }
                fs_util::create_dir_all(&materializer_state_dir)?;

                // Initialize a new db
                let db = Self::open(&db_path)?;
                db.create_all_tables()?;
                for (key, value) in versions.into_iter() {
                    db.versions_table.insert(key, value)?;
                }

                Ok((db, Err(e)))
            }
        }
    }

    pub(crate) fn materializer_state_table(&self) -> &MaterializerStateSqliteTable {
        &self.materializer_state_table
    }

    pub(crate) fn create_all_tables(&self) -> anyhow::Result<()> {
        self.materializer_state_table.create_table()?;
        self.versions_table.create_table()?;
        self.metadata_table.create_table()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use assert_matches::assert_matches;
    use buck2_common::file_ops::FileDigest;
    use buck2_common::file_ops::FileMetadata;
    use buck2_common::file_ops::TrackedFileDigest;
    use buck2_core::directory::DirectoryEntry;
    use buck2_core::fs::project::ProjectRelativePath;
    use buck2_core::fs::project::ProjectRoot;
    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_execute::directory::new_symlink;
    use buck2_execute::directory::ActionDirectoryMember;

    use super::*;

    #[test]
    fn test_artifact_metadata_dir_sqlite_entry_conversion_succeeds() {
        let digest = TrackedFileDigest::new(FileDigest::from_bytes_sha1(b"directory"));
        let metadata = ArtifactMetadata(DirectoryEntry::Dir(digest));
        let entry: ArtifactMetadataSqliteEntry = metadata.dupe().into();
        assert_eq!(metadata, entry.try_into().unwrap());
    }

    #[test]
    fn test_artifact_metadata_file_sqlite_entry_conversion_succeeds() {
        let digest = TrackedFileDigest::new(FileDigest::from_bytes_sha1(b"file"));
        let metadata = ArtifactMetadata(DirectoryEntry::Leaf(ActionDirectoryMember::File(
            FileMetadata {
                digest,
                is_executable: false,
            },
        )));
        let entry: ArtifactMetadataSqliteEntry = metadata.dupe().into();
        assert_eq!(metadata, entry.try_into().unwrap());
    }

    #[test]
    fn test_artifact_metadata_symlink_sqlite_entry_conversion_succeeds() {
        let symlink = new_symlink("foo/bar").unwrap();
        // We use `new_symlink` as a helper but it can technically create both Symlink and ExternalSymlink.
        // Verify that we have a Symlink here.
        assert_matches!(symlink, ActionDirectoryMember::Symlink(..));

        let metadata = ArtifactMetadata(DirectoryEntry::Leaf(symlink));
        let entry: ArtifactMetadataSqliteEntry = metadata.dupe().into();
        assert_eq!(metadata, entry.try_into().unwrap());
    }

    #[test]
    fn test_artifact_metadata_external_symlink_sqlite_entry_conversion_succeeds() {
        let external_symlink = new_symlink(if cfg!(windows) {
            // Not sure if we actually support any external symlink on windows, but better
            // to just check anyways.
            "C:\\external\\symlink\\to\\artifact"
        } else {
            "/mnt/gvfs/third-party2/zstd/28def025ee38919d509596da7d09e7a5262cbf32/1.4.x/platform010/64091f4/share"
        }).unwrap();
        // We use `new_symlink` as a helper but it can technically create both Symlink and ExternalSymlink.
        // Verify that we have an ExternalSymlink here.
        assert_matches!(external_symlink, ActionDirectoryMember::ExternalSymlink(..));

        let metadata = ArtifactMetadata(DirectoryEntry::Leaf(external_symlink));
        let entry: ArtifactMetadataSqliteEntry = metadata.dupe().into();
        assert_eq!(metadata, entry.try_into().unwrap());
    }

    fn now_seconds() -> DateTime<Utc> {
        Utc.timestamp_opt(Utc::now().timestamp(), 0)
            .single()
            .unwrap()
    }

    #[test]
    fn test_materializer_state_sqlite_table() {
        let fs = ProjectRootTemp::new().unwrap();
        let connection = Connection::open(
            fs.path()
                .resolve(ProjectRelativePath::unchecked_new("test.db")),
        )
        .unwrap();
        let table = MaterializerStateSqliteTable::new(Arc::new(Mutex::new(connection)));

        table.create_table().unwrap();

        let dir_fingerprint = TrackedFileDigest::new(FileDigest::from_bytes_sha1(b"directory"));
        let file = ActionDirectoryMember::File(FileMetadata {
            digest: TrackedFileDigest::new(FileDigest::from_bytes_sha1(b"file")),
            is_executable: false,
        });
        let symlink = new_symlink("foo/bar").unwrap();
        let external_symlink = new_symlink(if cfg!(windows) {
            // Not sure if we actually support any external symlink on windows, but better
            // to just check anyways.
            "C:\\external\\symlink\\to\\artifact"
        } else {
            "/mnt/gvfs/third-party2/zstd/28def025ee38919d509596da7d09e7a5262cbf32/1.4.x/platform010/64091f4/share"
        }).unwrap();
        // We use `new_symlink` as a helper but it can technically create both Symlink and ExternalSymlink.
        // Verify that we have proper symlink and external symlink.
        assert_matches!(symlink, ActionDirectoryMember::Symlink(..));
        assert_matches!(external_symlink, ActionDirectoryMember::ExternalSymlink(..));

        let mut artifacts = HashMap::from([
            (
                ProjectRelativePath::unchecked_new("a").to_owned(),
                (
                    ArtifactMetadata(DirectoryEntry::Dir(dir_fingerprint)),
                    now_seconds(),
                ),
            ),
            (
                ProjectRelativePath::unchecked_new("b/c").to_owned(),
                (ArtifactMetadata(DirectoryEntry::Leaf(file)), now_seconds()),
            ),
            (
                ProjectRelativePath::unchecked_new("d").to_owned(),
                (
                    ArtifactMetadata(DirectoryEntry::Leaf(symlink)),
                    now_seconds(),
                ),
            ),
            (
                ProjectRelativePath::unchecked_new("e").to_owned(),
                (
                    ArtifactMetadata(DirectoryEntry::Leaf(external_symlink)),
                    now_seconds(),
                ),
            ),
        ]);

        for (path, metadata) in artifacts.iter() {
            table
                .insert(path.to_owned(), metadata.0.clone(), metadata.1)
                .unwrap();
        }

        let state = table.read_all().unwrap();
        assert_eq!(artifacts, state.into_iter().collect::<HashMap<_, _>>());

        let paths_to_remove = vec![
            ProjectRelativePath::unchecked_new("d").to_owned(),
            ProjectRelativePath::unchecked_new("doesnt/exist").to_owned(),
        ];
        let rows_deleted = table.delete(paths_to_remove.clone()).unwrap();
        assert_eq!(rows_deleted, 1);

        for path in paths_to_remove.iter() {
            artifacts.remove(path);
        }
        let state = table.read_all().unwrap();
        assert_eq!(artifacts, state.into_iter().collect::<HashMap<_, _>>());
    }

    fn testing_materializer_state_sqlite_db(
        fs: &ProjectRoot,
        versions: HashMap<String, Option<String>>,
    ) -> anyhow::Result<(MaterializerStateSqliteDb, anyhow::Result<MaterializerState>)> {
        MaterializerStateSqliteDb::load_or_initialize_impl(
            fs.resolve(ProjectRelativePath::unchecked_new(
                "buck-out/v2/cache/materializer_state",
            )),
            versions,
        )
    }

    #[test]
    fn test_load_or_initialize_sqlite_db() -> anyhow::Result<()> {
        let fs = ProjectRootTemp::new()?;

        let path = ProjectRelativePath::unchecked_new("foo").to_owned();
        let artifact_metadata = ArtifactMetadata(DirectoryEntry::Dir(TrackedFileDigest::new(
            FileDigest::from_bytes_sha1(b"directory"),
        )));
        let timestamp = now_seconds();
        {
            let (db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                HashMap::from([("version".to_owned(), Some("0".to_owned()))]),
            )
            .unwrap();
            assert_matches!(
                loaded_state,
                Err(e) => {
                    assert_matches!(
                        e.downcast_ref::<MaterializerStateSqliteDbError>(),
                        Some(MaterializerStateSqliteDbError::PathDoesNotExist(_path)));
                }
            );

            db.materializer_state_table()
                .insert(path.clone(), artifact_metadata.clone(), timestamp)
                .unwrap();
        }

        {
            let (_db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                HashMap::from([("version".to_owned(), Some("0".to_owned()))]),
            )
            .unwrap();
            assert_matches!(
                loaded_state,
                Ok(v) => {
                    assert_eq!(v, vec![(path.clone(), (artifact_metadata.clone(), timestamp))]);
                }
            );
        }

        {
            let (db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                HashMap::from([("version".to_owned(), Some("1".to_owned()))]),
            )
            .unwrap();
            assert_matches!(
                loaded_state,
                Err(e) => {
                    assert_matches!(
                        e.downcast_ref::<MaterializerStateSqliteDbError>(),
                        Some(MaterializerStateSqliteDbError::VersionMismatch {
                            expected: _expected,
                            found: _found,
                            path: _path,
                    }));
                }
            );

            db.materializer_state_table()
                .insert(path.clone(), artifact_metadata.clone(), timestamp)
                .unwrap();
        }

        {
            let (_db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                HashMap::from([("version".to_owned(), Some("1".to_owned()))]),
            )
            .unwrap();
            assert_matches!(
                loaded_state,
                Ok(v) => {
                    assert_eq!(v, vec![(path, (artifact_metadata, timestamp))]);
                }
            );
        }

        Ok(())
    }
}
