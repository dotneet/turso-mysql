//! Pathless Core attachment through the trusted MySQL database registry.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use turso_core::{Database, PlatformIO, PreopenedDatabaseIdentity, IO};
use turso_mysql_parser::{parse_admin_command, MySqlAdminCommand, SessionSqlMode};

use crate::database_open::open_preopened_database_with_wal;
use crate::database_registry::{DatabaseName, DatabaseRegistry, OsDataRoot, RegistryError};
use crate::schema_sql::SchemaSqlSessionContext;
use crate::MySqlConnection;

type OsDatabaseRegistry = DatabaseRegistry<OsDataRoot>;

/// Errors returned by the public MySQL logical-database API.
///
/// The registry intentionally keeps filesystem and opaque-file details
/// private. This type preserves the action a protocol adapter needs to take
/// without disclosing those details to a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlDatabaseError {
    /// The supplied logical database name is not accepted by this server.
    InvalidDatabaseName,
    /// A ready or in-progress logical database already has this name.
    DatabaseAlreadyExists(String),
    /// No ready logical database has this name.
    DatabaseNotFound(String),
    /// The database is selected by a live session and cannot be dropped.
    DatabaseBusy(String),
    /// The database has not completed creation or removal.
    DatabaseNotReady(String),
    /// The selected database has not passed its durable identity checks.
    DatabaseIntegrity,
    /// The catalog cannot safely perform the requested operation.
    CatalogUnavailable,
    /// A Core connection could not be opened for a selected database.
    ConnectionUnavailable,
    /// A session has not selected a logical database.
    NoDatabaseSelected,
}

impl fmt::Display for MySqlDatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDatabaseName => f.write_str("invalid database name"),
            Self::DatabaseAlreadyExists(name) => write!(f, "database already exists: {name}"),
            Self::DatabaseNotFound(name) => write!(f, "unknown database: {name}"),
            Self::DatabaseBusy(name) => write!(f, "database is in use: {name}"),
            Self::DatabaseNotReady(name) => write!(f, "database is not ready: {name}"),
            Self::DatabaseIntegrity => f.write_str("database identity validation failed"),
            Self::CatalogUnavailable => f.write_str("database catalog is unavailable"),
            Self::ConnectionUnavailable => f.write_str("database connection is unavailable"),
            Self::NoDatabaseSelected => f.write_str("no database selected"),
        }
    }
}

impl Error for MySqlDatabaseError {}

/// The typed result of one trusted embedded database-management command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlAdminCommandResult {
    /// A logical database was created and published.
    Created { database: String },
    /// A logical database was dropped.
    Dropped { database: String },
    /// A session now selects the named logical database.
    Selected { database: String },
}

/// Errors returned by the trusted embedded admin-command API.
///
/// Syntax rejection deliberately carries no parser detail. Protocol callers
/// can turn it into a client syntax error without exposing parser internals,
/// filesystem paths, or registry state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlAdminCommandError {
    /// The input was not one strict single-statement admin command.
    Syntax,
    /// The catalog rejected a valid command.
    Database(MySqlDatabaseError),
}

impl fmt::Display for MySqlAdminCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax => f.write_str("syntax error"),
            Self::Database(error) => error.fmt(f),
        }
    }
}

impl Error for MySqlAdminCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Syntax => None,
            Self::Database(error) => Some(error),
        }
    }
}

impl From<MySqlDatabaseError> for MySqlAdminCommandError {
    fn from(error: MySqlDatabaseError) -> Self {
        Self::Database(error)
    }
}

impl From<RegistryError> for MySqlDatabaseError {
    fn from(error: RegistryError) -> Self {
        match error {
            RegistryError::EmptyDatabaseName
            | RegistryError::DatabaseNameTooLong
            | RegistryError::NulInDatabaseName
            | RegistryError::SeparatorInDatabaseName
            | RegistryError::NonAsciiDatabaseName
            | RegistryError::InvalidDatabaseNameCharacter
            | RegistryError::ReservedDatabaseName
            | RegistryError::NonCanonicalDatabaseName => Self::InvalidDatabaseName,
            RegistryError::DatabaseAlreadyExists(name) => {
                Self::DatabaseAlreadyExists(name.as_str().to_owned())
            }
            RegistryError::DatabaseNotFound(name) => {
                Self::DatabaseNotFound(name.as_str().to_owned())
            }
            RegistryError::DatabaseBusy(name) => Self::DatabaseBusy(name.as_str().to_owned()),
            RegistryError::DatabaseNotReady(name) => {
                Self::DatabaseNotReady(name.as_str().to_owned())
            }
            RegistryError::DatabaseMarkerMismatch(_) => Self::DatabaseIntegrity,
            RegistryError::InvalidOpaqueFileKey
            | RegistryError::DuplicateOpaqueFileKey
            | RegistryError::UnsupportedManifestVersion(_)
            | RegistryError::UnsupportedNamePolicy
            | RegistryError::Backend
            | RegistryError::RegistryAlreadyOpen
            | RegistryError::RegistryPoisoned
            | RegistryError::InvalidRegistryState => Self::CatalogUnavailable,
        }
    }
}

/// Public, pathless owner of one trusted MySQL logical-database catalog.
///
/// The configured root is used only while opening the catalog. Logical
/// callers can create, list, select, and drop names without receiving paths,
/// registry entries, or database descriptors.
pub struct MySqlDatabaseCatalog {
    inner: Mutex<DatabaseCatalog>,
}

impl MySqlDatabaseCatalog {
    /// Open a MySQL catalog rooted at `root_path`.
    pub fn open(root_path: impl AsRef<Path>) -> Result<Arc<Self>, MySqlDatabaseError> {
        let catalog = DatabaseCatalog::open(root_path).map_err(MySqlDatabaseError::from)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(catalog),
        }))
    }

    /// Create and publish an empty logical database, returning its canonical name.
    pub fn create(&self, requested_name: &str) -> Result<String, MySqlDatabaseError> {
        let mut catalog = self.lock()?;
        let (name, database) = catalog
            .create(requested_name)
            .map_err(MySqlDatabaseError::from)?;
        drop(database);
        Ok(name.as_str().to_owned())
    }

    /// Drop a logical database once no session still selects it.
    pub fn drop_database(&self, requested_name: &str) -> Result<(), MySqlDatabaseError> {
        self.lock()?
            .drop_database(requested_name)
            .map_err(MySqlDatabaseError::from)
    }

    /// List ready logical databases in canonical order.
    pub fn list(&self) -> Result<Vec<String>, MySqlDatabaseError> {
        Ok(self
            .lock()?
            .list()
            .map_err(MySqlDatabaseError::from)?
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .collect())
    }

    /// Make a session with immutable MySQL schema settings.
    pub fn new_session(
        self: &Arc<Self>,
        schema_context: SchemaSqlSessionContext,
    ) -> MySqlDatabaseSession {
        MySqlDatabaseSession {
            catalog: Arc::clone(self),
            schema_context,
            selected: None,
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, DatabaseCatalog>, MySqlDatabaseError> {
        self.inner
            .lock()
            .map_err(|_| MySqlDatabaseError::CatalogUnavailable)
    }
}

/// One client session and its currently selected MySQL logical database.
pub struct MySqlDatabaseSession {
    catalog: Arc<MySqlDatabaseCatalog>,
    schema_context: SchemaSqlSessionContext,
    selected: Option<(String, MySqlConnection)>,
}

impl MySqlDatabaseSession {
    /// Executes one strict database-management command in this trusted
    /// embedded session.
    ///
    /// This is intentionally not a network authorization boundary. A server
    /// adapter must authenticate and authorize before calling this API.
    pub fn execute_admin_command(
        &mut self,
        sql: &str,
    ) -> Result<MySqlAdminCommandResult, MySqlAdminCommandError> {
        let command = parse_admin_command(sql, self.parser_mode())
            .map_err(|_| MySqlAdminCommandError::Syntax)?;
        match command {
            MySqlAdminCommand::CreateDatabase { name } => {
                let database = self.catalog.create(name.as_str())?;
                Ok(MySqlAdminCommandResult::Created { database })
            }
            MySqlAdminCommand::DropDatabase { name } => {
                let database = name.into_string();
                if self.selected_database() == Some(database.as_str()) {
                    return Err(MySqlDatabaseError::DatabaseBusy(database).into());
                }
                self.catalog.drop_database(&database)?;
                Ok(MySqlAdminCommandResult::Dropped { database })
            }
            MySqlAdminCommand::Use { name } => {
                let database = name.into_string();
                self.select_database(&database)?;
                Ok(MySqlAdminCommandResult::Selected { database })
            }
        }
    }

    /// Select a ready database, preserving the prior selection if opening fails.
    pub fn select_database(&mut self, requested_name: &str) -> Result<(), MySqlDatabaseError> {
        let canonical_name = DatabaseName::parse(requested_name)
            .map_err(MySqlDatabaseError::from)?
            .as_str()
            .to_owned();
        let selected = {
            let mut catalog = self.catalog.lock()?;
            let database = catalog
                .acquire(&canonical_name)
                .map_err(MySqlDatabaseError::from)?;
            let connection = database
                .connect()
                .map_err(|_| MySqlDatabaseError::ConnectionUnavailable)?;
            let connection = MySqlConnection::new(connection, self.schema_context)
                .map_err(|_| MySqlDatabaseError::ConnectionUnavailable)?;
            (canonical_name, connection)
        };

        self.selected = Some(selected);
        Ok(())
    }

    /// Return the canonical selected database name, if any.
    pub fn selected_database(&self) -> Option<&str> {
        self.selected.as_ref().map(|(name, _)| name.as_str())
    }

    /// Return the selected connection for checked MySQL statement execution.
    pub fn connection(&self) -> Result<&MySqlConnection, MySqlDatabaseError> {
        self.selected
            .as_ref()
            .map(|(_, connection)| connection)
            .ok_or(MySqlDatabaseError::NoDatabaseSelected)
    }

    fn parser_mode(&self) -> SessionSqlMode {
        SessionSqlMode {
            ansi_quotes: self.schema_context.sql_mode.ansi_quotes,
            no_backslash_escapes: self.schema_context.sql_mode.no_backslash_escapes,
        }
    }
}

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
    use crate::schema_sql::{CharacterSet, Collation, SchemaSqlMode};
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

    #[test]
    fn public_sessions_share_one_catalog_and_see_each_others_rows() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        assert_eq!(catalog.create("Reports").unwrap(), "reports");

        let mut writer = catalog.new_session(binary_context());
        let mut reader = catalog.new_session(binary_context());
        writer
            .select_database("REPORTS")
            .map_err(|_| turso_core::LimboError::InternalError("select writer".into()))?;
        writer
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("writer connection".into()))?
            .execute("CREATE TABLE records (id INT, label TEXT)")?;
        writer
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("writer connection".into()))?
            .execute("INSERT INTO records (id, label) VALUES (7, 'kept')")?;

        reader
            .select_database("reports")
            .map_err(|_| turso_core::LimboError::InternalError("select reader".into()))?;
        assert_eq!(writer.selected_database(), Some("reports"));
        assert_eq!(reader.selected_database(), Some("reports"));
        assert_eq!(
            reader
                .connection()
                .map_err(|_| turso_core::LimboError::InternalError("reader connection".into()))?
                .prepare_select("SELECT id, label FROM records")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(7), Value::from_text("kept")]]
        );
        assert!(matches!(
            catalog.drop_database("reports"),
            Err(MySqlDatabaseError::DatabaseBusy(name)) if name == "reports"
        ));
        Ok(())
    }

    #[test]
    fn successful_selection_releases_the_previous_database_lease() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog
            .create("first")
            .map_err(|_| turso_core::LimboError::InternalError("create first".into()))?;
        catalog
            .create("second")
            .map_err(|_| turso_core::LimboError::InternalError("create second".into()))?;

        let mut session = catalog.new_session(binary_context());
        session
            .select_database("first")
            .map_err(|_| turso_core::LimboError::InternalError("select first".into()))?;
        session
            .select_database("second")
            .map_err(|_| turso_core::LimboError::InternalError("select second".into()))?;
        assert_eq!(session.selected_database(), Some("second"));
        catalog
            .drop_database("first")
            .map_err(|_| turso_core::LimboError::InternalError("drop first".into()))?;
        assert!(matches!(
            catalog.drop_database("second"),
            Err(MySqlDatabaseError::DatabaseBusy(name)) if name == "second"
        ));
        Ok(())
    }

    #[test]
    fn failed_selection_keeps_the_previous_connection_selected() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog
            .create("kept")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;
        let mut session = catalog.new_session(binary_context());
        session
            .select_database("kept")
            .map_err(|_| turso_core::LimboError::InternalError("select database".into()))?;

        assert!(matches!(
            session.select_database("missing"),
            Err(MySqlDatabaseError::DatabaseNotFound(name)) if name == "missing"
        ));
        assert_eq!(session.selected_database(), Some("kept"));
        session
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("selected connection".into()))?
            .execute("CREATE TABLE still_selected (id INT)")?;
        assert!(matches!(
            catalog.drop_database("kept"),
            Err(MySqlDatabaseError::DatabaseBusy(name)) if name == "kept"
        ));
        Ok(())
    }

    #[test]
    fn unselected_session_has_no_connection() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let session = catalog.new_session(binary_context());
        assert_eq!(session.selected_database(), None);
        assert!(matches!(
            session.connection(),
            Err(MySqlDatabaseError::NoDatabaseSelected)
        ));
    }

    #[test]
    fn trusted_admin_session_executes_create_use_and_drop() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let mut session = catalog.new_session(binary_context());

        assert_eq!(
            session.execute_admin_command("CREATE DATABASE Reports"),
            Ok(MySqlAdminCommandResult::Created {
                database: "reports".to_owned(),
            })
        );
        assert_eq!(
            session.execute_admin_command("USE REPORTS;"),
            Ok(MySqlAdminCommandResult::Selected {
                database: "reports".to_owned(),
            })
        );

        assert_eq!(
            session.execute_admin_command("CREATE DATABASE Archive"),
            Ok(MySqlAdminCommandResult::Created {
                database: "archive".to_owned(),
            })
        );
        assert_eq!(
            session.execute_admin_command("DROP DATABASE Archive"),
            Ok(MySqlAdminCommandResult::Dropped {
                database: "archive".to_owned(),
            })
        );
        assert_eq!(catalog.list().unwrap(), vec!["reports"]);
    }

    #[test]
    fn admin_parser_rejects_compounds_comments_and_unimplemented_options() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let mut session = catalog.new_session(binary_context());
        let root_path = directory.path().to_string_lossy().into_owned();

        for sql in [
            "CREATE DATABASE one; DROP DATABASE two",
            "CREATE DATABASE one -- comment",
            "CREATE DATABASE IF NOT EXISTS one",
            "DROP DATABASE IF EXISTS one",
            "USE one /* comment */",
        ] {
            let error = session.execute_admin_command(sql).unwrap_err();
            assert_eq!(error, MySqlAdminCommandError::Syntax);
            assert_eq!(error.to_string(), "syntax error");
            assert!(!error.to_string().contains(&root_path));
        }
        assert!(catalog.list().unwrap().is_empty());
        assert_eq!(session.selected_database(), None);
    }

    #[test]
    fn failed_admin_commands_leave_catalog_and_selection_unchanged() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let mut session = catalog.new_session(binary_context());
        session
            .execute_admin_command("CREATE DATABASE Kept")
            .unwrap();
        session.execute_admin_command("USE kept").unwrap();

        assert!(matches!(
            session.execute_admin_command("USE missing"),
            Err(MySqlAdminCommandError::Database(
                MySqlDatabaseError::DatabaseNotFound(name)
            )) if name == "missing"
        ));
        assert_eq!(session.selected_database(), Some("kept"));
        assert_eq!(catalog.list().unwrap(), vec!["kept"]);

        assert!(matches!(
            session.execute_admin_command("CREATE DATABASE KEPT"),
            Err(MySqlAdminCommandError::Database(
                MySqlDatabaseError::DatabaseAlreadyExists(name)
            )) if name == "kept"
        ));
        assert_eq!(session.selected_database(), Some("kept"));
        assert_eq!(catalog.list().unwrap(), vec!["kept"]);
    }

    #[test]
    fn two_sessions_keep_selected_database_busy_for_drop() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let mut first = catalog.new_session(binary_context());
        let mut second = catalog.new_session(binary_context());
        first
            .execute_admin_command("CREATE DATABASE Shared")
            .unwrap();
        first.execute_admin_command("USE shared").unwrap();
        second.execute_admin_command("USE SHARED").unwrap();

        assert!(matches!(
            first.execute_admin_command("DROP DATABASE shared"),
            Err(MySqlAdminCommandError::Database(
                MySqlDatabaseError::DatabaseBusy(name)
            )) if name == "shared"
        ));
        assert_eq!(first.selected_database(), Some("shared"));
        assert_eq!(second.selected_database(), Some("shared"));
        assert_eq!(catalog.list().unwrap(), vec!["shared"]);
    }
}
