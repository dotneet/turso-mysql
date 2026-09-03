//! Pathless opening of a MySQL database from retained main and WAL files.

use std::sync::Arc;

use turso_core::{
    Database, DatabaseOpts, OpenOptions, PreopenedDatabaseAccess, PreopenedDatabaseIdentity,
    PreopenedDatabaseWithWal, Result, SchemaCatalogValidationContext, IO,
};

use crate::MySqlDialect;

/// Opens a MySQL database from already-open main and WAL descriptors.
///
/// The descriptors are transferred to Core without resolving either a path or
/// a WAL sidecar. `identity` must have been validated by
/// [`PreopenedDatabaseIdentity::new`], while `durable_identity` is the value
/// used by the MySQL schema catalog and must match the registry proof supplied
/// by the caller. Core retains `guard` until all database connections are gone.
#[allow(dead_code)]
pub(crate) fn open_preopened_database_with_wal<G>(
    io: Arc<dyn IO>,
    main_file: std::fs::File,
    wal_file: std::fs::File,
    identity: PreopenedDatabaseIdentity,
    durable_identity: [u8; 16],
    guard: G,
) -> Result<Arc<Database>>
where
    G: Send + Sync + 'static,
{
    let database = PreopenedDatabaseWithWal::from_std_files(
        main_file,
        identity.clone(),
        PreopenedDatabaseAccess::ReadWrite,
        wal_file,
        identity,
        PreopenedDatabaseAccess::ReadWrite,
    )?
    .with_durable_identity(durable_identity)
    .with_lifetime_guard(Arc::new(guard));

    Database::open_preopened_with_wal(
        io,
        database,
        OpenOptions::new(Arc::new(MySqlDialect))
            .schema_catalog_validation_context(SchemaCatalogValidationContext::new(
                durable_identity,
            ))
            // VACUUM remains disabled until the registry owns the real WAL
            // sidecar lifecycle; a pre-opened capability has no path to use.
            .db_opts(DatabaseOpts::new().with_views(true)),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::OpenOptions as FsOpenOptions;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use tempfile::TempDir;
    use turso_core::{Clock, File, LimboError, OpenFlags, Value, IO};

    use super::*;

    struct NoPathIo;

    impl Clock for NoPathIo {
        fn current_time_monotonic(&self) -> turso_core::MonotonicInstant {
            turso_core::io::clock::DefaultClock.current_time_monotonic()
        }

        fn current_time_wall_clock(&self) -> turso_core::WallClockInstant {
            turso_core::io::clock::DefaultClock.current_time_wall_clock()
        }
    }

    impl IO for NoPathIo {
        fn open_file(
            &self,
            _path: &str,
            _flags: OpenFlags,
            _direct: bool,
        ) -> turso_core::Result<Arc<dyn File>> {
            panic!("pre-opened MySQL open must not resolve a path")
        }

        fn remove_file(&self, _path: &str) -> turso_core::Result<()> {
            panic!("pre-opened MySQL open must not remove a path")
        }

        fn file_id(&self, _path: &str) -> turso_core::Result<turso_core::io::FileId> {
            panic!("pre-opened MySQL open must not look up a path identity")
        }
    }

    fn files() -> (TempDir, std::fs::File, std::fs::File) {
        let directory = tempfile::tempdir().unwrap();
        let main_path = directory.path().join("main.db");
        let wal_path = directory.path().join("main.db-wal");
        let main = FsOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(main_path)
            .unwrap();
        let wal = FsOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(wal_path)
            .unwrap();
        (directory, main, wal)
    }

    fn identity(value: u8) -> [u8; 16] {
        [value; 16]
    }

    fn opaque_identity() -> PreopenedDatabaseIdentity {
        PreopenedDatabaseIdentity::new("db_0123456789abcdef0123456789abcdef").unwrap()
    }

    #[test]
    fn opens_empty_real_main_and_wal_with_mysql_application_id() -> Result<()> {
        let (_directory, main, wal) = files();
        let io: Arc<dyn IO> = Arc::new(NoPathIo);
        let db =
            open_preopened_database_with_wal(io, main, wal, opaque_identity(), identity(1), ())?;
        let connection = db.connect()?;
        assert_eq!(
            connection
                .prepare("PRAGMA application_id")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(i64::from(
                turso_core::DatabaseFileOwner::mysql_application_id(
                    turso_core::DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
                )
            ),)]]
        );
        assert!(connection.experimental_views_enabled());
        assert!(!connection.experimental_vacuum_enabled());
        connection.close()?;
        Ok(())
    }

    struct DropGuard(Arc<AtomicUsize>);

    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn retains_guard_until_the_last_connection_is_dropped() -> Result<()> {
        let (_directory, main, wal) = files();
        let drops = Arc::new(AtomicUsize::new(0));
        let db = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            main,
            wal,
            opaque_identity(),
            identity(2),
            DropGuard(drops.clone()),
        )?;
        let connection = db.connect()?;
        drop(db);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        connection.close()?;
        drop(connection);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn rejects_a_zero_durable_identity() {
        let (_directory, main, wal) = files();
        let error = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            main,
            wal,
            opaque_identity(),
            [0; 16],
            (),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must be nonzero"));
    }

    #[test]
    fn same_inode_descriptor_clones_reopen_the_same_core_database() -> Result<()> {
        let (_directory, main, wal) = files();
        let second_main = main.try_clone().unwrap();
        let second_wal = wal.try_clone().unwrap();
        let first = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            main,
            wal,
            opaque_identity(),
            identity(3),
            (),
        )?;
        let second = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            second_main,
            second_wal,
            opaque_identity(),
            identity(3),
            (),
        )?;
        assert!(Arc::ptr_eq(&first, &second));
        Ok(())
    }

    #[test]
    fn rejects_a_different_wal_or_identity_without_path_errors() -> Result<()> {
        let (_directory, main, wal) = files();
        let (_other_directory, _other_main, other_wal) = files();
        let mismatched_wal_main = main.try_clone().unwrap();
        let mismatched_identity_main = main.try_clone().unwrap();
        let mismatched_identity_wal = wal.try_clone().unwrap();
        let _first = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            main,
            wal,
            opaque_identity(),
            identity(4),
            (),
        )?;
        let pathless_identity = "main.db";
        let wal_error = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            mismatched_wal_main,
            other_wal,
            opaque_identity(),
            identity(4),
            (),
        )
        .unwrap_err();
        assert!(matches!(wal_error, LimboError::InvalidArgument(_)));
        assert!(!wal_error.to_string().contains(pathless_identity));

        let identity_error = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            mismatched_identity_main,
            mismatched_identity_wal,
            PreopenedDatabaseIdentity::new("different-identity").unwrap(),
            identity(4),
            (),
        )
        .unwrap_err();
        assert!(matches!(identity_error, LimboError::InvalidArgument(_)));
        assert!(!identity_error.to_string().contains(pathless_identity));
        Ok(())
    }

    #[test]
    fn opaque_identity_validation_is_pathless() {
        assert!(PreopenedDatabaseIdentity::new("db_opaque-token").is_ok());
        assert!(PreopenedDatabaseIdentity::new("../main.db").is_err());
    }
}
