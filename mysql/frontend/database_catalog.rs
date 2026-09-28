//! Pathless Core attachment through the trusted MySQL database registry.

use crate::named_locks::MySqlNamedLocks;
use crate::session_registry::MySqlSessionRegistry;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use turso_core::io::FileSyncType;
use turso_core::storage::auto_increment::{
    AllocatorDatabaseIdentity, AllocatorOpenMode, DurableRangeAllocator,
};
use turso_core::{Database, IOExt as _, PlatformIO, PreopenedDatabaseIdentity, IO};
use turso_mysql_parser::{
    parse_optional_admin_command, MySqlAdminCommand, MySqlTableCollation, ParseError,
    SessionSqlMode,
};

use crate::database_open::open_preopened_database_with_wal;
use crate::database_registry::{DatabaseName, DatabaseRegistry, OsDataRoot, RegistryError};
use crate::database_users::{DatabaseUser, DatabaseUsers, DropWaitError};
use crate::schema_sql::SchemaSqlSessionContext;
use crate::session::SharedDatabaseCollation;
use crate::wal_keeper::WalKeeper;
use crate::{MySqlConnection, MySqlPreparedStatementAuthority, MySqlQueryError};

type OsDatabaseRegistry = DatabaseRegistry<OsDataRoot>;

struct AllocatorDatabaseLifetime<L> {
    _lifetime: L,
    _allocator: DurableRangeAllocator,
}

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
    /// A `DROP DATABASE` gave up waiting for the sessions using the database.
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
    /// `CREATE DATABASE IF NOT EXISTS` found the database already there and
    /// left it as it stands.
    AlreadyExists { database: String },
    /// A logical database was given a new collation for the tables made in
    /// it from now on, or had its options restated.
    Altered { database: String },
    /// A logical database was dropped.
    Dropped { database: String },
    /// The `CREATE DATABASE` that makes a database as it is now, beside the
    /// name it was asked for by.
    CreateStatement { database: String, statement: String },
    /// A session now selects the named logical database.
    Selected { database: String },
    /// The catalog's ready logical databases in canonical order.
    Listed { databases: Vec<String> },
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
    /// The command is one MySQL takes and this server refuses.
    Unsupported,
    /// The command named a collation MySQL does not have.
    UnknownCollation,
    /// The command named a character set MySQL does not have.
    UnknownCharacterSet,
    /// The command named a collation beside another character set than its
    /// own.
    CollationOfAnotherCharacterSet,
    /// The command named two different character sets.
    ConflictingCharacterSets,
    /// The catalog rejected a valid command.
    Database(MySqlDatabaseError),
}

impl fmt::Display for MySqlAdminCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax => f.write_str("syntax error"),
            Self::Unsupported => f.write_str("not supported"),
            Self::UnknownCollation => f.write_str("unknown collation"),
            Self::UnknownCharacterSet => f.write_str("unknown character set"),
            Self::CollationOfAnotherCharacterSet => {
                f.write_str("collation is not valid for the character set")
            }
            Self::ConflictingCharacterSets => f.write_str("conflicting character sets"),
            Self::Database(error) => error.fmt(f),
        }
    }
}

impl Error for MySqlAdminCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MySqlDatabaseError> for MySqlAdminCommandError {
    fn from(error: MySqlDatabaseError) -> Self {
        Self::Database(error)
    }
}

impl From<ParseError> for MySqlAdminCommandError {
    fn from(error: ParseError) -> Self {
        match error {
            ParseError::Unsupported { .. } => Self::Unsupported,
            ParseError::UnknownCollation => Self::UnknownCollation,
            ParseError::UnknownCharacterSet => Self::UnknownCharacterSet,
            ParseError::CollationOfAnotherCharacterSet => Self::CollationOfAnotherCharacterSet,
            ParseError::ConflictingCharacterSets => Self::ConflictingCharacterSets,
            _ => Self::Syntax,
        }
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

/// Validate and canonicalize one MySQL logical-database name.
///
/// This is a pure name-policy check. It does not open, lock, or query a
/// database catalog, so callers can authorize a requested name before they
/// learn whether the database exists.
pub fn canonicalize_database_name(requested_name: &str) -> Result<String, MySqlDatabaseError> {
    Ok(DatabaseName::parse(requested_name)
        .map_err(MySqlDatabaseError::from)?
        .as_str()
        .to_owned())
}

/// Public, pathless owner of one trusted MySQL logical-database catalog.
///
/// The configured root is used only while opening the catalog. Logical
/// callers can create, list, select, and drop names without receiving paths,
/// registry entries, or database descriptors.
pub struct MySqlDatabaseCatalog {
    /// Declared first so that its thread is stopped before the databases it
    /// empties the WALs of are closed.
    wal_keeper: WalKeeper,
    inner: Mutex<DatabaseCatalog>,
    /// The named locks every session of this server shares.
    named_locks: Arc<MySqlNamedLocks>,
    /// The sessions logged in to this server, and when it opened.
    sessions: Arc<MySqlSessionRegistry>,
}

impl MySqlDatabaseCatalog {
    /// Open a MySQL catalog rooted at `root_path`.
    pub fn open(root_path: impl AsRef<Path>) -> Result<Arc<Self>, MySqlDatabaseError> {
        let catalog = DatabaseCatalog::open(root_path).map_err(MySqlDatabaseError::from)?;
        Ok(Arc::new(Self {
            wal_keeper: WalKeeper::start(),
            inner: Mutex::new(catalog),
            named_locks: Arc::default(),
            sessions: Arc::default(),
        }))
    }

    /// The named locks every session of this server shares.
    pub fn named_locks(&self) -> &Arc<MySqlNamedLocks> {
        &self.named_locks
    }

    /// The sessions logged in to this server, and when it opened.
    pub fn sessions(&self) -> &Arc<MySqlSessionRegistry> {
        &self.sessions
    }

    /// Create and publish an empty logical database, returning its canonical name.
    pub fn create(&self, requested_name: &str) -> Result<String, MySqlDatabaseError> {
        self.create_with_collation(requested_name, MySqlTableCollation::default())
    }

    /// Create and publish an empty logical database whose tables take
    /// `collation` when they name none, returning its canonical name.
    pub fn create_with_collation(
        &self,
        requested_name: &str,
        collation: MySqlTableCollation,
    ) -> Result<String, MySqlDatabaseError> {
        let mut catalog = self.lock()?;
        let (name, database) = catalog
            .create_with_collation(requested_name, collation)
            .map_err(MySqlDatabaseError::from)?;
        drop(database);
        Ok(name.as_str().to_owned())
    }

    /// The collation a database gives the tables made in it that name none.
    pub fn collation(
        &self,
        requested_name: &str,
    ) -> Result<MySqlTableCollation, MySqlDatabaseError> {
        self.lock()?
            .collation(requested_name)
            .map_err(MySqlDatabaseError::from)
    }

    /// Durably changes the collation a database gives the tables made in it
    /// from now on, for every session. The tables already there keep theirs.
    pub fn set_collation(
        &self,
        requested_name: &str,
        collation: MySqlTableCollation,
    ) -> Result<(), MySqlDatabaseError> {
        self.lock()?
            .set_collation(requested_name, collation)
            .map_err(MySqlDatabaseError::from)
    }

    /// Drops a logical database once no session is running a statement on it
    /// or holding a transaction open on it, waiting at most `wait` for them.
    ///
    /// A session that merely has the database selected holds nothing up: its
    /// next statement on the database answers 1049, and the files are removed
    /// while it still holds them open, which Unix lets it do without either
    /// side seeing the other.
    pub fn drop_database(
        &self,
        requested_name: &str,
        wait: std::time::Duration,
    ) -> Result<(), MySqlDatabaseError> {
        let users = self
            .lock()?
            .users_of_a_ready_database(requested_name)
            .map_err(MySqlDatabaseError::from)?;
        let dropping = users.wait_to_drop(wait).map_err(|error| match error {
            DropWaitError::AlreadyDropped => {
                MySqlDatabaseError::DatabaseNotFound(requested_name.to_owned())
            }
            DropWaitError::TimedOut => MySqlDatabaseError::DatabaseBusy(requested_name.to_owned()),
        })?;
        self.lock()?
            .drop_database(requested_name)
            .map_err(MySqlDatabaseError::from)?;
        dropping.finish();
        Ok(())
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
        self.new_session_with_prepared_statement_authority(
            schema_context,
            MySqlPreparedStatementAuthority::default(),
        )
    }

    /// Makes a session whose selected connections share the supplied prepared
    /// statement quota, including across database switches.
    pub fn new_session_with_prepared_statement_authority(
        self: &Arc<Self>,
        schema_context: SchemaSqlSessionContext,
        prepared_statement_authority: MySqlPreparedStatementAuthority,
    ) -> MySqlDatabaseSession {
        MySqlDatabaseSession {
            catalog: Arc::clone(self),
            schema_context,
            last_insert_id: 0,
            selected: None,
            prepared_statement_authority,
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
    last_insert_id: u64,
    selected: Option<SelectedDatabase>,
    prepared_statement_authority: MySqlPreparedStatementAuthority,
}

struct SelectedDatabase {
    name: String,
    connection: MySqlConnection,
    /// The database's collation when it was selected, which is what
    /// `@@collation_database` reads: measured on MySQL 8.4.11, an `ALTER
    /// DATABASE` another session runs reaches this session's next `CREATE
    /// TABLE` but not this reading until the database is selected again.
    collation_when_selected: MySqlTableCollation,
}

impl MySqlDatabaseSession {
    /// Parses one database-management command without reading or changing the
    /// catalog.
    ///
    /// `None` means the SQL belongs to a different command path. Callers that
    /// accept only database-management SQL can turn it into a syntax error.
    pub fn parse_admin_command(
        &self,
        sql: &str,
    ) -> Result<Option<MySqlAdminCommand>, MySqlAdminCommandError> {
        parse_optional_admin_command(sql, self.parser_mode()).map_err(MySqlAdminCommandError::from)
    }

    /// Executes one strict database-management command in this trusted
    /// embedded session.
    ///
    /// This is intentionally not a network authorization boundary. A server
    /// adapter must authenticate and authorize before calling this API.
    pub fn execute_admin_command(
        &mut self,
        sql: &str,
    ) -> Result<MySqlAdminCommandResult, MySqlAdminCommandError> {
        let command = self
            .parse_admin_command(sql)?
            .ok_or(MySqlAdminCommandError::Syntax)?;
        self.execute_parsed_admin_command(command)
            .map_err(MySqlAdminCommandError::from)
    }

    /// Executes a database-management command that was already parsed.
    ///
    /// A network adapter can authorize the typed command before calling this
    /// method, without exposing the catalog or selected connection.
    pub fn execute_parsed_admin_command(
        &mut self,
        command: MySqlAdminCommand,
    ) -> Result<MySqlAdminCommandResult, MySqlDatabaseError> {
        match command {
            MySqlAdminCommand::CreateDatabase {
                name,
                only_if_missing,
                collation,
            } => match self.catalog.create_with_collation(name.as_str(), collation) {
                Ok(database) => Ok(MySqlAdminCommandResult::Created { database }),
                Err(MySqlDatabaseError::DatabaseAlreadyExists(database)) if only_if_missing => {
                    Ok(MySqlAdminCommandResult::AlreadyExists { database })
                }
                Err(error) => Err(error),
            },
            MySqlAdminCommand::AlterDatabase { name, collation } => {
                let database = match name {
                    Some(name) => name.into_string(),
                    None => self
                        .selected_database()
                        .ok_or(MySqlDatabaseError::NoDatabaseSelected)?
                        .to_owned(),
                };
                let Some(collation) = collation else {
                    self.catalog.collation(&database)?;
                    return Ok(MySqlAdminCommandResult::Altered { database });
                };
                self.catalog.set_collation(&database, collation)?;
                // Measured on MySQL 8.4.11: the session that alters the
                // database it is in reads the new collation back straight
                // away.
                if let Some(selected) = self
                    .selected
                    .as_mut()
                    .filter(|selected| selected.name == database)
                {
                    selected.collation_when_selected = collation;
                }
                Ok(MySqlAdminCommandResult::Altered { database })
            }
            MySqlAdminCommand::DropDatabase { name } => {
                let database = name.into_string();
                // Measured on MySQL 8.4.11: `DROP DATABASE` commits the
                // session's transaction first, whichever database it names.
                if let Ok(connection) = self.connection() {
                    connection
                        .execute_transaction_command("COMMIT")
                        .map_err(|_| MySqlDatabaseError::ConnectionUnavailable)?;
                }
                let drops_its_own = self.selected_database() == Some(database.as_str());
                if drops_its_own {
                    if let Some(selected) = &self.selected {
                        selected.connection.stop_using_the_database();
                    }
                }
                self.catalog
                    .drop_database(&database, Self::DROP_DATABASE_WAIT)?;
                // Measured on MySQL 8.4.11: the session that drops the
                // database it is in is left in none, `DATABASE()` answering
                // NULL.
                if drops_its_own {
                    self.selected = None;
                }
                Ok(MySqlAdminCommandResult::Dropped { database })
            }
            MySqlAdminCommand::ShowCreateDatabase {
                name,
                written_name,
                only_if_missing,
            } => {
                let collation = self.catalog.collation(name.as_str())?;
                Ok(MySqlAdminCommandResult::CreateStatement {
                    statement: create_database_statement(&written_name, collation, only_if_missing),
                    database: written_name,
                })
            }
            MySqlAdminCommand::Use { name } => {
                let database = name.into_string();
                self.select_database(&database)?;
                Ok(MySqlAdminCommandResult::Selected { database })
            }
            MySqlAdminCommand::ListDatabases => Ok(MySqlAdminCommandResult::Listed {
                databases: self.catalog.list()?,
            }),
        }
    }

    /// How long a `DROP DATABASE` waits for the sessions using the database:
    /// MySQL's `lock_wait_timeout` starts at a year, and no session here can
    /// set it lower.
    const DROP_DATABASE_WAIT: std::time::Duration = std::time::Duration::from_secs(31_536_000);

    /// The selected database's name when another session has dropped it.
    ///
    /// Measured on MySQL 8.4.11: a session keeps the name of a database
    /// another session drops as its own, `DATABASE()` answering it and every
    /// statement on it answering 1049, and reads the new database once one is
    /// made under that name — which selecting the name again does here.
    pub fn dropped_database(&self) -> Option<&str> {
        self.selected
            .as_ref()
            .filter(|selected| selected.connection.database_was_dropped())
            .map(|selected| selected.name.as_str())
    }

    /// Select a ready database, preserving the prior selection if opening fails.
    pub fn select_database(&mut self, requested_name: &str) -> Result<(), MySqlDatabaseError> {
        let canonical_name = canonicalize_database_name(requested_name)?;
        let selected = {
            let mut catalog = self.catalog.lock()?;
            let (database, allocator) = catalog
                .acquire_with_allocator(&canonical_name)
                .map_err(MySqlDatabaseError::from)?;
            let io = Arc::clone(&catalog.io);
            let collation = catalog
                .shared_collation(&canonical_name)
                .map_err(MySqlDatabaseError::from)?;
            let users = catalog
                .users_of_a_ready_database(&canonical_name)
                .map_err(MySqlDatabaseError::from)?;
            let connection = database
                .connect()
                .map_err(|_| MySqlDatabaseError::ConnectionUnavailable)?;
            let connection =
                MySqlConnection::new_with_auto_increment_and_prepared_statement_authority(
                    connection,
                    self.schema_context,
                    allocator,
                    io,
                    self.prepared_statement_authority.clone(),
                )
                .map_err(|_| MySqlDatabaseError::ConnectionUnavailable)?
                .with_wal_keeper(self.catalog.wal_keeper.handle(), Arc::downgrade(&database))
                .with_database_collation(collation.clone())
                .with_database_user(DatabaseUser::new(users, canonical_name.clone()));
            connection.set_last_insert_id(self.last_insert_id);
            SelectedDatabase {
                name: canonical_name,
                connection,
                collation_when_selected: collation.get(),
            }
        };

        if let Some(previous) = self.selected.replace(selected) {
            self.last_insert_id = previous.connection.last_insert_id();
            self.selected
                .as_ref()
                .expect("selected database was just installed")
                .connection
                .set_last_insert_id(self.last_insert_id);
        }
        Ok(())
    }

    /// Return the canonical selected database name, if any.
    pub fn selected_database(&self) -> Option<&str> {
        self.selected
            .as_ref()
            .map(|selected| selected.name.as_str())
    }

    /// The collation `@@collation_database` reads: the selected database's as
    /// it was when it was selected, or `None` with no database selected.
    pub fn selected_database_collation(&self) -> Option<MySqlTableCollation> {
        self.selected
            .as_ref()
            .map(|selected| selected.collation_when_selected)
    }

    /// Return the selected connection for checked MySQL statement execution.
    ///
    /// A database another session dropped has no connection to run anything
    /// on: its files are gone, and a statement on it answers 1049.
    pub fn connection(&self) -> Result<&MySqlConnection, MySqlDatabaseError> {
        let selected = self
            .selected
            .as_ref()
            .ok_or(MySqlDatabaseError::NoDatabaseSelected)?;
        if selected.connection.database_was_dropped() {
            return Err(MySqlDatabaseError::DatabaseNotFound(selected.name.clone()));
        }
        Ok(&selected.connection)
    }

    /// Resets connection state while retaining the selected logical database.
    pub fn reset_connection(&mut self) -> Result<(), MySqlQueryError> {
        if let Ok(connection) = self.connection() {
            connection.reset_connection()?;
        }
        self.last_insert_id = 0;
        Ok(())
    }

    fn parser_mode(&self) -> SessionSqlMode {
        SessionSqlMode {
            ansi_quotes: self.schema_context.sql_mode.ansi_quotes,
            no_backslash_escapes: self.schema_context.sql_mode.no_backslash_escapes,
        }
    }
}

impl MySqlDatabaseSession {
    /// Returns the lexer modes this session runs in.
    ///
    /// A client's opening `SET sql_mode` is answered by comparing what it names
    /// against these, so a mode that would change how a quote or a backslash is
    /// read cannot be accepted silently.
    pub fn session_sql_mode(&self) -> SessionSqlMode {
        self.parser_mode()
    }
}

/// What `SHOW CREATE DATABASE` prints for a database of this collation.
///
/// Measured on MySQL 8.4.11: the name is printed as the statement wrote it,
/// and every database here is `utf8mb4` and unencrypted.
fn create_database_statement(
    written_name: &str,
    collation: MySqlTableCollation,
    only_if_missing: bool,
) -> String {
    format!(
        "CREATE DATABASE {}`{}` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE {} */ /*!80016 DEFAULT ENCRYPTION='N' */",
        if only_if_missing {
            "/*!32312 IF NOT EXISTS*/ "
        } else {
            ""
        },
        written_name.replace('`', "``"),
        collation.name()
    )
}

/// Owns the trusted root capability and opens registered MySQL databases
/// without exposing a filesystem path to Core or to logical-database callers.
pub(crate) struct DatabaseCatalog {
    registry: OsDatabaseRegistry,
    io: Arc<dyn IO>,
    /// The collation of each database a session has selected, shared with
    /// that session's connection.
    shared_collations: BTreeMap<DatabaseName, SharedDatabaseCollation>,
    /// The connections using each database a session has selected or a
    /// `DROP DATABASE` has named.
    users: BTreeMap<DatabaseName, Arc<DatabaseUsers>>,
}

impl DatabaseCatalog {
    /// Opens the configured data root once and retains only its capability.
    pub(crate) fn open(root_path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let root = OsDataRoot::open(root_path.as_ref())?;
        let registry = DatabaseRegistry::open_or_create(root)?;
        let io = Arc::new(PlatformIO::new().map_err(|_| RegistryError::Backend)?);
        Ok(Self {
            registry,
            io,
            shared_collations: BTreeMap::new(),
            users: BTreeMap::new(),
        })
    }

    /// Creates, initializes, publishes, and opens one logical database.
    pub(crate) fn create(
        &mut self,
        requested_name: &str,
    ) -> Result<(DatabaseName, Arc<Database>), RegistryError> {
        self.create_with_collation(requested_name, MySqlTableCollation::default())
    }

    /// Creates, initializes, publishes, and opens one logical database whose
    /// tables take `collation` when they name none.
    pub(crate) fn create_with_collation(
        &mut self,
        requested_name: &str,
        collation: MySqlTableCollation,
    ) -> Result<(DatabaseName, Arc<Database>), RegistryError> {
        let io = Arc::clone(&self.io);
        // The registry canonicalizes the name it was given, and the tables the
        // database answers `information_schema` with carry that name.
        let logical_name = DatabaseName::parse(requested_name)
            .map_err(|_| RegistryError::Backend)?
            .as_str()
            .to_owned();
        self.registry.create_with_initializer(
            requested_name,
            collation,
            move |stage, expected, lifetime| {
                let identity = PreopenedDatabaseIdentity::new(expected.file_key().as_str())
                    .map_err(|_| RegistryError::Backend)?;
                let durable_identity = expected.file_key().to_database_identity()?;
                let main_file = stage.main_file()?;
                let wal_file = stage.wal_file()?;
                let allocator = initialize_stage_allocator(
                    io.as_ref(),
                    stage.allocator_file()?,
                    expected.file_key().as_str(),
                    durable_identity,
                )?;
                open_preopened_database_with_wal(
                    io,
                    main_file,
                    wal_file,
                    identity,
                    durable_identity,
                    &logical_name,
                    AllocatorDatabaseLifetime {
                        _lifetime: lifetime,
                        _allocator: allocator,
                    },
                )
                .map_err(|_| RegistryError::Backend)
            },
        )
    }

    /// Acquires and opens one ready logical database by its canonical name.
    pub(crate) fn acquire(&mut self, requested_name: &str) -> Result<Arc<Database>, RegistryError> {
        self.acquire_with_allocator(requested_name)
            .map(|(database, _)| database)
    }

    /// Acquires one ready database and the verified allocator retained for its
    /// next allocator-backed execution slice.
    fn acquire_with_allocator(
        &mut self,
        requested_name: &str,
    ) -> Result<(Arc<Database>, DurableRangeAllocator), RegistryError> {
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
        let allocator = reopen_allocator(
            self.io.as_ref(),
            handle.allocator_file()?,
            expected_key.as_str(),
            durable_identity,
        )?;
        let database = open_preopened_database_with_wal(
            Arc::clone(&self.io),
            main_file,
            wal_file,
            identity,
            durable_identity,
            name.as_str(),
            AllocatorDatabaseLifetime {
                _lifetime: lifetime,
                _allocator: allocator.clone(),
            },
        )
        .map_err(|_| RegistryError::Backend)?;
        Ok((database, allocator))
    }

    /// The collation a ready database gives the tables made in it.
    pub(crate) fn collation(
        &self,
        requested_name: &str,
    ) -> Result<MySqlTableCollation, RegistryError> {
        self.registry.collation(requested_name)
    }

    /// Durably changes a ready database's collation, then hands the new one to
    /// every connection to it.
    pub(crate) fn set_collation(
        &mut self,
        requested_name: &str,
        collation: MySqlTableCollation,
    ) -> Result<(), RegistryError> {
        let name = self.registry.set_collation(requested_name, collation)?;
        if let Some(shared) = self.shared_collations.get(&name) {
            shared.set(collation);
        }
        Ok(())
    }

    /// The collation every connection to a ready database shares.
    fn shared_collation(
        &mut self,
        requested_name: &str,
    ) -> Result<SharedDatabaseCollation, RegistryError> {
        let name = DatabaseName::parse(requested_name)?;
        if let Some(shared) = self.shared_collations.get(&name) {
            return Ok(shared.clone());
        }
        let shared = SharedDatabaseCollation::new(self.registry.collation(name.as_str())?);
        self.shared_collations.insert(name, shared.clone());
        Ok(shared)
    }

    /// The connections using a ready database, which every connection to it
    /// shares with a `DROP DATABASE` of it.
    fn users_of_a_ready_database(
        &mut self,
        requested_name: &str,
    ) -> Result<Arc<DatabaseUsers>, RegistryError> {
        let name = DatabaseName::parse(requested_name)?;
        if !self.registry.contains(name.as_str())? {
            return Err(RegistryError::DatabaseNotFound(name));
        }
        Ok(Arc::clone(self.users.entry(name).or_default()))
    }

    /// Drops a ready logical database, whether or not a session still holds
    /// it open.
    ///
    /// The caller has already waited for every connection using it; one that
    /// merely holds it stops being able to use it once the drop is recorded.
    pub(crate) fn drop_database(&mut self, requested_name: &str) -> Result<(), RegistryError> {
        self.registry.drop_database(requested_name)?;
        let name = DatabaseName::parse(requested_name)?;
        self.shared_collations.remove(&name);
        self.users.remove(&name);
        Ok(())
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

fn initialize_stage_allocator(
    io: &dyn IO,
    file: std::fs::File,
    debug_identity: &str,
    durable_identity: [u8; 16],
) -> Result<DurableRangeAllocator, RegistryError> {
    let identity =
        AllocatorDatabaseIdentity::new(durable_identity).map_err(|_| RegistryError::Backend)?;
    let reopen_file = file.try_clone().map_err(|_| RegistryError::Backend)?;
    let allocator = DurableRangeAllocator::from_std_file(
        file,
        debug_identity.to_owned(),
        identity,
        AllocatorOpenMode::Create,
        FileSyncType::Fsync,
    )
    .map_err(|_| RegistryError::Backend)?;
    let mut operation = allocator.initialize().map_err(|_| RegistryError::Backend)?;
    io.block(|| operation.step())
        .map_err(|_| RegistryError::Backend)?;
    drop(operation);
    drop(allocator);
    reopen_allocator(io, reopen_file, debug_identity, durable_identity)
}

fn reopen_allocator(
    io: &dyn IO,
    file: std::fs::File,
    debug_identity: &str,
    durable_identity: [u8; 16],
) -> Result<DurableRangeAllocator, RegistryError> {
    let identity =
        AllocatorDatabaseIdentity::new(durable_identity).map_err(|_| RegistryError::Backend)?;
    let allocator = DurableRangeAllocator::from_std_file(
        file,
        debug_identity.to_owned(),
        identity,
        AllocatorOpenMode::Reopen,
        FileSyncType::Fsync,
    )
    .map_err(|_| RegistryError::Backend)?;
    let mut operation = allocator.verify().map_err(|_| RegistryError::Backend)?;
    io.block(|| operation.step())
        .map_err(|_| RegistryError::Backend)?;
    Ok(allocator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_sql::{CharacterSet, Collation, SchemaSqlMode};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
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

    /// The files go while a connection still holds them open, and neither
    /// the connection nor the database made again under the name sees the
    /// other: Unix keeps a removed file for whoever has it open.
    #[test]
    fn a_connection_holding_a_dropped_database_keeps_its_files_to_itself() -> CoreResult<()> {
        let directory = private_tempdir();
        let mut catalog = DatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        let (_, database) = catalog
            .create("busy")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;
        let connection = open_connection(&database)?;
        drop(database);
        connection.execute("CREATE TABLE records (id INT)")?;
        connection.execute("INSERT INTO records (id) VALUES (1)")?;
        let files_before = fs::read_dir(directory.path()).unwrap().count();
        catalog
            .drop_database("busy")
            .map_err(|_| turso_core::LimboError::InternalError("drop database".into()))?;
        assert!(fs::read_dir(directory.path()).unwrap().count() < files_before);
        assert!(!catalog.contains("busy").unwrap());

        let (_, again) = catalog
            .create("busy")
            .map_err(|_| turso_core::LimboError::InternalError("create again".into()))?;
        let fresh = open_connection(&again)?;
        fresh.execute("CREATE TABLE records (id INT)")?;
        fresh.execute("INSERT INTO records (id) VALUES (2)")?;
        connection.execute("INSERT INTO records (id) VALUES (3)")?;
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM records ORDER BY id")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)], vec![Value::from_i64(3)]]
        );
        assert_eq!(
            fresh
                .prepare_select("SELECT id FROM records")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(2)]]
        );
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

    /// A session that read a table's columns, or the list of tables, sees
    /// another session's change to them on its next read.
    #[test]
    fn a_session_sees_the_columns_another_session_changed() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog.create("shared").unwrap();
        let mut reader = catalog.new_session(binary_context());
        let mut writer = catalog.new_session(binary_context());
        for session in [&mut reader, &mut writer] {
            session
                .select_database("shared")
                .map_err(|_| turso_core::LimboError::InternalError("select".into()))?;
        }
        let connection = |session: &MySqlDatabaseSession| session.connection().unwrap().clone();
        connection(&writer).execute("CREATE TABLE records (id INT, label TEXT)")?;
        let table = turso_mysql_parser::MySqlTableName::parse("records").unwrap();
        let names = |session: &MySqlDatabaseSession| {
            connection(session)
                .list_columns(&table)
                .unwrap()
                .iter()
                .map(|column| column.name().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&reader), ["id", "label"]);
        connection(&writer).execute("ALTER TABLE records ADD COLUMN n INT")?;
        assert_eq!(names(&reader), ["id", "label", "n"]);

        let tables = |session: &MySqlDatabaseSession| {
            connection(session)
                .list_tables()
                .unwrap()
                .iter()
                .map(|table| table.name().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(tables(&reader), ["records"]);
        connection(&writer).execute("CREATE TABLE notes (id INT)")?;
        assert_eq!(tables(&reader), ["notes", "records"]);
        Ok(())
    }

    /// A session of a catalog leaves emptying a WAL past its bound to the
    /// catalog's keeper, which empties it on a thread of its own while the
    /// session stays open and goes on writing.
    #[test]
    fn the_catalogs_keeper_empties_a_wal_while_the_session_stays_open() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog.create("kept").unwrap();
        let mut session = catalog.new_session(binary_context());
        session
            .select_database("kept")
            .map_err(|_| turso_core::LimboError::InternalError("select".into()))?;
        let connection = session.connection().unwrap().clone();
        connection.execute("CREATE TABLE records (id INT, label TEXT)")?;
        let label = "x".repeat(1000);
        for id in 0..50 {
            connection.execute(&format!(
                "INSERT INTO records (id, label) VALUES ({id}, '{label}')"
            ))?;
        }
        let wal_length = || {
            fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap())
                .find(|entry| entry.file_name().to_string_lossy().ends_with("-wal"))
                .map(|entry| entry.metadata().unwrap().len())
                .unwrap()
        };
        assert!(wal_length() > 0);

        connection.truncate_the_wal_past(0)?;
        let keeper = catalog.wal_keeper.handle();
        keeper.wait_for_the_requests_before();
        assert_eq!(
            keeper.truncated.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(wal_length(), 0);

        connection.execute("INSERT INTO records (id, label) VALUES (50, 'after')")?;
        assert_eq!(
            connection
                .prepare_select("SELECT COUNT(*) FROM records")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(51)]]
        );
        Ok(())
    }

    /// Only a closed engine connection runs the closing checkpoint, so a
    /// session that ends has to close its connection, or the WAL stays as
    /// large as the last write made it and the next open reads all of it.
    #[test]
    fn a_session_that_ends_leaves_an_empty_wal() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog.create("written").unwrap();
        let mut session = catalog.new_session(binary_context());
        session
            .select_database("written")
            .map_err(|_| turso_core::LimboError::InternalError("select".into()))?;
        let connection = session
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("connection".into()))?;
        connection.execute("CREATE TABLE records (id INT, label TEXT)")?;
        let label = "x".repeat(1000);
        for id in 0..50 {
            connection.execute(&format!(
                "INSERT INTO records (id, label) VALUES ({id}, '{label}')"
            ))?;
        }
        let wal_length = || {
            fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap())
                .find(|entry| entry.file_name().to_string_lossy().ends_with("-wal"))
                .map(|entry| entry.metadata().unwrap().len())
                .unwrap()
        };
        assert!(wal_length() > 0);
        drop(session);
        assert_eq!(wal_length(), 0);
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
        // Neither session is running a statement or holding a transaction,
        // so neither holds the drop up; each keeps the name and has nothing
        // left to run a statement on.
        catalog
            .drop_database("reports", Duration::ZERO)
            .map_err(|_| turso_core::LimboError::InternalError("drop database".into()))?;
        for session in [&writer, &reader] {
            assert_eq!(session.selected_database(), Some("reports"));
            assert!(matches!(
                session.connection(),
                Err(MySqlDatabaseError::DatabaseNotFound(name)) if name == "reports"
            ));
        }
        Ok(())
    }

    #[test]
    fn selected_database_switch_releases_old_prepared_statements() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog
            .create("first")
            .map_err(|_| turso_core::LimboError::InternalError("create first".into()))?;
        catalog
            .create("second")
            .map_err(|_| turso_core::LimboError::InternalError("create second".into()))?;
        let authority = MySqlPreparedStatementAuthority::new(1).unwrap();
        let mut session = catalog
            .new_session_with_prepared_statement_authority(binary_context(), authority.clone());

        session
            .select_database("first")
            .map_err(|_| turso_core::LimboError::InternalError("select first".into()))?;
        session
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("first connection".into()))?
            .prepare_checked_statement("SELECT 1")
            .map_err(|_| turso_core::LimboError::InternalError("prepare first".into()))?;
        assert_eq!(authority.active_count(), 1);

        session
            .select_database("second")
            .map_err(|_| turso_core::LimboError::InternalError("select second".into()))?;
        assert_eq!(authority.active_count(), 0);
        session
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("second connection".into()))?
            .prepare_checked_statement("SELECT 2")
            .map_err(|_| turso_core::LimboError::InternalError("prepare second".into()))?;
        assert_eq!(authority.active_count(), 1);
        session.reset_connection().unwrap();
        assert_eq!(authority.active_count(), 0);
        Ok(())
    }

    #[test]
    fn selected_session_uses_the_retained_auto_increment_allocator() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog
            .create("generated")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;

        let mut session = catalog.new_session(binary_context());
        session
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("select database".into()))?;
        let connection = session
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("selected connection".into()))?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.execute("INSERT INTO users (name) VALUES ('Ada'), ('Grace')")?;
        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(1), Value::from_text("Ada")],
                vec![Value::from_i64(2), Value::from_text("Grace")],
            ]
        );
        Ok(())
    }

    #[test]
    fn auto_increment_sequence_survives_catalog_reopen() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog
            .create("generated")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;

        let mut before_restart = catalog.new_session(binary_context());
        before_restart
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("select database".into()))?;
        let connection = before_restart
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("selected connection".into()))?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.execute("INSERT INTO users (name) VALUES ('before_restart')")?;
        connection.close()?;
        drop(before_restart);
        drop(catalog);

        let reopened = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("reopen catalog".into()))?;
        let mut after_restart = reopened.new_session(binary_context());
        after_restart
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("reselect database".into()))?;
        let connection = after_restart
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("reselected connection".into()))?;
        connection.execute("INSERT INTO users (name) VALUES ('after_restart')")?;
        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(1), Value::from_text("before_restart")],
                vec![Value::from_i64(2), Value::from_text("after_restart")],
            ]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_sessions_reserve_disjoint_contiguous_ranges() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog
            .create("generated")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;

        let mut first = catalog.new_session(binary_context());
        let mut second = catalog.new_session(binary_context());
        first
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("select first".into()))?;
        second
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("select second".into()))?;

        let first_connection = first
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("first connection".into()))?;
        first_connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        first_connection.execute("INSERT INTO users (name) VALUES ('first_1'), ('first_2')")?;

        let second_connection = second
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("second connection".into()))?;
        second_connection
            .execute("INSERT INTO users (name) VALUES ('second_1'), ('second_2'), ('second_3')")?;

        assert_eq!(
            first_connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(1), Value::from_text("first_1")],
                vec![Value::from_i64(2), Value::from_text("first_2")],
                vec![Value::from_i64(3), Value::from_text("second_1")],
                vec![Value::from_i64(4), Value::from_text("second_2")],
                vec![Value::from_i64(5), Value::from_text("second_3")],
            ]
        );
        first_connection.close()?;
        second_connection.close()?;
        Ok(())
    }

    #[test]
    fn selected_sessions_keep_last_insert_id_for_their_own_allocator_range() -> CoreResult<()> {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
        catalog
            .create("generated")
            .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;

        let mut first = catalog.new_session(binary_context());
        let mut second = catalog.new_session(binary_context());
        first
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("select first".into()))?;
        second
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("select second".into()))?;

        let first_connection = first
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("first connection".into()))?;
        first_connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        first_connection.execute("INSERT INTO users (name) VALUES ('first_1'), ('first_2')")?;

        let second_connection = second
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("second connection".into()))?;
        second_connection
            .execute("INSERT INTO users (name) VALUES ('second_1'), ('second_2'), ('second_3')")?;

        assert_eq!(
            first_connection
                .prepare_select("SELECT LAST_INSERT_ID()")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]]
        );
        assert_eq!(
            second_connection
                .prepare_select("SELECT LAST_INSERT_ID()")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(3)]]
        );
        assert_eq!(first_connection.last_insert_id(), 1);
        assert_eq!(second_connection.last_insert_id(), 3);

        first_connection.close()?;
        second_connection.close()?;
        Ok(())
    }

    #[test]
    fn reopened_session_starts_last_insert_id_at_zero_and_continues_sequence() -> CoreResult<()> {
        let directory = private_tempdir();
        {
            let catalog = MySqlDatabaseCatalog::open(directory.path())
                .map_err(|_| turso_core::LimboError::InternalError("open catalog".into()))?;
            catalog
                .create("generated")
                .map_err(|_| turso_core::LimboError::InternalError("create database".into()))?;

            let mut session = catalog.new_session(binary_context());
            session
                .select_database("generated")
                .map_err(|_| turso_core::LimboError::InternalError("select database".into()))?;
            let connection = session
                .connection()
                .map_err(|_| turso_core::LimboError::InternalError("selected connection".into()))?;
            connection.execute(
                "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
            )?;
            connection.execute("INSERT INTO users (name) VALUES ('before_1'), ('before_2')")?;
            assert_eq!(connection.last_insert_id(), 1);
            connection.close()?;
        }

        let catalog = MySqlDatabaseCatalog::open(directory.path())
            .map_err(|_| turso_core::LimboError::InternalError("reopen catalog".into()))?;
        let mut session = catalog.new_session(binary_context());
        session
            .select_database("generated")
            .map_err(|_| turso_core::LimboError::InternalError("reselect database".into()))?;
        let connection = session
            .connection()
            .map_err(|_| turso_core::LimboError::InternalError("reselected connection".into()))?;
        assert_eq!(connection.last_insert_id(), 0);
        assert_eq!(
            connection
                .prepare_select("SELECT LAST_INSERT_ID()")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(0)]]
        );

        connection.execute("INSERT INTO users (name) VALUES ('after_reopen')")?;
        assert_eq!(connection.last_insert_id(), 3);
        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(1), Value::from_text("before_1")],
                vec![Value::from_i64(2), Value::from_text("before_2")],
                vec![Value::from_i64(3), Value::from_text("after_reopen")],
            ]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn switching_databases_preserves_session_last_insert_id() -> CoreResult<()> {
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
        session.connection().unwrap().execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        session
            .connection()
            .unwrap()
            .execute("INSERT INTO users (name) VALUES ('kept')")?;
        assert_eq!(session.connection().unwrap().last_insert_id(), 1);

        session
            .select_database("second")
            .map_err(|_| turso_core::LimboError::InternalError("select second".into()))?;
        assert_eq!(session.connection().unwrap().last_insert_id(), 1);
        assert_eq!(
            session
                .connection()
                .unwrap()
                .prepare_select("SELECT LAST_INSERT_ID()")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]]
        );
        Ok(())
    }

    #[test]
    fn resetting_session_clears_last_insert_id_and_keeps_database_selected() -> CoreResult<()> {
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
        session.connection().unwrap().execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        session
            .connection()
            .unwrap()
            .execute("INSERT INTO users (name) VALUES ('before_reset')")?;
        assert_eq!(session.connection().unwrap().last_insert_id(), 1);

        session.reset_connection()?;

        assert_eq!(session.selected_database(), Some("first"));
        assert_eq!(session.connection().unwrap().last_insert_id(), 0);
        assert_eq!(
            session
                .connection()
                .unwrap()
                .prepare_select("SELECT LAST_INSERT_ID()")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(0)]]
        );
        session
            .select_database("second")
            .map_err(|_| turso_core::LimboError::InternalError("select second".into()))?;
        assert_eq!(session.connection().unwrap().last_insert_id(), 0);
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
            .drop_database("first", Duration::ZERO)
            .map_err(|_| turso_core::LimboError::InternalError("drop first".into()))?;
        assert!(session.connection().is_ok());
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
    fn canonicalizing_a_name_is_independent_of_catalog_state() {
        assert_eq!(
            canonicalize_database_name("RePoRtS"),
            Ok("reports".to_owned())
        );
        assert_eq!(
            canonicalize_database_name("mysql"),
            Err(MySqlDatabaseError::InvalidDatabaseName)
        );
        assert_eq!(
            canonicalize_database_name("bad/name"),
            Err(MySqlDatabaseError::InvalidDatabaseName)
        );
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
    fn parsed_admin_commands_execute_without_reparsing_and_list_keeps_selection() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let mut session = catalog.new_session(binary_context());

        let create_reports = session
            .parse_admin_command("CREATE DATABASE Reports")
            .unwrap()
            .expect("CREATE DATABASE must be an admin command");
        assert_eq!(
            session.execute_parsed_admin_command(create_reports),
            Ok(MySqlAdminCommandResult::Created {
                database: "reports".to_owned(),
            })
        );
        session
            .execute_admin_command("CREATE DATABASE archive")
            .unwrap();
        assert_eq!(
            session.execute_admin_command("CREATE DATABASE IF NOT EXISTS Archive"),
            Ok(MySqlAdminCommandResult::AlreadyExists {
                database: "archive".to_owned(),
            })
        );
        assert_eq!(
            session.execute_admin_command("CREATE DATABASE archive"),
            Err(MySqlAdminCommandError::Database(
                MySqlDatabaseError::DatabaseAlreadyExists("archive".to_owned())
            ))
        );
        session.execute_admin_command("USE reports").unwrap();

        let list = session
            .parse_admin_command("SHOW DATABASES;")
            .unwrap()
            .expect("SHOW DATABASES must be an admin command");
        assert_eq!(list, MySqlAdminCommand::ListDatabases);
        assert_eq!(
            session.execute_parsed_admin_command(list),
            Ok(MySqlAdminCommandResult::Listed {
                databases: vec!["archive".to_owned(), "reports".to_owned()],
            })
        );
        assert_eq!(session.selected_database(), Some("reports"));
        assert_eq!(session.parse_admin_command("SELECT 1"), Ok(None));
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
            "DROP DATABASE IF EXISTS one",
            "USE one /* comment */",
        ] {
            let error = session.execute_admin_command(sql).unwrap_err();
            assert_eq!(error, MySqlAdminCommandError::Syntax);
            assert_eq!(error.to_string(), "syntax error");
            assert!(!error.to_string().contains(&root_path));
        }
        assert_eq!(
            session.execute_admin_command("CREATE DATABASE one CHARACTER SET latin1"),
            Err(MySqlAdminCommandError::Unsupported)
        );
        assert!(catalog.list().unwrap().is_empty());
        assert_eq!(session.selected_database(), None);
    }

    /// The collation lives in the registry, which is written whole to a new
    /// file and renamed over the old one, so it is there after a restart and
    /// never half written.
    #[test]
    fn a_database_collation_survives_reopening_the_catalog() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let mut session = catalog.new_session(binary_context());
        for sql in [
            "CREATE DATABASE made COLLATE utf8mb4_unicode_ci",
            "CREATE DATABASE altered",
            "ALTER DATABASE altered COLLATE utf8mb4_unicode_ci",
            "CREATE DATABASE plain",
        ] {
            session.execute_admin_command(sql).unwrap();
        }
        session.execute_admin_command("USE altered").unwrap();
        assert_eq!(
            session.selected_database_collation(),
            Some(MySqlTableCollation::Utf8mb4UnicodeCi)
        );
        assert_eq!(
            session.connection().unwrap().database_collation(),
            MySqlTableCollation::Utf8mb4UnicodeCi
        );
        drop(session);
        drop(catalog);

        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        for (database, collation) in [
            ("made", MySqlTableCollation::Utf8mb4UnicodeCi),
            ("altered", MySqlTableCollation::Utf8mb4UnicodeCi),
            ("plain", MySqlTableCollation::Utf8mb40900AiCi),
        ] {
            assert_eq!(catalog.collation(database), Ok(collation), "{database}");
        }
        let mut session = catalog.new_session(binary_context());
        assert_eq!(session.selected_database_collation(), None);
        session.execute_admin_command("USE made").unwrap();
        assert_eq!(
            session.connection().unwrap().database_collation(),
            MySqlTableCollation::Utf8mb4UnicodeCi
        );
        assert_eq!(
            session.execute_admin_command("SHOW CREATE DATABASE Made"),
            Ok(MySqlAdminCommandResult::CreateStatement {
                database: "Made".to_owned(),
                statement: "CREATE DATABASE `Made` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci */ /*!80016 DEFAULT ENCRYPTION='N' */".to_owned(),
            })
        );
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

    /// Measured on MySQL 8.4.11: the session that drops the database it is
    /// in is left in none; another session in it keeps its name, answers
    /// 1049 for anything on it, and reads the new database once one is made
    /// under the name.
    #[test]
    fn two_sessions_in_a_database_one_of_them_drops() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        let mut first = catalog.new_session(binary_context());
        let mut second = catalog.new_session(binary_context());
        first
            .execute_admin_command("CREATE DATABASE Shared")
            .unwrap();
        first.execute_admin_command("USE shared").unwrap();
        second.execute_admin_command("USE SHARED").unwrap();
        second
            .connection()
            .unwrap()
            .execute("CREATE TABLE before_the_drop (id INT)")
            .unwrap();

        assert_eq!(
            first.execute_admin_command("DROP DATABASE shared"),
            Ok(MySqlAdminCommandResult::Dropped {
                database: "shared".to_owned()
            })
        );
        assert_eq!(first.selected_database(), None);
        assert_eq!(second.selected_database(), Some("shared"));
        assert!(matches!(
            second.connection(),
            Err(MySqlDatabaseError::DatabaseNotFound(name)) if name == "shared"
        ));
        assert!(catalog.list().unwrap().is_empty());
        assert!(matches!(
            second.execute_admin_command("USE shared"),
            Err(MySqlAdminCommandError::Database(MySqlDatabaseError::DatabaseNotFound(name)))
                if name == "shared"
        ));
        assert_eq!(second.selected_database(), Some("shared"));
        assert_eq!(second.dropped_database(), Some("shared"));
        assert!(second.connection().is_err());

        first
            .execute_admin_command("CREATE DATABASE shared")
            .unwrap();
        second.select_database("shared").unwrap();
        assert_eq!(second.dropped_database(), None);
        assert!(second
            .connection()
            .unwrap()
            .list_tables()
            .unwrap()
            .is_empty());
    }

    /// A transaction left open on a database holds a drop of it up until it
    /// ends, and a drop that waits longer than it was given answers busy and
    /// leaves the database as it was.
    #[test]
    fn a_drop_waits_for_an_open_transaction() {
        let directory = private_tempdir();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        catalog.create("held").unwrap();
        let mut holder = catalog.new_session(binary_context());
        holder.select_database("held").unwrap();
        let connection = holder.connection().unwrap().clone();
        connection
            .execute("CREATE TABLE rows_held (id INT)")
            .unwrap();
        connection.start_a_statement().unwrap();
        connection.execute_transaction_command("BEGIN").unwrap();
        connection
            .execute("INSERT INTO rows_held (id) VALUES (1)")
            .unwrap();
        connection.finish_a_statement();

        assert_eq!(
            catalog.drop_database("held", Duration::from_millis(20)),
            Err(MySqlDatabaseError::DatabaseBusy("held".to_owned()))
        );
        assert!(holder.connection().is_ok());

        let committer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            connection.start_a_statement().unwrap();
            connection.execute_transaction_command("COMMIT").unwrap();
            connection.finish_a_statement();
        });
        catalog
            .drop_database("held", Duration::from_secs(60))
            .unwrap();
        committer.join().unwrap();
        assert!(holder.connection().is_err());
        assert!(matches!(
            catalog.drop_database("held", Duration::ZERO),
            Err(MySqlDatabaseError::DatabaseNotFound(name)) if name == "held"
        ));
    }
}
