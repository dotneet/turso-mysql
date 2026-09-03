//! Pathless Core attachment through the trusted MySQL database registry.

use std::path::Path;
use std::sync::Arc;

use turso_core::{Database, PlatformIO, PreopenedDatabaseIdentity, IO};

use crate::database_open::open_preopened_database_with_wal;
use crate::database_registry::{DatabaseName, DatabaseRegistry, OsDataRoot, RegistryError};

type OsDatabaseRegistry = DatabaseRegistry<OsDataRoot>;

/// Owns the trusted root capability and opens registered MySQL databases
/// without exposing a filesystem path to Core or to logical-database callers.
pub(crate) struct DatabaseCatalog {
    registry: OsDatabaseRegistry,
    io: Arc<dyn IO>,
}

impl DatabaseCatalog {
    /// Opens the configured data root once and retains only its capability.
    pub(crate) fn open(root_path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let root = OsDataRoot::open(root_path.as_ref())?;
        let registry = DatabaseRegistry::open_or_create(root)?;
        let io = Arc::new(PlatformIO::new().map_err(|_| RegistryError::Backend)?);
        Ok(Self { registry, io })
    }

    /// Creates, initializes, publishes, and opens one logical database.
    pub(crate) fn create(
        &mut self,
        requested_name: &str,
    ) -> Result<(DatabaseName, Arc<Database>), RegistryError> {
        let io = Arc::clone(&self.io);
        self.registry
            .create_with_initializer(requested_name, move |stage, expected, lifetime| {
                let identity = PreopenedDatabaseIdentity::new(expected.file_key().as_str())
                    .map_err(|_| RegistryError::Backend)?;
                let durable_identity = expected.file_key().to_database_identity()?;
                let main_file = stage.main_file()?;
                let wal_file = stage.wal_file()?;
                open_preopened_database_with_wal(
                    io,
                    main_file,
                    wal_file,
                    identity,
                    durable_identity,
                    lifetime,
                )
                .map_err(|_| RegistryError::Backend)
            })
    }

    /// Acquires and opens one ready logical database by its canonical name.
    pub(crate) fn acquire(&mut self, requested_name: &str) -> Result<Arc<Database>, RegistryError> {
        let lease = self.registry.acquire(requested_name)?;
        let name = lease.name().clone();
        let expected_key = lease.database_file_key().clone();
        let handle = lease.database_handle();

        // The backend must return the exact descriptors checked against this
        // lease. Compare its retained key before consuming the lease so an
        // identity swap cannot reach Core.
        if handle.identity() != &expected_key {
            return Err(RegistryError::DatabaseMarkerMismatch(name));
        }
        let identity = PreopenedDatabaseIdentity::new(expected_key.as_str())
            .map_err(|_| RegistryError::Backend)?;
        let durable_identity = expected_key.to_database_identity()?;
        let (handle, lifetime) = lease.into_core_parts();
        let main_file = handle.main_file()?;
        let wal_file = handle.wal_file()?;
        open_preopened_database_with_wal(
            Arc::clone(&self.io),
            main_file,
            wal_file,
            identity,
            durable_identity,
            lifetime,
        )
        .map_err(|_| RegistryError::Backend)
    }

    /// Drops a ready logical database after all Core references release it.
    pub(crate) fn drop_database(&mut self, requested_name: &str) -> Result<(), RegistryError> {
        self.registry.drop_database(requested_name)
    }

    /// Lists ready logical databases in canonical order.
    pub(crate) fn list(&self) -> Result<Vec<DatabaseName>, RegistryError> {
        self.registry.ready_databases()
    }

    /// Reports whether a canonical logical database is ready.
    pub(crate) fn contains(&self, requested_name: &str) -> Result<bool, RegistryError> {
        self.registry.contains(requested_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext};
    use crate::MySqlConnection;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use turso_core::{Result as CoreResult, Value};

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn binary_context() -> SchemaSqlSessionContext {
        SchemaSqlSessionContext {
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

    fn open_connection(database: &Arc<Database>) -> CoreResult<MySqlConnection> {
        MySqlConnection::new(database.connect()?, binary_context())
    }

    #[test]
    fn create_write_drop_core_refs_reopen_and_acquire_persisted_rows() -> CoreResult<()> {
        let directory = private_tempdir();
        let mut catalog = DatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        let (_, database) = catalog
            .create("Reports")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;
        assert!(catalog.contains("REPORTS").unwrap());
        assert_eq!(catalog.list().unwrap()[0].as_str(), "reports");
        assert!(database.path.is_empty());
        let connection = open_connection(&database)?;
        connection.execute("CREATE TABLE records (id INT, label TEXT)")?;
        connection.execute("INSERT INTO records (id, label) VALUES (7, 'kept')")?;
        connection.close()?;
        drop(connection);
        drop(database);
        drop(catalog);

        let mut reopened = DatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("reopen catalog".into()))?;
        let database = reopened
            .acquire("reports")
            .map_err(|_| turso_core::LimboError::InternalError("acquire database".into()))?;
        let connection = open_connection(&database)?;
        assert_eq!(
            connection
                .prepare_select("SELECT id, label FROM records")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(7), Value::from_text("kept")]]
        );
        drop(connection);
        drop(database);
        reopened
            .drop_database("reports")
            .map_err(|_| turso_core::LimboError::InternalError("drop database".into()))?;
        assert!(!reopened.contains("reports").unwrap());
        Ok(())
    }

    #[test]
    fn acquiring_a_live_database_reuses_core_cache() -> CoreResult<()> {
        let directory = private_tempdir();
        let mut catalog = DatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        let (_, first) = catalog
            .create("cache")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;
        let second = catalog
            .acquire("CACHE")
            .map_err(|_| turso_core::LimboError::InternalError("acquire database".into()))?;
        assert!(Arc::ptr_eq(&first, &second));
        drop(second);
        drop(first);
        catalog
            .drop_database("cache")
            .map_err(|_| turso_core::LimboError::InternalError("drop database".into()))?;
        Ok(())
    }

    #[test]
    fn core_connection_keeps_drop_busy_until_released() -> CoreResult<()> {
        let directory = private_tempdir();
        let mut catalog = DatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        let (_, database) = catalog
            .create("busy")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;
        let connection = open_connection(&database)?;
        drop(database);
        assert!(matches!(
            catalog.drop_database("busy"),
            Err(RegistryError::DatabaseBusy(_))
        ));
        drop(connection);
        catalog
            .drop_database("busy")
            .map_err(|_| turso_core::LimboError::InternalError("drop database".into()))?;
        Ok(())
    }

    #[test]
    fn core_lifetime_guard_keeps_the_root_locked_after_catalog_drop() -> CoreResult<()> {
        let directory = private_tempdir();
        let mut catalog = DatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        let (_, database) = catalog
            .create("held")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;
        let connection = open_connection(&database)?;
        drop(catalog);

        assert!(matches!(
            DatabaseCatalog::open(directory.path()),
            Err(RegistryError::RegistryAlreadyOpen)
        ));

        drop(connection);
        drop(database);
        let mut reopened = DatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("reopen catalog".into()))?;
        reopened
            .drop_database("held")
            .map_err(|_| turso_core::LimboError::InternalError("drop database".into()))?;
        Ok(())
    }
}
