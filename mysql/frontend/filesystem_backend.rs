//! Unix capability backend for [`super::RegistryRoot`].
//!
//! The constructor is the only operation that accepts a path. It immediately
//! opens that path as a directory and all later operations use names relative
//! to the retained descriptor.

use super::*;
use serde::de::DeserializeOwned;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[path = "filesystem_backend/database_metadata.rs"]
mod database_metadata;

use database_metadata::{DatabaseMetadata, MetadataArtifactRole};
use turso_core::DatabaseFileOwner;

const MANIFEST_FILE: &str = ".turso-mysql-root.json";
const REGISTRY_FILE: &str = ".turso-mysql-registry.json";
const LOCK_FILE: &str = ".turso-mysql-root.lock";
const MAX_MANIFEST_BYTES: usize = 4096;
const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const SQLITE_HEADER_BYTES: usize = 100;
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const WAL_SUFFIX: &str = "-wal";
const MAIN_INFO_SUFFIX: &str = ".turso-mysql-main-info";
const WAL_INFO_SUFFIX: &str = ".turso-mysql-wal-info";
const PRIVATE_TEMP_MAX_AGE: Duration = Duration::from_secs(60 * 60);

const PRIVATE_TEMP_PREFIXES: [(&[u8], PrivateTemporaryKind); 5] = [
    (
        b".turso-mysql-registry.tmp.",
        PrivateTemporaryKind::Registry,
    ),
    (
        b".turso-mysql-database-main.tmp.",
        PrivateTemporaryKind::DatabaseMain,
    ),
    (
        b".turso-mysql-database-wal.tmp.",
        PrivateTemporaryKind::DatabaseWal,
    ),
    (
        b".turso-mysql-database-main-info.tmp.",
        PrivateTemporaryKind::DatabaseMainInfo,
    ),
    (
        b".turso-mysql-database-wal-info.tmp.",
        PrivateTemporaryKind::DatabaseWalInfo,
    ),
];

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

/// A retained directory capability. No child operation reinterprets a path.
pub(crate) struct OsDataRoot {
    directory: File,
    #[cfg(test)]
    private_temporary_cleanup_test_hook: Option<PrivateTemporaryCleanupTestHook>,
    #[cfg(test)]
    stage_creation_failure_test_hook: Option<StageCreationFailureTestHook>,
    #[cfg(test)]
    publish_database_stage_test_hook: Option<PublishDatabaseStageTestHook>,
    #[cfg(test)]
    database_artifact_operation_failure_test_hook: Option<DatabaseArtifactOperationFailureTestHook>,
}

#[cfg(test)]
mod four_artifact_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn expected(key: &str) -> DatabaseFileExpectation {
        DatabaseFileExpectation::new(
            OpaqueFileKey::new(key.to_owned()).unwrap(),
            MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1),
        )
    }

    pub(super) fn initialize_main(stage: &OsDatabaseStage) -> Result<(), RegistryError> {
        let mut page = [0; 4096];
        page[..SQLITE_MAGIC.len()].copy_from_slice(SQLITE_MAGIC);
        page[16..18].copy_from_slice(&(4096u16).to_be_bytes());
        page[18] = 1;
        page[19] = 1;
        page[21] = 64;
        page[22] = 32;
        page[23] = 32;
        page[68..72].copy_from_slice(
            &(DatabaseFileOwner::mysql_application_id(
                DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
            ) as u32)
                .to_be_bytes(),
        );
        stage
            .main_file()?
            .write_at(&page, 0)
            .map_err(|_| RegistryError::Backend)?;
        Ok(())
    }

    fn create_database_new(
        root: &mut OsDataRoot,
        expected: &DatabaseFileExpectation,
    ) -> Result<(), RegistryError> {
        let stage = root.stage_database_new(expected)?;
        initialize_main(&stage)?;
        root.sync_database_stage(&stage)?;
        root.publish_database_stage_new(expected, stage)?;
        root.fsync_dir()
    }

    fn stale_private_file(directory: &std::path::Path, name: &str) {
        let file = File::create(directory.join(name)).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
    }

    fn valid_private_names() -> [&'static str; 5] {
        [
            ".turso-mysql-registry.tmp.123.1.0123456789abcdef0123456789abcdef",
            ".turso-mysql-database-main.tmp.123.2.0123456789abcdef0123456789abcdef",
            ".turso-mysql-database-wal.tmp.123.3.0123456789abcdef0123456789abcdef",
            ".turso-mysql-database-main-info.tmp.123.4.0123456789abcdef0123456789abcdef",
            ".turso-mysql-database-wal-info.tmp.123.5.0123456789abcdef0123456789abcdef",
        ]
    }

    fn create_fifo(path: &std::path::Path) {
        let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `path` is NUL-terminated and points to a fresh test name.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn root_requires_mode_0700_and_rejects_symlinks() {
        let directory = private_tempdir();
        assert!(OsDataRoot::open(directory.path()).is_ok());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            OsDataRoot::open(directory.path()),
            Err(RegistryError::Backend)
        ));

        let parent = private_tempdir();
        let link = parent.path().join("root-link");
        symlink(directory.path(), &link).unwrap();
        assert!(matches!(
            OsDataRoot::open(&link),
            Err(RegistryError::Backend)
        ));
    }

    #[test]
    fn root_lock_is_exclusive_across_independent_capabilities() {
        let directory = private_tempdir();
        let mut first = OsDataRoot::open(directory.path()).unwrap();
        let mut second = OsDataRoot::open(directory.path()).unwrap();
        let _first_lock = first.acquire_exclusive_registry_lock().unwrap();
        assert!(matches!(
            second.acquire_exclusive_registry_lock(),
            Err(RegistryError::RegistryAlreadyOpen)
        ));
    }

    #[test]
    fn one_sided_root_markers_fail_closed() {
        let manifest_only = private_tempdir();
        let mut root = OsDataRoot::open(manifest_only.path()).unwrap();
        root.create_manifest_new(&RootManifest::lower_case_table_names_1())
            .unwrap();
        root.fsync_dir().unwrap();
        drop(root);
        assert!(matches!(
            DatabaseRegistry::open_or_create(OsDataRoot::open(manifest_only.path()).unwrap()),
            Err(RegistryError::Backend)
        ));

        let registry_only = private_tempdir();
        let mut root = OsDataRoot::open(registry_only.path()).unwrap();
        root.replace_registry(&RegistrySnapshot::default()).unwrap();
        drop(root);
        assert!(matches!(
            DatabaseRegistry::open_or_create(OsDataRoot::open(registry_only.path()).unwrap()),
            Err(RegistryError::Backend)
        ));
    }

    #[test]
    fn registry_replacement_replaces_a_symlink_without_following_it() {
        let directory = private_tempdir();
        let outside = private_tempdir();
        let target = outside.path().join("registry-target");
        fs::write(&target, b"outside").unwrap();
        symlink(&target, directory.path().join(REGISTRY_FILE)).unwrap();

        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let snapshot = RegistrySnapshot::default();
        root.replace_registry(&snapshot).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"outside");
        assert!(!fs::symlink_metadata(directory.path().join(REGISTRY_FILE))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(root.read_registry(), Ok(Some(snapshot)));
    }

    #[test]
    fn fifo_artifacts_are_nonblocking_mismatches() {
        for artifact in [DatabaseArtifact::Main, DatabaseArtifact::MainInfo] {
            let directory = private_tempdir();
            let mut root = OsDataRoot::open(directory.path()).unwrap();
            let entry = expected("db_00000000000000000000000000000011");
            create_fifo(
                &directory
                    .path()
                    .join(OsDataRoot::artifact_name(&entry, artifact)),
            );
            assert_eq!(
                root.inspect_database(&entry),
                Ok(DatabaseFileInspection::Mismatch),
                "{artifact:?}"
            );
        }
    }

    #[test]
    fn stages_and_publishes_four_real_artifacts() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000001");
        create_database_new(&mut root, &entry).unwrap();

        for artifact in DatabaseArtifact::ALL {
            assert!(directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, artifact))
                .is_file());
        }
        let main = fs::read(directory.path().join(entry.file_key().as_str())).unwrap();
        assert!(main.len() > SQLITE_HEADER_BYTES);
        assert_eq!(&main[..SQLITE_MAGIC.len()], SQLITE_MAGIC);
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Matching)
        );
    }

    #[test]
    fn sync_requires_core_to_initialize_the_main_header() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000002");
        let stage = root.stage_database_new(&entry).unwrap();
        assert_eq!(
            root.sync_database_stage(&stage),
            Err(RegistryError::Backend)
        );
        initialize_main(&stage).unwrap();
        root.sync_database_stage(&stage).unwrap();
    }

    #[test]
    fn sidecar_sync_failure_keeps_creating_for_safe_reopen_recovery() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.publish_database_stage_test_hook = Some(PublishDatabaseStageTestHook {
            fail_sidecar_sync: true,
            sidecar_sync_attempts: 0,
        });
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();

        assert_eq!(
            registry.create_with_initializer("sidecar_sync_failure", |stage, _, lifetime| {
                initialize_main(stage)?;
                drop(lifetime);
                Ok(())
            }),
            Err(RegistryError::Backend)
        );

        let name = DatabaseName::parse("sidecar_sync_failure").unwrap();
        let entry = expected(registry.snapshot.entries[&name].file_key.as_str());
        assert_eq!(
            registry.snapshot.entries[&name].state,
            DatabaseState::Creating
        );
        assert_eq!(
            registry
                .root
                .publish_database_stage_test_hook
                .as_ref()
                .unwrap()
                .sidecar_sync_attempts,
            1
        );
        for artifact in [DatabaseArtifact::Main, DatabaseArtifact::Wal] {
            assert!(!directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, artifact))
                .exists());
        }
        for artifact in [DatabaseArtifact::MainInfo, DatabaseArtifact::WalInfo] {
            assert!(directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, artifact))
                .is_file());
        }

        drop(registry);
        let registry =
            DatabaseRegistry::open_or_create(OsDataRoot::open(directory.path()).unwrap()).unwrap();
        assert!(!registry.contains(name.as_str()).unwrap());
        drop(registry);

        let mut root = OsDataRoot::open(directory.path()).unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Missing)
        );
    }

    #[test]
    fn publish_operation_failures_keep_creating_for_safe_reopen_recovery() {
        for (operation, fail_at_attempt, expected_final_artifacts) in [
            (
                DatabaseArtifactOperation::PublishLink,
                1,
                &[] as &[DatabaseArtifact],
            ),
            (
                DatabaseArtifactOperation::PublishLink,
                3,
                &[DatabaseArtifact::MainInfo, DatabaseArtifact::WalInfo],
            ),
            (
                DatabaseArtifactOperation::DirectorySync,
                2,
                &[DatabaseArtifact::MainInfo, DatabaseArtifact::WalInfo],
            ),
            (
                DatabaseArtifactOperation::DirectorySync,
                3,
                &DatabaseArtifact::ALL,
            ),
        ] {
            let directory = private_tempdir();
            let mut registry =
                DatabaseRegistry::open_or_create(OsDataRoot::open(directory.path()).unwrap())
                    .unwrap();
            registry.root.database_artifact_operation_failure_test_hook =
                Some(DatabaseArtifactOperationFailureTestHook {
                    operation,
                    fail_at_attempt,
                    attempts: 0,
                });

            assert_eq!(
                registry.create_with_initializer("publish_failure", |stage, _, lifetime| {
                    initialize_main(stage)?;
                    drop(lifetime);
                    Ok(())
                }),
                Err(RegistryError::Backend),
                "{operation:?} attempt {fail_at_attempt}"
            );

            let name = DatabaseName::parse("publish_failure").unwrap();
            let entry = expected(registry.snapshot.entries[&name].file_key.as_str());
            assert_eq!(
                registry.snapshot.entries[&name].state,
                DatabaseState::Creating
            );
            for artifact in DatabaseArtifact::ALL {
                assert_eq!(
                    directory
                        .path()
                        .join(OsDataRoot::artifact_name(&entry, artifact))
                        .exists(),
                    expected_final_artifacts.contains(&artifact),
                    "{operation:?} attempt {fail_at_attempt}: {artifact:?}"
                );
            }

            drop(registry);
            let registry =
                DatabaseRegistry::open_or_create(OsDataRoot::open(directory.path()).unwrap())
                    .unwrap();
            assert!(!registry.contains(name.as_str()).unwrap());
            drop(registry);

            let mut root = OsDataRoot::open(directory.path()).unwrap();
            assert_eq!(
                root.inspect_database(&entry),
                Ok(DatabaseFileInspection::Missing),
                "{operation:?} attempt {fail_at_attempt}"
            );
        }
    }

    #[test]
    fn drop_operation_failures_keep_dropping_for_safe_reopen_recovery() {
        for (operation, fail_at_attempt, main_is_final, main_is_tombstoned) in [
            (DatabaseArtifactOperation::DropRename, 1, true, false),
            (DatabaseArtifactOperation::DirectorySync, 2, false, true),
            (DatabaseArtifactOperation::DropUnlink, 1, false, true),
            (DatabaseArtifactOperation::DirectorySync, 3, false, false),
        ] {
            let directory = private_tempdir();
            let mut registry =
                DatabaseRegistry::open_or_create(OsDataRoot::open(directory.path()).unwrap())
                    .unwrap();
            registry
                .create_with_initializer("drop_failure", |stage, _, lifetime| {
                    initialize_main(stage)?;
                    drop(lifetime);
                    Ok(())
                })
                .unwrap();
            let name = DatabaseName::parse("drop_failure").unwrap();
            let entry = expected(registry.snapshot.entries[&name].file_key.as_str());
            registry.root.database_artifact_operation_failure_test_hook =
                Some(DatabaseArtifactOperationFailureTestHook {
                    operation,
                    fail_at_attempt,
                    attempts: 0,
                });

            assert_eq!(
                registry.drop_database(name.as_str()),
                Err(RegistryError::Backend),
                "{operation:?} attempt {fail_at_attempt}"
            );
            assert_eq!(
                registry.snapshot.entries[&name].state,
                DatabaseState::Dropping
            );
            let main = OsDataRoot::artifact_name(&entry, DatabaseArtifact::Main);
            let main_tombstone =
                OsDataRoot::artifact_tombstone_name(&entry, DatabaseArtifact::Main);
            assert_eq!(directory.path().join(main).exists(), main_is_final);
            assert_eq!(
                directory.path().join(main_tombstone).exists(),
                main_is_tombstoned
            );

            drop(registry);
            let registry =
                DatabaseRegistry::open_or_create(OsDataRoot::open(directory.path()).unwrap())
                    .unwrap();
            assert!(!registry.contains(name.as_str()).unwrap());
            drop(registry);

            let mut root = OsDataRoot::open(directory.path()).unwrap();
            assert_eq!(
                root.inspect_database(&entry),
                Ok(DatabaseFileInspection::Missing),
                "{operation:?} attempt {fail_at_attempt}"
            );
        }
    }

    #[test]
    fn stage_creation_failure_removes_every_created_private_artifact_and_syncs() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.stage_creation_failure_test_hook = Some(StageCreationFailureTestHook {
            fail_create_at_attempt: 3,
            create_attempts: 0,
            fsync_attempts: 0,
        });
        let entry = expected("db_00000000000000000000000000000010");
        assert!(matches!(
            root.stage_database_new(&entry),
            Err(RegistryError::Backend)
        ));
        let hook = root.stage_creation_failure_test_hook.as_ref().unwrap();
        assert_eq!(hook.create_attempts, 3);
        assert_eq!(hook.fsync_attempts, 1);
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".turso-mysql-database-")
        }));
    }

    #[test]
    fn opened_handle_retains_the_inspected_main_and_wal_inodes() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000003");
        let stage = root.stage_database_new(&entry).unwrap();
        initialize_main(&stage).unwrap();
        let main_identity = OsDataRoot::file_identity(&stage.main_file().unwrap()).unwrap();
        let wal_identity = OsDataRoot::file_identity(&stage.wal_file().unwrap()).unwrap();
        root.sync_database_stage(&stage).unwrap();
        root.publish_database_stage_new(&entry, stage).unwrap();
        root.fsync_dir().unwrap();

        let OpenDatabaseInspection::Matching(handle) = root.open_database(&entry).unwrap() else {
            panic!("published four-artifact bundle must open");
        };
        assert_eq!(
            OsDataRoot::file_identity(&handle.main_file().unwrap()).unwrap(),
            main_identity
        );
        assert_eq!(
            OsDataRoot::file_identity(&handle.wal_file().unwrap()).unwrap(),
            wal_identity
        );
    }

    #[test]
    fn acquired_lease_retains_the_checked_raw_descriptors_after_name_replacement() {
        let directory = private_tempdir();
        let mut registry =
            DatabaseRegistry::open_or_create(OsDataRoot::open(directory.path()).unwrap()).unwrap();
        registry
            .create_with_initializer("orders", |stage, _, lifetime| {
                initialize_main(stage)?;
                drop(lifetime);
                Ok(())
            })
            .unwrap();
        let lease = registry.acquire("orders").unwrap();
        let entry = expected(lease.database_file_key().as_str());
        let main = lease.database_handle().main_file().unwrap();
        let wal = lease.database_handle().wal_file().unwrap();
        let main_identity = OsDataRoot::file_identity(&main).unwrap();
        let wal_identity = OsDataRoot::file_identity(&wal).unwrap();

        let replacement_main = directory.path().join("replacement-main");
        let replacement_wal = directory.path().join("replacement-wal");
        fs::write(&replacement_main, b"replacement main").unwrap();
        fs::write(&replacement_wal, b"replacement wal").unwrap();
        fs::rename(
            &replacement_main,
            directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::Main)),
        )
        .unwrap();
        fs::rename(
            &replacement_wal,
            directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::Wal)),
        )
        .unwrap();

        assert_eq!(OsDataRoot::file_identity(&main).unwrap(), main_identity);
        assert_eq!(OsDataRoot::file_identity(&wal).unwrap(), wal_identity);
        assert_eq!(main.write_at(b"M", 128).unwrap(), 1);
        assert_eq!(wal.write_at(b"W", 0).unwrap(), 1);
        assert_ne!(
            fs::metadata(
                directory
                    .path()
                    .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::Main)),
            )
            .unwrap()
            .ino(),
            main_identity.inode
        );
        assert_ne!(
            fs::metadata(
                directory
                    .path()
                    .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::Wal)),
            )
            .unwrap()
            .ino(),
            wal_identity.inode
        );
    }

    #[test]
    fn every_missing_artifact_is_partial_and_every_foreign_sidecar_is_mismatch() {
        for artifact in DatabaseArtifact::ALL {
            let directory = private_tempdir();
            let mut root = OsDataRoot::open(directory.path()).unwrap();
            let entry = expected("db_00000000000000000000000000000004");
            create_database_new(&mut root, &entry).unwrap();
            fs::remove_file(
                directory
                    .path()
                    .join(OsDataRoot::artifact_name(&entry, artifact)),
            )
            .unwrap();
            assert_eq!(
                root.inspect_database(&entry),
                Ok(match artifact {
                    DatabaseArtifact::Main | DatabaseArtifact::Wal => {
                        DatabaseFileInspection::Partial
                    }
                    DatabaseArtifact::MainInfo | DatabaseArtifact::WalInfo => {
                        DatabaseFileInspection::Mismatch
                    }
                }),
                "{artifact:?}"
            );
        }

        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000005");
        create_database_new(&mut root, &entry).unwrap();
        fs::write(
            directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::WalInfo)),
            b"foreign",
        )
        .unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Mismatch)
        );
    }

    #[test]
    fn metadata_binds_each_sidecar_to_its_raw_artifact_inode() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000006");
        create_database_new(&mut root, &entry).unwrap();
        let replacement = directory.path().join("replacement-main");
        let mut page = [0; 4096];
        page[..SQLITE_MAGIC.len()].copy_from_slice(SQLITE_MAGIC);
        page[68..72].copy_from_slice(
            &(DatabaseFileOwner::mysql_application_id(
                DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
            ) as u32)
                .to_be_bytes(),
        );
        fs::write(&replacement, page).unwrap();
        fs::rename(
            &replacement,
            directory.path().join(entry.file_key().as_str()),
        )
        .unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Mismatch)
        );
    }

    #[test]
    fn unlink_rejects_a_partial_bundle_when_its_main_sidecar_binds_an_old_inode() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000015");
        create_database_new(&mut root, &entry).unwrap();
        fs::remove_file(
            directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::Wal)),
        )
        .unwrap();
        fs::remove_file(
            directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::WalInfo)),
        )
        .unwrap();

        let replacement = directory.path().join("replacement-main");
        let mut page = [0; 4096];
        page[..SQLITE_MAGIC.len()].copy_from_slice(SQLITE_MAGIC);
        page[68..72].copy_from_slice(
            &(DatabaseFileOwner::mysql_application_id(
                DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
            ) as u32)
                .to_be_bytes(),
        );
        fs::write(&replacement, page).unwrap();
        let replacement_inode = fs::metadata(&replacement).unwrap().ino();
        let main = directory.path().join(entry.file_key().as_str());
        fs::rename(&replacement, &main).unwrap();

        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Partial)
        );
        assert_eq!(root.unlink_database(&entry), Err(RegistryError::Backend));
        assert_eq!(fs::metadata(&main).unwrap().ino(), replacement_inode);
        assert!(directory
            .path()
            .join(OsDataRoot::artifact_name(
                &entry,
                DatabaseArtifact::MainInfo
            ))
            .exists());
    }

    #[test]
    fn unlink_recovers_a_main_tombstone_while_its_bound_sidecar_is_still_final() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000016");
        create_database_new(&mut root, &entry).unwrap();
        root.rename_child(
            &OsDataRoot::artifact_name(&entry, DatabaseArtifact::Main),
            &OsDataRoot::artifact_tombstone_name(&entry, DatabaseArtifact::Main),
        )
        .unwrap();

        root.unlink_database(&entry).unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Missing)
        );
    }

    #[test]
    fn drop_preflights_all_four_artifacts_before_mutating_any_name() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000007");
        create_database_new(&mut root, &entry).unwrap();
        let tombstone = OsDataRoot::artifact_tombstone_name(&entry, DatabaseArtifact::WalInfo);
        fs::write(directory.path().join(&tombstone), b"foreign").unwrap();
        assert_eq!(root.unlink_database(&entry), Err(RegistryError::Backend));
        for artifact in DatabaseArtifact::ALL {
            assert!(directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, artifact))
                .exists());
        }
        assert!(directory.path().join(tombstone).exists());
    }

    #[test]
    fn drop_recovers_a_partial_creating_bundle_without_touching_symlink_targets() {
        let directory = private_tempdir();
        let outside = private_tempdir();
        let target = outside.path().join("target");
        fs::write(&target, b"keep").unwrap();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000008");
        create_database_new(&mut root, &entry).unwrap();
        let wal = directory
            .path()
            .join(OsDataRoot::artifact_name(&entry, DatabaseArtifact::Wal));
        fs::remove_file(&wal).unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Partial)
        );
        root.unlink_database(&entry).unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Missing)
        );

        symlink(&target, &wal).unwrap();
        assert_eq!(root.inspect_database(&entry), Err(RegistryError::Backend));
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn private_gc_removes_only_the_four_private_stage_prefixes() {
        let directory = private_tempdir();
        let names = valid_private_names();
        for name in names {
            stale_private_file(directory.path(), name);
        }
        let keep = ".turso-mysql-database-main-tombstone-db_00000000000000000000000000000009";
        File::create(directory.path().join(keep)).unwrap();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        for name in names {
            assert!(!directory.path().join(name).exists());
        }
        assert!(directory.path().join(keep).exists());
    }

    #[test]
    fn private_gc_preserves_malformed_and_symlink_candidates() {
        let directory = private_tempdir();
        let outside = private_tempdir();
        let malformed = ".turso-mysql-database-main.tmp.0123.1.0123456789abcdef0123456789abcdef";
        stale_private_file(directory.path(), malformed);
        let target = outside.path().join("private-temp-target");
        fs::write(&target, b"keep").unwrap();
        let symlink_name = valid_private_names()[1];
        symlink(&target, directory.path().join(symlink_name)).unwrap();

        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        assert!(directory.path().join(malformed).exists());
        assert!(fs::symlink_metadata(directory.path().join(symlink_name))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }

    #[test]
    fn private_gc_syncs_a_successful_unlink_before_reporting_a_later_failure() {
        let directory = private_tempdir();
        let names = valid_private_names();
        for name in names[..3].iter().copied() {
            stale_private_file(directory.path(), name);
        }
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.private_temporary_cleanup_test_hook = Some(PrivateTemporaryCleanupTestHook {
            fail_unlink_at_attempt: 2,
            unlink_attempts: 0,
            fsync_attempts: 0,
        });
        assert!(matches!(
            root.acquire_exclusive_registry_lock(),
            Err(RegistryError::Backend)
        ));
        let hook = root.private_temporary_cleanup_test_hook.as_ref().unwrap();
        assert_eq!(hook.unlink_attempts, 2);
        assert_eq!(hook.fsync_attempts, 1);
        assert_eq!(
            names[..3]
                .iter()
                .filter(|name| directory.path().join(name).exists())
                .count(),
            2
        );
    }

    #[test]
    fn stale_tombstones_reject_new_database_creation() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let entry = expected("db_00000000000000000000000000000012");
        let stage = root.stage_database_new(&entry).unwrap();
        let tombstone = OsDataRoot::artifact_tombstone_name(&entry, DatabaseArtifact::Wal);
        root.rename_child(&stage.wal_temporary, &tombstone).unwrap();
        root.abort_database_stage(&entry, stage).unwrap();
        assert_eq!(
            root.inspect_database_creation(&entry),
            Ok(DatabaseFileInspection::Mismatch)
        );
        assert!(directory.path().join(tombstone).exists());
    }

    #[test]
    fn publishing_rejects_replaced_raw_and_symlinked_info_stage_names() {
        for (artifact, replace_with_symlink) in [
            (DatabaseArtifact::Main, false),
            (DatabaseArtifact::MainInfo, true),
        ] {
            let directory = private_tempdir();
            let outside = private_tempdir();
            let target = outside.path().join("stage-target");
            fs::write(&target, b"keep").unwrap();
            let mut root = OsDataRoot::open(directory.path()).unwrap();
            let entry = expected("db_00000000000000000000000000000013");
            let stage = root.stage_database_new(&entry).unwrap();
            initialize_main(&stage).unwrap();
            let temporary = match artifact {
                DatabaseArtifact::Main => &stage.main_temporary,
                DatabaseArtifact::MainInfo => &stage.main_info_temporary,
                _ => unreachable!(),
            };
            fs::remove_file(directory.path().join(temporary)).unwrap();
            if replace_with_symlink {
                symlink(&target, directory.path().join(temporary)).unwrap();
            } else {
                fs::write(directory.path().join(temporary), b"replacement").unwrap();
            }

            assert_eq!(
                root.publish_database_stage_new(&entry, stage),
                Err(RegistryError::Backend),
                "{artifact:?}"
            );
            assert!(!directory
                .path()
                .join(OsDataRoot::artifact_name(&entry, artifact))
                .exists());
            assert_eq!(fs::read(&target).unwrap(), b"keep");
        }
    }

    #[test]
    fn reopening_recovers_an_interrupted_dropping_record() {
        let directory = private_tempdir();
        let entry = expected("db_00000000000000000000000000000014");
        let name = DatabaseName::parse("dropping").unwrap();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        create_database_new(&mut root, &entry).unwrap();
        root.create_manifest_new(&RootManifest::lower_case_table_names_1())
            .unwrap();
        root.fsync_dir().unwrap();
        root.replace_registry(&RegistrySnapshot {
            entries: BTreeMap::from([(
                name,
                RegistryEntry {
                    file_key: entry.file_key().clone(),
                    state: DatabaseState::Dropping,
                },
            )]),
        })
        .unwrap();
        drop(root);

        let registry =
            DatabaseRegistry::open_or_create(OsDataRoot::open(directory.path()).unwrap()).unwrap();
        assert!(!registry.contains("dropping").unwrap());
        drop(registry);
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Missing)
        );
    }
}

/// A cloneable reference to the lock file. Clones retain the same open handle.
pub(crate) struct OsRegistryLock(Arc<File>);

/// The inspected writable SQLite descriptor pair for one opaque database key.
///
/// Both files retain the handles that were checked through the root descriptor;
/// callers never reopen them by a logical database name.
pub(crate) struct OsDatabaseHandle {
    main_file: File,
    wal_file: File,
    identity: OpaqueFileKey,
}

/// A private writable SQLite pair retained through initialization and
/// publication. The open descriptors identify the same inodes later linked
/// under their final names.
pub(crate) struct OsDatabaseStage {
    main_file: File,
    wal_file: File,
    main_info_file: File,
    wal_info_file: File,
    main_temporary: String,
    wal_temporary: String,
    main_info_temporary: String,
    wal_info_temporary: String,
    expected: DatabaseFileExpectation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

enum UnlinkArtifactLocation {
    Missing,
    Final(File),
    Tombstone(File),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatabaseArtifact {
    Main,
    Wal,
    MainInfo,
    WalInfo,
}

#[derive(Debug, Clone, Copy)]
enum PrivateTemporaryKind {
    Registry,
    DatabaseMain,
    DatabaseWal,
    DatabaseMainInfo,
    DatabaseWalInfo,
}

#[cfg(test)]
struct PrivateTemporaryCleanupTestHook {
    fail_unlink_at_attempt: usize,
    unlink_attempts: usize,
    fsync_attempts: usize,
}

#[cfg(test)]
struct StageCreationFailureTestHook {
    fail_create_at_attempt: usize,
    create_attempts: usize,
    fsync_attempts: usize,
}

#[cfg(test)]
struct PublishDatabaseStageTestHook {
    fail_sidecar_sync: bool,
    sidecar_sync_attempts: usize,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatabaseArtifactOperation {
    PublishLink,
    DirectorySync,
    DropRename,
    DropUnlink,
}

#[cfg(test)]
struct DatabaseArtifactOperationFailureTestHook {
    operation: DatabaseArtifactOperation,
    fail_at_attempt: usize,
    attempts: usize,
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: the stream owns the duplicated descriptor returned by
        // `fdopendir`, and is closed exactly once here.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

impl DatabaseArtifact {
    const ALL: [Self; 4] = [Self::Main, Self::Wal, Self::MainInfo, Self::WalInfo];

    const fn metadata_role(self) -> Option<MetadataArtifactRole> {
        match self {
            Self::Main => None,
            Self::Wal => None,
            Self::MainInfo => Some(MetadataArtifactRole::Main),
            Self::WalInfo => Some(MetadataArtifactRole::Wal),
        }
    }

    const fn temporary_prefix(self) -> &'static str {
        match self {
            Self::Main => "database-main.tmp",
            Self::Wal => "database-wal.tmp",
            Self::MainInfo => "database-main-info.tmp",
            Self::WalInfo => "database-wal-info.tmp",
        }
    }

    const fn tombstone_label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Wal => "wal",
            Self::MainInfo => "main-info",
            Self::WalInfo => "wal-info",
        }
    }
}

impl UnlinkArtifactLocation {
    fn file(&self) -> Option<&File> {
        match self {
            Self::Missing => None,
            Self::Final(file) | Self::Tombstone(file) => Some(file),
        }
    }

    const fn is_final(&self) -> bool {
        matches!(self, Self::Final(_))
    }

    const fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone(_))
    }
}

impl Clone for OsRegistryLock {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl OsDatabaseHandle {
    pub(crate) fn main_file(&self) -> Result<File, RegistryError> {
        self.main_file
            .try_clone()
            .map_err(|_| RegistryError::Backend)
    }

    pub(crate) fn wal_file(&self) -> Result<File, RegistryError> {
        self.wal_file
            .try_clone()
            .map_err(|_| RegistryError::Backend)
    }

    pub(crate) fn identity(&self) -> &OpaqueFileKey {
        &self.identity
    }
}

impl OsDatabaseStage {
    pub(crate) fn main_file(&self) -> Result<File, RegistryError> {
        self.main_file
            .try_clone()
            .map_err(|_| RegistryError::Backend)
    }

    pub(crate) fn wal_file(&self) -> Result<File, RegistryError> {
        self.wal_file
            .try_clone()
            .map_err(|_| RegistryError::Backend)
    }
}

impl OsDataRoot {
    pub(crate) fn open(path: &Path) -> Result<Self, RegistryError> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| RegistryError::Backend)?;
        // This is the one path-based operation. The resulting descriptor is
        // retained and is the capability used by every method below.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0,
            )
        };
        if fd < 0 {
            return Err(RegistryError::Backend);
        }
        // SAFETY: `fd` is a fresh descriptor owned by this value.
        let directory = unsafe { File::from_raw_fd(fd) };
        let metadata = directory.metadata().map_err(|_| RegistryError::Backend)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(RegistryError::Backend);
        }
        Ok(Self {
            directory,
            #[cfg(test)]
            private_temporary_cleanup_test_hook: None,
            #[cfg(test)]
            stage_creation_failure_test_hook: None,
            #[cfg(test)]
            publish_database_stage_test_hook: None,
            #[cfg(test)]
            database_artifact_operation_failure_test_hook: None,
        })
    }

    #[cfg(test)]
    fn fail_database_artifact_operation(
        &mut self,
        operation: DatabaseArtifactOperation,
    ) -> Result<(), RegistryError> {
        let Some(hook) = &mut self.database_artifact_operation_failure_test_hook else {
            return Ok(());
        };
        if hook.operation != operation {
            return Ok(());
        }
        hook.attempts += 1;
        if hook.attempts == hook.fail_at_attempt {
            return Err(RegistryError::Backend);
        }
        Ok(())
    }

    fn open_child(
        &self,
        name: &str,
        flags: i32,
        mode: libc::mode_t,
    ) -> Result<File, RegistryError> {
        let name = CString::new(name.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(RegistryError::Backend);
        }
        // SAFETY: `fd` is a fresh descriptor owned by this value.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_child_optional(&self, name: &str, flags: i32) -> Result<Option<File>, RegistryError> {
        let name = CString::new(name.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(RegistryError::Backend);
        }
        // SAFETY: `fd` is a fresh descriptor owned by this value.
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    }

    fn open_stage_child(&mut self, name: &str) -> Result<File, RegistryError> {
        #[cfg(test)]
        if let Some(hook) = &mut self.stage_creation_failure_test_hook {
            hook.create_attempts += 1;
            if hook.create_attempts == hook.fail_create_at_attempt {
                return Err(RegistryError::Backend);
            }
        }
        self.open_child(name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600)
    }

    fn open_private_temporary(&self, name: &[u8]) -> Result<Option<File>, RegistryError> {
        let name = CString::new(name).map_err(|_| RegistryError::Backend)?;
        loop {
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                    0,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` is a fresh descriptor owned by this value.
                return Ok(Some(unsafe { File::from_raw_fd(fd) }));
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // A candidate that disappeared, became a symlink, or is otherwise
            // not openable is left untouched. Cleanup must fail closed for
            // entries that cannot be inspected through this capability.
            return Ok(None);
        }
    }

    fn directory_private_temporary_names(&self) -> Result<Vec<Vec<u8>>, RegistryError> {
        let duplicated =
            unsafe { libc::fcntl(self.directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated < 0 {
            return Err(RegistryError::Backend);
        }
        let stream = unsafe { libc::fdopendir(duplicated) };
        if stream.is_null() {
            // SAFETY: `duplicated` was not consumed when `fdopendir` failed.
            unsafe {
                libc::close(duplicated);
            }
            return Err(RegistryError::Backend);
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            errno::set_errno(errno::Errno(0));
            // SAFETY: `stream.0` is a valid directory stream until its Drop.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if errno::errno().0 != 0 {
                    return Err(RegistryError::Backend);
                }
                break;
            }
            // SAFETY: `d_name` is a NUL-terminated name supplied by libc.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if Self::private_temporary_kind(name).is_some() {
                names.push(name.to_vec());
            }
        }
        Ok(names)
    }

    fn private_temporary_kind(name: &[u8]) -> Option<PrivateTemporaryKind> {
        for (prefix, kind) in PRIVATE_TEMP_PREFIXES {
            let Some(rest) = name.strip_prefix(prefix) else {
                continue;
            };
            let mut parts = rest.split(|byte| *byte == b'.');
            let Some(pid) = parts.next() else {
                continue;
            };
            let Some(counter) = parts.next() else {
                continue;
            };
            let Some(random) = parts.next() else {
                continue;
            };
            if parts.next().is_none()
                && Self::positive_decimal(pid)
                && Self::positive_decimal(counter)
                && random.len() == 32
                && random
                    .iter()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Some(kind);
            }
        }
        None
    }

    fn positive_decimal(value: &[u8]) -> bool {
        if value.is_empty() || (value.len() > 1 && value[0] == b'0') {
            return false;
        }
        let mut number = 0u64;
        for byte in value {
            if !byte.is_ascii_digit() {
                return false;
            }
            let Some(next) = number
                .checked_mul(10)
                .and_then(|number| number.checked_add(u64::from(byte - b'0')))
            else {
                return false;
            };
            number = next;
        }
        number != 0
    }

    fn collect_private_temporary_files(&mut self) -> Result<(), RegistryError> {
        let now = SystemTime::now();
        let names = self.directory_private_temporary_names()?;
        let mut removed = false;
        for name in names {
            let Some(file) = self.open_private_temporary(&name)? else {
                continue;
            };
            let metadata = file.metadata().map_err(|_| RegistryError::Backend)?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || !Self::is_stale(
                    metadata.modified().map_err(|_| RegistryError::Backend)?,
                    now,
                )
            {
                continue;
            }
            let identity = Self::file_identity(&file)?;

            // Re-open after the age and ownership check. Cooperative registry
            // writers hold the same exclusive root lock, so the pathname
            // cannot be replaced by a live creator before unlink.
            let Some(current) = self.open_private_temporary(&name)? else {
                continue;
            };
            let current_metadata = current.metadata().map_err(|_| RegistryError::Backend)?;
            if !current_metadata.is_file()
                || current_metadata.uid() != unsafe { libc::geteuid() }
                || Self::file_identity(&current)? != identity
                || !Self::is_stale(
                    current_metadata
                        .modified()
                        .map_err(|_| RegistryError::Backend)?,
                    now,
                )
            {
                continue;
            }
            match self.unlink_private_temporary_if_present(&name) {
                Ok(did_remove) => removed |= did_remove,
                Err(error) => {
                    if removed {
                        self.fsync_dir()?;
                    }
                    return Err(error);
                }
            }
        }
        if removed {
            self.fsync_dir()?;
        }
        Ok(())
    }

    fn is_stale(modified: SystemTime, now: SystemTime) -> bool {
        now.duration_since(modified)
            .map(|age| age >= PRIVATE_TEMP_MAX_AGE)
            .unwrap_or(false)
    }

    fn read_bounded(file: File, limit: usize) -> Result<Vec<u8>, RegistryError> {
        let length = file.metadata().map_err(|_| RegistryError::Backend)?.len();
        if length > limit as u64 {
            return Err(RegistryError::Backend);
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| RegistryError::Backend)?;
        if bytes.len() > limit {
            return Err(RegistryError::Backend);
        }
        Ok(bytes)
    }

    fn read_json<T: DeserializeOwned>(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Option<T>, RegistryError> {
        let Some(file) = self.open_child_optional(name, libc::O_RDONLY)? else {
            return Ok(None);
        };
        let bytes = Self::read_bounded(file, limit)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| RegistryError::Backend)
    }

    fn file_identity(file: &File) -> Result<FileIdentity, RegistryError> {
        let metadata = file.metadata().map_err(|_| RegistryError::Backend)?;
        Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn artifact_name(expected: &DatabaseFileExpectation, artifact: DatabaseArtifact) -> String {
        let key = expected.file_key().as_str();
        match artifact {
            DatabaseArtifact::Main => key.to_owned(),
            DatabaseArtifact::Wal => format!("{key}{WAL_SUFFIX}"),
            DatabaseArtifact::MainInfo => format!("{key}{MAIN_INFO_SUFFIX}"),
            DatabaseArtifact::WalInfo => format!("{key}{WAL_INFO_SUFFIX}"),
        }
    }

    fn artifact_tombstone_name(
        expected: &DatabaseFileExpectation,
        artifact: DatabaseArtifact,
    ) -> String {
        format!(
            ".turso-mysql-database-{}-tombstone-{}",
            artifact.tombstone_label(),
            expected.file_key().as_str()
        )
    }

    fn durable_identity(expected: &DatabaseFileExpectation) -> Result<[u8; 16], RegistryError> {
        expected.file_key().to_database_identity()
    }

    fn metadata_bytes(
        expected: &DatabaseFileExpectation,
        role: MetadataArtifactRole,
        target: &File,
    ) -> Result<[u8; database_metadata::ENCODED_BYTES], RegistryError> {
        let identity = Self::file_identity(target)?;
        DatabaseMetadata::new(
            Self::durable_identity(expected)?,
            role,
            identity.device,
            identity.inode,
        )
        .map(|metadata| metadata.encode())
        .map_err(|_| RegistryError::Backend)
    }

    fn read_at_start(file: &File, limit: usize) -> Result<Vec<u8>, RegistryError> {
        let mut bytes = vec![0; limit];
        let mut read = 0;
        while read < bytes.len() {
            let count = file
                .read_at(&mut bytes[read..], read as u64)
                .map_err(|_| RegistryError::Backend)?;
            if count == 0 {
                break;
            }
            read += count;
        }
        bytes.truncate(read);
        Ok(bytes)
    }

    fn read_fixed_at_start(file: &File, length: usize) -> Result<Vec<u8>, RegistryError> {
        if file.metadata().map_err(|_| RegistryError::Backend)?.len() != length as u64 {
            return Err(RegistryError::Backend);
        }
        let bytes = Self::read_at_start(file, length)?;
        if bytes.len() != length {
            return Err(RegistryError::Backend);
        }
        Ok(bytes)
    }

    fn main_header_matches(
        file: &File,
        expected: &DatabaseFileExpectation,
    ) -> Result<bool, RegistryError> {
        let bytes = Self::read_at_start(file, SQLITE_HEADER_BYTES)?;
        if bytes.len() != SQLITE_HEADER_BYTES || bytes[..SQLITE_MAGIC.len()] != *SQLITE_MAGIC {
            return Ok(false);
        }
        let actual = u32::from_be_bytes(
            bytes[68..72]
                .try_into()
                .expect("a checked SQLite header includes the application id"),
        );
        Ok(expected
            .marker()
            .validate_for_policy(NamePolicy::LowerCaseTableNames1)
            && actual
                == DatabaseFileOwner::mysql_application_id(
                    DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
                ) as u32)
    }

    fn metadata_matches(
        file: &File,
        expected: &DatabaseFileExpectation,
        role: MetadataArtifactRole,
        target: &File,
    ) -> Result<bool, RegistryError> {
        let bytes = Self::read_fixed_at_start(file, database_metadata::ENCODED_BYTES)?;
        let target = Self::file_identity(target)?;
        Ok(DatabaseMetadata::decode(&bytes).ok()
            == DatabaseMetadata::new(
                Self::durable_identity(expected)?,
                role,
                target.device,
                target.inode,
            )
            .ok())
    }

    fn inspect_open_artifact(
        file: &File,
        expected: &DatabaseFileExpectation,
        artifact: DatabaseArtifact,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        if !file
            .metadata()
            .map_err(|_| RegistryError::Backend)?
            .is_file()
        {
            return Ok(DatabaseFileInspection::Mismatch);
        }
        let matching = match artifact {
            DatabaseArtifact::Main => Self::main_header_matches(file, expected)?,
            DatabaseArtifact::Wal => true,
            DatabaseArtifact::MainInfo | DatabaseArtifact::WalInfo => {
                let bytes = match Self::read_fixed_at_start(file, database_metadata::ENCODED_BYTES)
                {
                    Ok(bytes) => bytes,
                    Err(_) => return Ok(DatabaseFileInspection::Mismatch),
                };
                match DatabaseMetadata::decode(&bytes) {
                    Ok(metadata) => {
                        metadata.durable_identity() == Self::durable_identity(expected)?
                            && metadata.role() == artifact.metadata_role().unwrap()
                    }
                    Err(_) => false,
                }
            }
        };
        Ok(if matching {
            DatabaseFileInspection::Matching
        } else {
            DatabaseFileInspection::Mismatch
        })
    }

    fn inspect_named_artifact(
        &self,
        name: &str,
        expected: &DatabaseFileExpectation,
        artifact: DatabaseArtifact,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        let Some(file) = self.open_child_optional(name, libc::O_RDONLY)? else {
            return Ok(DatabaseFileInspection::Missing);
        };
        Self::inspect_open_artifact(&file, expected, artifact)
    }

    fn open_unlink_artifact(
        &self,
        expected: &DatabaseFileExpectation,
        artifact: DatabaseArtifact,
    ) -> Result<UnlinkArtifactLocation, RegistryError> {
        let named =
            self.open_child_optional(&Self::artifact_name(expected, artifact), libc::O_RDONLY)?;
        let tombstoned = self.open_child_optional(
            &Self::artifact_tombstone_name(expected, artifact),
            libc::O_RDONLY,
        )?;
        match (named, tombstoned) {
            (None, None) => Ok(UnlinkArtifactLocation::Missing),
            (Some(file), None) => {
                if Self::inspect_open_artifact(&file, expected, artifact)?
                    != DatabaseFileInspection::Matching
                {
                    return Err(RegistryError::Backend);
                }
                Ok(UnlinkArtifactLocation::Final(file))
            }
            (None, Some(file)) => {
                if Self::inspect_open_artifact(&file, expected, artifact)?
                    != DatabaseFileInspection::Matching
                {
                    return Err(RegistryError::Backend);
                }
                Ok(UnlinkArtifactLocation::Tombstone(file))
            }
            (Some(_), Some(_)) => Err(RegistryError::Backend),
        }
    }

    fn preflight_unlink_raw_pair(
        expected: &DatabaseFileExpectation,
        raw: &UnlinkArtifactLocation,
        info: &UnlinkArtifactLocation,
        role: MetadataArtifactRole,
    ) -> Result<(), RegistryError> {
        let Some(raw) = raw.file() else {
            return Ok(());
        };
        let Some(info) = info.file() else {
            return Err(RegistryError::Backend);
        };
        if !Self::metadata_matches(info, expected, role, raw)? {
            return Err(RegistryError::Backend);
        }
        Ok(())
    }

    fn preflight_database_unlink(
        &self,
        expected: &DatabaseFileExpectation,
    ) -> Result<(), RegistryError> {
        let main = self.open_unlink_artifact(expected, DatabaseArtifact::Main)?;
        let wal = self.open_unlink_artifact(expected, DatabaseArtifact::Wal)?;
        let main_info = self.open_unlink_artifact(expected, DatabaseArtifact::MainInfo)?;
        let wal_info = self.open_unlink_artifact(expected, DatabaseArtifact::WalInfo)?;

        if (main.is_final() && main_info.is_tombstone())
            || (wal.is_final() && wal_info.is_tombstone())
        {
            return Err(RegistryError::Backend);
        }
        Self::preflight_unlink_raw_pair(expected, &main, &main_info, MetadataArtifactRole::Main)?;
        Self::preflight_unlink_raw_pair(expected, &wal, &wal_info, MetadataArtifactRole::Wal)?;
        if let (Some(main), Some(wal)) = (main.file(), wal.file()) {
            if Self::file_identity(main)? == Self::file_identity(wal)? {
                return Err(RegistryError::Backend);
            }
        }
        Ok(())
    }

    fn unlink_database_artifact(
        &mut self,
        expected: &DatabaseFileExpectation,
        artifact: DatabaseArtifact,
    ) -> Result<(), RegistryError> {
        let name = Self::artifact_name(expected, artifact);
        let tombstone = Self::artifact_tombstone_name(expected, artifact);
        let Some(file) = self.open_child_optional(&name, libc::O_RDONLY)? else {
            let Some(tombstone_file) = self.open_child_optional(&tombstone, libc::O_RDONLY)? else {
                return Ok(());
            };
            if Self::inspect_open_artifact(&tombstone_file, expected, artifact)?
                != DatabaseFileInspection::Matching
            {
                return Err(RegistryError::Backend);
            }
            #[cfg(test)]
            self.fail_database_artifact_operation(DatabaseArtifactOperation::DropUnlink)?;
            self.unlink_if_present(&tombstone)?;
            return self.fsync_dir();
        };
        if Self::inspect_open_artifact(&file, expected, artifact)?
            != DatabaseFileInspection::Matching
        {
            return Err(RegistryError::Backend);
        }
        let identity = Self::file_identity(&file)?;
        #[cfg(test)]
        self.fail_database_artifact_operation(DatabaseArtifactOperation::DropRename)?;
        self.rename_child(&name, &tombstone)?;
        self.fsync_dir()?;
        let Some(tombstone_file) = self.open_child_optional(&tombstone, libc::O_RDONLY)? else {
            return Err(RegistryError::Backend);
        };
        if Self::inspect_open_artifact(&tombstone_file, expected, artifact)?
            != DatabaseFileInspection::Matching
            || Self::file_identity(&tombstone_file)? != identity
        {
            return Err(RegistryError::Backend);
        }
        #[cfg(test)]
        self.fail_database_artifact_operation(DatabaseArtifactOperation::DropUnlink)?;
        self.unlink_if_present(&tombstone)?;
        self.fsync_dir()
    }

    fn write_new_synced(&self, name: &str, bytes: &[u8]) -> Result<(), RegistryError> {
        let mut file =
            self.open_child(name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
        file.write_all(bytes).map_err(|_| RegistryError::Backend)?;
        file.sync_all().map_err(|_| RegistryError::Backend)
    }

    fn unlink_if_present(&self, name: &str) -> Result<(), RegistryError> {
        self.unlink_if_present_bytes(name.as_bytes())
    }

    fn unlink_if_present_bytes(&self, name: &[u8]) -> Result<(), RegistryError> {
        let name = CString::new(name).map_err(|_| RegistryError::Backend)?;
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(RegistryError::Backend)
        }
    }

    fn unlink_private_temporary_if_present(&mut self, name: &[u8]) -> Result<bool, RegistryError> {
        #[cfg(test)]
        if let Some(hook) = &mut self.private_temporary_cleanup_test_hook {
            hook.unlink_attempts += 1;
            if hook.unlink_attempts == hook.fail_unlink_at_attempt {
                return Err(RegistryError::Backend);
            }
        }
        let name = CString::new(name).map_err(|_| RegistryError::Backend)?;
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 {
            Ok(true)
        } else if matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOENT) | Some(libc::EISDIR) | Some(libc::ELOOP) | Some(libc::EPERM)
        ) {
            Ok(false)
        } else {
            Err(RegistryError::Backend)
        }
    }

    fn rename_child(&self, from: &str, to: &str) -> Result<(), RegistryError> {
        let from = CString::new(from.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let to = CString::new(to.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let result = unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                from.as_ptr(),
                self.directory.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(RegistryError::Backend)
        }
    }

    fn publish_child_new(&mut self, from: &str, to: &str) -> Result<(), RegistryError> {
        #[cfg(test)]
        self.fail_database_artifact_operation(DatabaseArtifactOperation::PublishLink)?;
        let from = CString::new(from.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let to = CString::new(to.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let result = unsafe {
            libc::linkat(
                self.directory.as_raw_fd(),
                from.as_ptr(),
                self.directory.as_raw_fd(),
                to.as_ptr(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(RegistryError::Backend)
        }
    }

    fn publish_staged_child_new(
        &mut self,
        temporary: &str,
        stage_file: &File,
        final_name: &str,
    ) -> Result<(), RegistryError> {
        // This detects a replaced private name before it is made visible. The
        // private root and advisory registry lock still define the same-UID
        // cooperative-writer trust boundary; this is not a complete defense
        // against a non-cooperating writer racing every system call.
        let stage_identity = Self::file_identity(stage_file)?;
        let temporary_file = self.open_child(temporary, libc::O_RDONLY, 0)?;
        if Self::file_identity(&temporary_file)? != stage_identity {
            return Err(RegistryError::Backend);
        }

        self.publish_child_new(temporary, final_name)?;

        let final_file = self.open_child(final_name, libc::O_RDONLY, 0)?;
        if Self::file_identity(&final_file)? != stage_identity {
            // Do not guess whether this name is ours: recovery will only remove
            // files whose owner marker and sidecar binding match the Creating
            // record.
            return Err(RegistryError::Backend);
        }

        self.unlink_if_present(temporary)
    }

    fn sync_published_database_sidecars(&mut self) -> Result<(), RegistryError> {
        #[cfg(test)]
        if let Some(hook) = &mut self.publish_database_stage_test_hook {
            hook.sidecar_sync_attempts += 1;
            if hook.fail_sidecar_sync {
                return Err(RegistryError::Backend);
            }
        }
        self.fsync_dir()
    }

    fn abort_stage_names(&mut self, names: &[&str]) -> Result<(), RegistryError> {
        let mut removed = false;
        let mut first_error = None;
        for name in names {
            let existed = match self.open_child_optional(name, libc::O_RDONLY) {
                Ok(file) => file.is_some(),
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            match self.unlink_if_present(name) {
                Ok(()) => removed |= existed,
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if removed {
            if let Err(error) = self.fsync_dir() {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn next_private_name(prefix: &str) -> Result<String, RegistryError> {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| RegistryError::Backend)?;
        let mut suffix = String::with_capacity(random.len() * 2);
        for byte in random {
            use std::fmt::Write as _;
            write!(&mut suffix, "{byte:02x}").map_err(|_| RegistryError::Backend)?;
        }
        Ok(format!(
            ".turso-mysql-{}.{}.{}.{}",
            prefix,
            std::process::id(),
            id,
            suffix
        ))
    }
}

impl RegistryRoot for OsDataRoot {
    type RegistryLock = OsRegistryLock;
    type DatabaseHandle = OsDatabaseHandle;
    type DatabaseStage = OsDatabaseStage;

    fn acquire_exclusive_registry_lock(&mut self) -> Result<Self::RegistryLock, RegistryError> {
        let lock = self.open_child(LOCK_FILE, libc::O_RDWR | libc::O_CREAT, 0o600)?;
        loop {
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            ) {
                return Err(RegistryError::RegistryAlreadyOpen);
            }
            return Err(RegistryError::Backend);
        }
        self.collect_private_temporary_files()?;
        Ok(OsRegistryLock(Arc::new(lock)))
    }

    fn read_manifest(&mut self) -> Result<Option<RootManifest>, RegistryError> {
        self.read_json(MANIFEST_FILE, MAX_MANIFEST_BYTES)
    }

    fn create_manifest_new(&mut self, manifest: &RootManifest) -> Result<(), RegistryError> {
        manifest.validate()?;
        let bytes = serde_json::to_vec(manifest).map_err(|_| RegistryError::Backend)?;
        self.write_new_synced(MANIFEST_FILE, &bytes)
    }

    fn read_registry(&mut self) -> Result<Option<RegistrySnapshot>, RegistryError> {
        let Some(registry) = self.read_json(REGISTRY_FILE, MAX_REGISTRY_BYTES)? else {
            return Ok(None);
        };
        Ok(Some(registry))
    }

    fn replace_registry(&mut self, registry: &RegistrySnapshot) -> Result<(), RegistryError> {
        let bytes = serde_json::to_vec(registry).map_err(|_| RegistryError::Backend)?;
        if bytes.len() > MAX_REGISTRY_BYTES {
            return Err(RegistryError::Backend);
        }
        let temporary = Self::next_private_name("registry.tmp")?;
        if let Err(error) = self.write_new_synced(&temporary, &bytes) {
            let _ = self.unlink_if_present(&temporary);
            return Err(error);
        }
        if let Err(error) = self.rename_child(&temporary, REGISTRY_FILE) {
            // The temporary is ours, and the destination was not touched by a
            // failed rename. Cleanup failure must not hide the write failure.
            let _ = self.unlink_if_present(&temporary);
            return Err(error);
        }
        self.fsync_dir()
    }

    fn allocate_file_key(&mut self) -> Result<OpaqueFileKey, RegistryError> {
        loop {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|_| RegistryError::Backend)?;
            if random.iter().all(|byte| *byte == 0) {
                continue;
            }
            let mut key = String::with_capacity(35);
            key.push_str("db_");
            for byte in random {
                use std::fmt::Write as _;
                write!(&mut key, "{byte:02x}").map_err(|_| RegistryError::Backend)?;
            }
            return OpaqueFileKey::new(key);
        }
    }

    fn inspect_database_creation(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        for artifact in DatabaseArtifact::ALL {
            for name in [
                Self::artifact_name(expected, artifact),
                Self::artifact_tombstone_name(expected, artifact),
            ] {
                if self.inspect_named_artifact(&name, expected, artifact)?
                    != DatabaseFileInspection::Missing
                {
                    return Ok(DatabaseFileInspection::Mismatch);
                }
            }
        }
        Ok(DatabaseFileInspection::Missing)
    }

    fn stage_database_new(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<Self::DatabaseStage, RegistryError> {
        let main_temporary = Self::next_private_name(DatabaseArtifact::Main.temporary_prefix())?;
        let wal_temporary = Self::next_private_name(DatabaseArtifact::Wal.temporary_prefix())?;
        let main_info_temporary =
            Self::next_private_name(DatabaseArtifact::MainInfo.temporary_prefix())?;
        let wal_info_temporary =
            Self::next_private_name(DatabaseArtifact::WalInfo.temporary_prefix())?;
        let names = [
            main_temporary.as_str(),
            wal_temporary.as_str(),
            main_info_temporary.as_str(),
            wal_info_temporary.as_str(),
        ];
        let main_file = self.open_stage_child(&main_temporary)?;
        let wal_file = match self.open_stage_child(&wal_temporary) {
            Ok(file) => file,
            Err(error) => {
                self.abort_stage_names(&names[..1])?;
                return Err(error);
            }
        };
        let mut main_info_file = match self.open_stage_child(&main_info_temporary) {
            Ok(file) => file,
            Err(error) => {
                self.abort_stage_names(&names[..2])?;
                return Err(error);
            }
        };
        let mut wal_info_file = match self.open_stage_child(&wal_info_temporary) {
            Ok(file) => file,
            Err(error) => {
                self.abort_stage_names(&names[..3])?;
                return Err(error);
            }
        };
        let mut write_metadata = || -> Result<(), RegistryError> {
            main_info_file
                .write_all(&Self::metadata_bytes(
                    expected,
                    MetadataArtifactRole::Main,
                    &main_file,
                )?)
                .map_err(|_| RegistryError::Backend)?;
            wal_info_file
                .write_all(&Self::metadata_bytes(
                    expected,
                    MetadataArtifactRole::Wal,
                    &wal_file,
                )?)
                .map_err(|_| RegistryError::Backend)
        };
        if let Err(error) = write_metadata() {
            self.abort_stage_names(&names)?;
            return Err(error);
        }
        Ok(OsDatabaseStage {
            main_file,
            wal_file,
            main_info_file,
            wal_info_file,
            main_temporary,
            wal_temporary,
            main_info_temporary,
            wal_info_temporary,
            expected: expected.clone(),
        })
    }

    fn sync_database_stage(&mut self, stage: &Self::DatabaseStage) -> Result<(), RegistryError> {
        if !Self::main_header_matches(&stage.main_file, &stage.expected)?
            || !Self::metadata_matches(
                &stage.main_info_file,
                &stage.expected,
                MetadataArtifactRole::Main,
                &stage.main_file,
            )?
            || !Self::metadata_matches(
                &stage.wal_info_file,
                &stage.expected,
                MetadataArtifactRole::Wal,
                &stage.wal_file,
            )?
        {
            return Err(RegistryError::Backend);
        }
        for file in [
            &stage.main_file,
            &stage.wal_file,
            &stage.main_info_file,
            &stage.wal_info_file,
        ] {
            file.sync_all().map_err(|_| RegistryError::Backend)?;
        }
        Ok(())
    }

    fn publish_database_stage_new(
        &mut self,
        expected: &DatabaseFileExpectation,
        stage: Self::DatabaseStage,
    ) -> Result<(), RegistryError> {
        for (artifact, temporary, file) in [
            (
                DatabaseArtifact::MainInfo,
                &stage.main_info_temporary,
                &stage.main_info_file,
            ),
            (
                DatabaseArtifact::WalInfo,
                &stage.wal_info_temporary,
                &stage.wal_info_file,
            ),
        ] {
            self.publish_staged_child_new(
                temporary,
                file,
                &Self::artifact_name(expected, artifact),
            )?;
        }

        // A raw artifact is recoverable only after its inode-bound sidecar is
        // durable. A failed sync leaves the durable Creating record and any
        // published sidecars for recovery; do not abort this consumed stage.
        self.sync_published_database_sidecars()?;

        for (artifact, temporary, file) in [
            (
                DatabaseArtifact::Main,
                &stage.main_temporary,
                &stage.main_file,
            ),
            (DatabaseArtifact::Wal, &stage.wal_temporary, &stage.wal_file),
        ] {
            self.publish_staged_child_new(
                temporary,
                file,
                &Self::artifact_name(expected, artifact),
            )?;
        }
        Ok(())
    }

    fn abort_database_stage(
        &mut self,
        _expected: &DatabaseFileExpectation,
        stage: Self::DatabaseStage,
    ) -> Result<(), RegistryError> {
        self.abort_stage_names(&[
            &stage.main_temporary,
            &stage.wal_temporary,
            &stage.main_info_temporary,
            &stage.wal_info_temporary,
        ])
    }

    fn inspect_database(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        let main = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::Main),
            libc::O_RDONLY,
        )?;
        let wal = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::Wal),
            libc::O_RDONLY,
        )?;
        let main_info = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::MainInfo),
            libc::O_RDONLY,
        )?;
        let wal_info = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::WalInfo),
            libc::O_RDONLY,
        )?;
        if [
            main.as_ref(),
            wal.as_ref(),
            main_info.as_ref(),
            wal_info.as_ref(),
        ]
        .iter()
        .all(Option::is_none)
        {
            return Ok(DatabaseFileInspection::Missing);
        }
        for (file, artifact) in [
            (main.as_ref(), DatabaseArtifact::Main),
            (wal.as_ref(), DatabaseArtifact::Wal),
            (main_info.as_ref(), DatabaseArtifact::MainInfo),
            (wal_info.as_ref(), DatabaseArtifact::WalInfo),
        ] {
            if let Some(file) = file {
                if Self::inspect_open_artifact(file, expected, artifact)?
                    != DatabaseFileInspection::Matching
                {
                    return Ok(DatabaseFileInspection::Mismatch);
                }
            }
        }
        let main_pair = match (main.as_ref(), main_info.as_ref()) {
            (Some(main), Some(main_info)) => {
                Self::metadata_matches(main_info, expected, MetadataArtifactRole::Main, main)?
            }
            (Some(_), None) => return Ok(DatabaseFileInspection::Mismatch),
            (None, Some(_)) | (None, None) => false,
        };
        let wal_pair = match (wal.as_ref(), wal_info.as_ref()) {
            (Some(wal), Some(wal_info)) => {
                Self::metadata_matches(wal_info, expected, MetadataArtifactRole::Wal, wal)?
            }
            (Some(_), None) => return Ok(DatabaseFileInspection::Mismatch),
            (None, Some(_)) | (None, None) => false,
        };
        match (main.as_ref(), wal.as_ref(), main_pair, wal_pair) {
            (Some(main), Some(wal), true, true)
                if Self::file_identity(main)? != Self::file_identity(wal)? =>
            {
                Ok(DatabaseFileInspection::Matching)
            }
            (Some(_), Some(_), _, _) => Ok(DatabaseFileInspection::Mismatch),
            _ => Ok(DatabaseFileInspection::Partial),
        }
    }

    fn open_database(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<OpenDatabaseInspection<Self::DatabaseHandle>, RegistryError> {
        let main = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::Main),
            libc::O_RDWR,
        )?;
        let wal = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::Wal),
            libc::O_RDWR,
        )?;
        let main_info = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::MainInfo),
            libc::O_RDONLY,
        )?;
        let wal_info = self.open_child_optional(
            &Self::artifact_name(expected, DatabaseArtifact::WalInfo),
            libc::O_RDONLY,
        )?;
        if [
            main.as_ref(),
            wal.as_ref(),
            main_info.as_ref(),
            wal_info.as_ref(),
        ]
        .iter()
        .all(Option::is_none)
        {
            return Ok(OpenDatabaseInspection::Missing);
        }
        let (Some(main_file), Some(wal_file), Some(main_info), Some(wal_info)) =
            (main, wal, main_info, wal_info)
        else {
            return Ok(OpenDatabaseInspection::Mismatch);
        };
        if Self::inspect_open_artifact(&main_file, expected, DatabaseArtifact::Main)?
            != DatabaseFileInspection::Matching
            || Self::inspect_open_artifact(&wal_file, expected, DatabaseArtifact::Wal)?
                != DatabaseFileInspection::Matching
            || Self::inspect_open_artifact(&main_info, expected, DatabaseArtifact::MainInfo)?
                != DatabaseFileInspection::Matching
            || Self::inspect_open_artifact(&wal_info, expected, DatabaseArtifact::WalInfo)?
                != DatabaseFileInspection::Matching
            || !Self::metadata_matches(
                &main_info,
                expected,
                MetadataArtifactRole::Main,
                &main_file,
            )?
            || !Self::metadata_matches(&wal_info, expected, MetadataArtifactRole::Wal, &wal_file)?
            || Self::file_identity(&main_file)? == Self::file_identity(&wal_file)?
        {
            return Ok(OpenDatabaseInspection::Mismatch);
        }
        Ok(OpenDatabaseInspection::Matching(OsDatabaseHandle {
            main_file,
            wal_file,
            identity: expected.file_key().clone(),
        }))
    }

    fn unlink_database(&mut self, expected: &DatabaseFileExpectation) -> Result<(), RegistryError> {
        self.preflight_database_unlink(expected)?;
        for artifact in DatabaseArtifact::ALL {
            self.unlink_database_artifact(expected, artifact)?;
        }
        Ok(())
    }

    fn fsync_dir(&mut self) -> Result<(), RegistryError> {
        #[cfg(test)]
        self.fail_database_artifact_operation(DatabaseArtifactOperation::DirectorySync)?;
        #[cfg(test)]
        if let Some(hook) = &mut self.private_temporary_cleanup_test_hook {
            hook.fsync_attempts += 1;
        }
        #[cfg(test)]
        if let Some(hook) = &mut self.stage_creation_failure_test_hook {
            hook.fsync_attempts += 1;
        }
        let result = unsafe { libc::fsync(self.directory.as_raw_fd()) };
        if result == 0 {
            Ok(())
        } else {
            Err(RegistryError::Backend)
        }
    }
}
