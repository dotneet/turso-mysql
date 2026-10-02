//! Pathless opening of a MySQL database from retained main and WAL files.

use std::sync::Arc;

use turso_core::{
    Database, DatabaseOpts, OpenOptions, PreopenedDatabaseAccess, PreopenedDatabaseIdentity,
    PreopenedDatabaseWithWal, Result, SchemaCatalogValidationContext, IO,
};

use crate::MySqlDialect;

/// The environment variable that opens every database in MVCC mode, where
/// writers run side by side instead of one at a time. Off unless set to `1`.
///
/// Experimental: MVCC gives snapshot isolation with row-level write conflicts,
/// which is not what the server's isolation levels promise yet.
pub const EXPERIMENTAL_MVCC_VARIABLE: &str = "TURSO_MYSQL_EXPERIMENTAL_MVCC";

/// Whether databases open in MVCC mode, read once per process.
pub fn experimental_mvcc_is_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var(EXPERIMENTAL_MVCC_VARIABLE).is_ok_and(|value| value == "1"))
}

/// Opens a MySQL database from already-open main and WAL descriptors.
///
/// The descriptors are transferred to Core without resolving either a path or
/// a WAL sidecar. `identity` must have been validated by
/// [`PreopenedDatabaseIdentity::new`], while `durable_identity` is the value
/// used by the MySQL schema catalog and must match the registry proof supplied
/// by the caller. Core retains `guard` until all database connections are gone.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn open_preopened_database_with_wal<G>(
    io: Arc<dyn IO>,
    main_file: std::fs::File,
    wal_file: std::fs::File,
    identity: PreopenedDatabaseIdentity,
    durable_identity: [u8; 16],
    logical_name: &str,
    mvcc_log_file: Option<std::fs::File>,
    guard: G,
) -> Result<Arc<Database>>
where
    G: Send + Sync + 'static,
{
    let mut database = PreopenedDatabaseWithWal::from_std_files(
        main_file,
        identity.clone(),
        PreopenedDatabaseAccess::ReadWrite,
        wal_file,
        identity,
        PreopenedDatabaseAccess::ReadWrite,
    )?
    .with_durable_identity(durable_identity)
    .with_lifetime_guard(Arc::new(guard));
    let switch_to_mvcc = mvcc_log_file.is_some();
    if let Some(file) = mvcc_log_file {
        database = database.with_logical_log_std_file(file)?;
    }

    let database = Database::open_preopened_with_wal(
        io,
        database,
        OpenOptions::new(Arc::new(MySqlDialect))
            .schema_catalog_validation_context(SchemaCatalogValidationContext::new(
                durable_identity,
            ))
            // VACUUM remains disabled until the registry owns the real WAL
            // sidecar lifecycle; a pre-opened capability has no path to use.
            .db_opts(
                DatabaseOpts::new()
                    .with_views(true)
                    .with_mvcc_row_locks(switch_to_mvcc),
            ),
    )?;
    // One database is one logical database, so the `information_schema` tables
    // it answers know their own name from here rather than from the engine,
    // which has no notion of one.
    crate::catalog_tables::register_catalog_tables(&database, logical_name)?;
    if switch_to_mvcc && !database.mvcc_enabled() {
        let connection = database.connect()?;
        connection.execute("PRAGMA journal_mode = 'mvcc'")?;
        connection.close()?;
    }
    if let Some(store) = database.get_mv_store().as_ref() {
        store.set_exclusive_tx_and_writers_wait(true);
    }
    Ok(database)
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
        let db = open_preopened_database_with_wal(
            io,
            main,
            wal,
            opaque_identity(),
            identity(1),
            "probe",
            None,
            (),
        )?;
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

    fn binary_context() -> crate::schema_sql::SchemaSqlSessionContext {
        use crate::schema_sql::{CharacterSet, Collation, SchemaSqlMode};
        crate::schema_sql::SchemaSqlSessionContext {
            sql_mode: SchemaSqlMode {
                ansi_quotes: false,
                no_backslash_escapes: false,
            },
            character_set_client: CharacterSet::Binary,
            collation_connection: Collation::Binary,
            default_character_set: CharacterSet::Binary,
            default_collation: Collation::Binary,
        }
    }

    fn rows_of(connection: &crate::MySqlConnection) -> Result<Vec<Vec<Value>>> {
        connection
            .prepare_select("SELECT id FROM records ORDER BY id")?
            .run_collect_rows()
    }

    /// A database handed a logical log runs in MVCC mode, where a write left
    /// in a transaction the session never sees committed would stay invisible
    /// to every other session and lost on reopen.
    #[test]
    fn a_database_given_a_logical_log_commits_rows_every_session_sees() -> Result<()> {
        let (directory, main, wal) = files();
        let log = || {
            FsOpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.path().join("main.db-log"))
                .unwrap()
        };
        let open = |main: std::fs::File, wal: std::fs::File| {
            open_preopened_database_with_wal(
                Arc::new(NoPathIo),
                main,
                wal,
                opaque_identity(),
                identity(9),
                "probe",
                Some(log()),
                (),
            )
        };
        let db = open(main.try_clone().unwrap(), wal.try_clone().unwrap())?;
        assert!(db.mvcc_enabled());
        let writer = crate::MySqlConnection::new(db.connect()?, binary_context())?;
        let reader = crate::MySqlConnection::new(db.connect()?, binary_context())?;
        writer.execute("CREATE TABLE records (id INT)")?;
        // The table is not checkpointed yet, so its catalog row names it by a
        // negative root.
        let table = turso_mysql_parser::MySqlTableName::parse("records").unwrap();
        let columns = reader.list_columns(&table).unwrap();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name(), "id");
        writer.execute("INSERT INTO records (id) VALUES (1)")?;
        assert_eq!(rows_of(&reader)?, vec![vec![Value::from_i64(1)]]);

        writer.execute_transaction_command("BEGIN").unwrap();
        reader.execute_transaction_command("BEGIN").unwrap();
        writer.execute("INSERT INTO records (id) VALUES (2)")?;
        reader.execute("INSERT INTO records (id) VALUES (3)")?;
        writer.execute_transaction_command("COMMIT").unwrap();
        reader.execute_transaction_command("COMMIT").unwrap();
        writer.close()?;
        reader.close()?;
        drop((writer, reader, db));

        let reopened = open(main, wal)?;
        let connection = crate::MySqlConnection::new(reopened.connect()?, binary_context())?;
        assert_eq!(
            rows_of(&connection)?,
            vec![
                vec![Value::from_i64(1)],
                vec![Value::from_i64(2)],
                vec![Value::from_i64(3)]
            ]
        );
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
            "probe",
            None,
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
            "probe",
            None,
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
            "probe",
            None,
            (),
        )?;
        let second = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            second_main,
            second_wal,
            opaque_identity(),
            identity(3),
            "probe",
            None,
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
            "probe",
            None,
            (),
        )?;
        let pathless_identity = "main.db";
        let wal_error = open_preopened_database_with_wal(
            Arc::new(NoPathIo),
            mismatched_wal_main,
            other_wal,
            opaque_identity(),
            identity(4),
            "probe",
            None,
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
            "probe",
            None,
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
