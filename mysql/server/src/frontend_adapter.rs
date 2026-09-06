//! Adapter from the bounded MySQL frontend to the transport-neutral server.
//!
//! The dependency points from the protocol crate to the frontend crate.  The
//! frontend does not depend on this crate, so this keeps the execution boundary
//! one-way while allowing a server owner to opt into the checked SELECT slice.

use std::collections::HashMap;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use turso_core::Statement;
use turso_core::{LimboError, Numeric, Value};
#[cfg(unix)]
use turso_mysql::MySqlTableKind;
#[cfg(unix)]
use turso_mysql::{
    canonicalize_database_name, MySqlDatabaseCatalog, MySqlDatabaseError, MySqlDatabaseSession,
    MySqlPreparedStatementAuthority,
};
#[cfg(unix)]
use turso_mysql::{
    MySqlAdminCommand, MySqlAdminCommandError, MySqlAdminCommandResult, MySqlColumnDefault,
    MySqlColumnKey, MySqlColumnMetadata, MySqlColumnMetadataError, MySqlIndexEntry,
    MySqlShowCreateTableError, MySqlShowCreateTableResult,
};
use turso_mysql::{
    MySqlAffectedRowsMode, MySqlConnection, MySqlDropTableError, MySqlMarkerType,
    MySqlPreparedExecutionResult, MySqlPreparedResultColumn, MySqlPreparedResultColumnTypeMetadata,
    MySqlQueryError,
};
use turso_mysql::{
    MySqlPreparedStatementError, MySqlPreparedStatementMetadata, MySqlPreparedValue,
};
use turso_mysql_parser::{
    parse_driver_bootstrap_query, parse_optional_drop_table, parse_optional_drop_view,
    parse_select, MySqlDriverBootstrapQuery, SessionSqlMode,
};
#[cfg(unix)]
use turso_mysql_parser::{
    parse_optional_describe, parse_optional_information_schema_columns,
    parse_optional_information_schema_schemata, parse_optional_information_schema_tables,
    parse_optional_show_columns, parse_optional_show_create_table, parse_optional_show_full_tables,
    parse_optional_create_table_with_keys, parse_optional_show_index, parse_optional_show_tables,
    MySqlDatabaseName, MySqlShowCommand,
    MySqlTableName,
};

#[cfg(unix)]
use crate::{
    authorization_frontend_error, AuthenticatedCommandExecutor, AuthenticatedExecutorFactory,
    AuthenticatedPrincipal, AuthorizationError, DatabaseAction, DatabaseAuthorizer, TableAction,
};
use crate::{
    decode_statement_execute_parameters_with_long_data, BinaryResultSet, BinaryResultValue,
    ColumnDefinitionConfig, CommandExecutionOptions, CommandExecutionResult, CommandExecutor,
    CommandOkResult, FrontendErrorKind, InitialDatabaseSelector, PreparedStatementExecutionResult,
    PreparedStatementResult, StatementExecuteDecodeError, StatementParameterType,
    StatementParameterValue, TextResultSet, DEFAULT_UTF8MB4_COLLATION, MAX_COMMAND_PAYLOAD_LENGTH,
    MAX_DISPATCH_RESULT_ROWS, MAX_RESPONSE_PACKET_PAYLOAD_LENGTH, MAX_RESULT_COLUMNS,
    MAX_TEXT_ROW_VALUE_LENGTH, SERVER_STATUS_AUTOCOMMIT, SERVER_STATUS_IN_TRANS,
};
use crate::static_result_metadata::{
    static_column_definition, static_result_column_metadata,
};

const DEFAULT_MYSQL_WAIT_TIMEOUT: Duration = Duration::from_secs(8 * 60 * 60);

/// Values returned by the exact bootstrap query used by the MySQL driver.
///
/// The runtime owns these values because both settings describe the protocol
/// owner rather than a selected database. Keeping them together prevents the
/// adapter from accidentally reporting a value that differs from the limits
/// enforced by the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlBootstrapSettings {
    max_allowed_packet: usize,
    wait_timeout: Duration,
}

impl MySqlBootstrapSettings {
    /// Creates bootstrap settings from the server's packet and idle limits.
    pub fn new(max_allowed_packet: usize, wait_timeout: Duration) -> Self {
        assert!(
            max_allowed_packet > 0,
            "max_allowed_packet must be non-zero"
        );
        assert!(!wait_timeout.is_zero(), "wait_timeout must be non-zero");
        Self {
            max_allowed_packet,
            wait_timeout: whole_second_timeout(wait_timeout),
        }
    }

    /// Returns the packet payload limit reported to the client.
    pub const fn max_allowed_packet(self) -> usize {
        self.max_allowed_packet
    }

    /// Returns the runtime idle duration represented by `wait_timeout`.
    pub const fn wait_timeout(self) -> Duration {
        self.wait_timeout
    }

    /// Returns the integer seconds reported by MySQL's `wait_timeout` value.
    pub const fn wait_timeout_seconds(self) -> u64 {
        self.wait_timeout.as_secs()
    }
}

impl Default for MySqlBootstrapSettings {
    fn default() -> Self {
        Self::new(MAX_COMMAND_PAYLOAD_LENGTH, DEFAULT_MYSQL_WAIT_TIMEOUT)
    }
}

/// Executes the frontend's checked MySQL SELECT subset for classic commands.
///
/// This adapter owns one [`MySqlConnection`].  It deliberately accepts only
/// SELECT text in `COM_QUERY`; schema writes and every other statement remain
/// outside the classic command slice until their protocol semantics are wired.
/// `COM_INIT_DB` is denied because a directly supplied connection has no
/// logical-database catalog.
pub struct MySqlCommandAdapter {
    connection: MySqlConnection,
    bootstrap_settings: MySqlBootstrapSettings,
    session_variables: crate::session_variables::MySqlSessionVariables,
    prepared_types: HashMap<u32, Vec<StatementParameterType>>,
    pending_long_data: PendingLongData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingLongDataError {
    InvalidParameter,
    TooLarge,
}

#[derive(Default)]
struct PendingLongData {
    values: HashMap<(u32, u16), Vec<u8>>,
    errors: HashMap<u32, PendingLongDataError>,
    retained_bytes: usize,
}

struct StatementLongData {
    values: Vec<Option<Vec<u8>>>,
    error: Option<PendingLongDataError>,
}

#[cfg(unix)]
struct DatabasePreparedStatement {
    database: String,
    source_table: Option<String>,
    connection: MySqlConnection,
    connection_statement_id: u32,
    parameter_types: Option<Vec<StatementParameterType>>,
}

#[cfg(unix)]
struct DatabasePreparedStatementRegistry {
    next_statement_id: Option<u32>,
    statements: HashMap<u32, DatabasePreparedStatement>,
}

#[cfg(unix)]
impl Default for DatabasePreparedStatementRegistry {
    fn default() -> Self {
        Self {
            next_statement_id: Some(1),
            statements: HashMap::new(),
        }
    }
}

impl MySqlCommandAdapter {
    /// Creates an adapter around a checked MySQL frontend connection.
    pub fn new(connection: MySqlConnection) -> Self {
        Self {
            connection,
            bootstrap_settings: MySqlBootstrapSettings::default(),
            session_variables: crate::session_variables::MySqlSessionVariables::default(),
            prepared_types: HashMap::new(),
            pending_long_data: PendingLongData::default(),
        }
    }

    /// Supplies the protocol limits returned by the driver's bootstrap query.
    pub fn with_bootstrap_settings(
        mut self,
        max_allowed_packet: usize,
        wait_timeout: Duration,
    ) -> Self {
        self.bootstrap_settings = MySqlBootstrapSettings::new(max_allowed_packet, wait_timeout);
        self
    }
}

impl CommandExecutor for MySqlCommandAdapter {
    fn status_flags(&self) -> u16 {
        connection_status_flags(&self.connection)
    }

    fn execute_init_db(
        &mut self,
        _database: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        Err(FrontendErrorKind::Unsupported)
    }

    fn execute_query(&mut self, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind> {
        let status_flags = self.status_flags();
        if let Some(result) = self.session_variables.execute_query(
            sql,
            self.bootstrap_settings,
            None,
            status_flags,
        )? {
            return Ok(result);
        }
        if let Some(result) = execute_bootstrap_query(
            sql,
            self.bootstrap_settings,
            connection_status_flags(&self.connection),
        )? {
            return Ok(result);
        }
        if is_internal_catalog_select(sql) {
            return Err(FrontendErrorKind::Unsupported);
        }
        execute_checked_query(
            &self.connection,
            sql,
            None,
            None,
            None,
            MySqlAffectedRowsMode::Changed,
            self.session_variables.sql_notes(),
        )
    }

    fn execute_reset_connection(&mut self) -> Result<(), FrontendErrorKind> {
        self.connection
            .reset_connection()
            .map_err(frontend_query_error)?;
        self.prepared_types.clear();
        self.session_variables = crate::session_variables::MySqlSessionVariables::default();
        self.pending_long_data = PendingLongData::default();
        Ok(())
    }

    fn execute_stmt_prepare(
        &mut self,
        sql: &str,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        if is_internal_catalog_select(sql) {
            return Err(FrontendErrorKind::Unsupported);
        }
        prepare_checked_statement(&self.connection, sql)
    }

    fn execute_stmt_close(&mut self, statement_id: u32) {
        self.connection.remove_prepared_statement(statement_id);
        self.prepared_types.remove(&statement_id);
        self.pending_long_data.clear_statement(statement_id);
    }

    fn execute_stmt_reset(&mut self, statement_id: u32) -> Result<(), FrontendErrorKind> {
        let result = self
            .connection
            .reset_prepared_statement(statement_id)
            .map_err(prepared_statement_error);
        if result.is_ok() {
            self.pending_long_data.clear_statement(statement_id);
        }
        result
    }

    fn execute_stmt_send_long_data(&mut self, statement_id: u32, parameter_id: u16, data: &[u8]) {
        let Some(parameter_count) = self
            .connection
            .prepared_statement_metadata(statement_id)
            .map(|metadata| metadata.parameter_count)
        else {
            return;
        };
        self.pending_long_data
            .append(statement_id, parameter_id, data, parameter_count);
    }

    fn execute_stmt_execute(
        &mut self,
        statement_id: u32,
        parameter_payload: &[u8],
    ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
        let long_data = self.pending_long_data.take_statement(statement_id);
        execute_prepared_statement(
            &self.connection,
            &mut self.prepared_types,
            statement_id,
            parameter_payload,
            long_data,
            None,
            MySqlAffectedRowsMode::Changed,
        )
    }
}

/// Creates one authorization-gated registry-backed MySQL adapter.
///
/// The factory owns the catalog and policy until authentication produces an
/// opaque principal. It cannot create a database session before then.
#[cfg(unix)]
pub struct AuthorizedDatabaseAdapterFactory<A> {
    catalog: Arc<MySqlDatabaseCatalog>,
    schema_context: turso_mysql::schema_sql::SchemaSqlSessionContext,
    authorizer: Arc<A>,
    prepared_statement_authority: MySqlPreparedStatementAuthority,
    query_timeout: Option<Duration>,
    bootstrap_settings: MySqlBootstrapSettings,
}

#[cfg(unix)]
impl<A> AuthorizedDatabaseAdapterFactory<A> {
    /// Creates a one-shot factory for an authenticated database session.
    pub fn new(
        catalog: Arc<MySqlDatabaseCatalog>,
        schema_context: turso_mysql::schema_sql::SchemaSqlSessionContext,
        authorizer: Arc<A>,
    ) -> Self {
        Self {
            catalog,
            schema_context,
            authorizer,
            prepared_statement_authority: MySqlPreparedStatementAuthority::default(),
            query_timeout: None,
            bootstrap_settings: MySqlBootstrapSettings::default(),
        }
    }

    /// Shares a prepared-statement quota with other factories for this server.
    pub fn with_prepared_statement_authority(
        mut self,
        prepared_statement_authority: MySqlPreparedStatementAuthority,
    ) -> Self {
        self.prepared_statement_authority = prepared_statement_authority;
        self
    }

    /// Applies the runtime's validated timeout to each checked SELECT.
    pub(crate) fn with_query_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "query timeout must be non-zero");
        self.query_timeout = Some(timeout);
        self
    }

    /// Supplies the protocol limits returned by the driver's bootstrap query.
    pub fn with_bootstrap_settings(
        mut self,
        max_allowed_packet: usize,
        wait_timeout: Duration,
    ) -> Self {
        self.bootstrap_settings = MySqlBootstrapSettings::new(max_allowed_packet, wait_timeout);
        self
    }
}

#[cfg(unix)]
impl<A> AuthenticatedExecutorFactory for AuthorizedDatabaseAdapterFactory<A>
where
    A: DatabaseAuthorizer,
{
    type Executor = AuthorizedDatabaseCommandAdapter<A>;

    fn build(
        self,
        principal: AuthenticatedPrincipal,
    ) -> Result<Self::Executor, AuthorizationError> {
        self.build_with_options(principal, CommandExecutionOptions::default())
    }

    fn build_with_options(
        self,
        principal: AuthenticatedPrincipal,
        command_options: CommandExecutionOptions,
    ) -> Result<Self::Executor, AuthorizationError> {
        Ok(AuthorizedDatabaseCommandAdapter {
            session: self.catalog.new_session_with_prepared_statement_authority(
                self.schema_context,
                self.prepared_statement_authority.clone(),
            ),
            principal,
            authorizer: self.authorizer,
            query_timeout: self.query_timeout,
            bootstrap_settings: self.bootstrap_settings,
            session_variables: crate::session_variables::MySqlSessionVariables::default(),
            command_options,
            prepared_statements: DatabasePreparedStatementRegistry::default(),
            pending_long_data: PendingLongData::default(),
        })
    }
}

/// Executes classic commands through one authenticated, authorized database
/// session.
///
/// This adapter is Unix-only while the trusted catalog backend depends on
/// directory-descriptor operations. It intentionally exposes neither the
/// session nor its Core connection: every catalog lookup and query must first
/// pass the policy check.
#[cfg(unix)]
pub struct AuthorizedDatabaseCommandAdapter<A> {
    session: MySqlDatabaseSession,
    principal: AuthenticatedPrincipal,
    authorizer: Arc<A>,
    query_timeout: Option<Duration>,
    bootstrap_settings: MySqlBootstrapSettings,
    session_variables: crate::session_variables::MySqlSessionVariables,
    command_options: CommandExecutionOptions,
    prepared_statements: DatabasePreparedStatementRegistry,
    pending_long_data: PendingLongData,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogVisibility {
    All,
    GrantedTables,
}

#[cfg(unix)]
impl<A> AuthorizedDatabaseCommandAdapter<A>
where
    A: DatabaseAuthorizer,
{
    /// Returns the immutable options selected by the client handshake.
    pub const fn command_options(&self) -> CommandExecutionOptions {
        self.command_options
    }

    fn select_database(&mut self, requested_name: &str) -> Result<(), FrontendErrorKind> {
        if self.session.connection().is_ok_and(|connection| {
            !connection.is_auto_commit() || !connection.session_autocommit()
        }) {
            return Err(FrontendErrorKind::Unsupported);
        }
        let canonical_name =
            canonicalize_database_name(requested_name).map_err(database_error_kind)?;
        self.authorize(DatabaseAction::Connect {
            database: Some(&canonical_name),
        })?;
        self.session
            .select_database(&canonical_name)
            .map_err(database_error_kind)
    }

    fn authorize(&self, action: DatabaseAction<'_>) -> Result<(), FrontendErrorKind> {
        self.authorizer
            .authorize(&self.principal, action)
            .map_err(authorization_frontend_error)
    }

    fn authorize_table_select(&self, database: &str, table: &str) -> Result<(), FrontendErrorKind> {
        self.authorizer
            .authorize_table(&self.principal, TableAction::Select { database, table })
            .map_err(authorization_frontend_error)
    }

    fn authorize_catalog_visibility(
        &self,
        database: &str,
    ) -> Result<CatalogVisibility, FrontendErrorKind> {
        match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::Query { database })
        {
            Ok(()) => Ok(CatalogVisibility::All),
            Err(AuthorizationError::Denied) => Ok(CatalogVisibility::GrantedTables),
            Err(error) => Err(authorization_frontend_error(error)),
        }
    }

    fn authorize_catalog_table(
        &self,
        database: &str,
        table: &str,
    ) -> Result<(), FrontendErrorKind> {
        match self.authorize_catalog_visibility(database)? {
            CatalogVisibility::All => Ok(()),
            CatalogVisibility::GrantedTables => self.authorize_table_select(database, table),
        }
    }

    fn list_information_schema_columns(
        &self,
        table: &MySqlTableName,
    ) -> Result<Vec<MySqlColumnMetadata>, FrontendErrorKind> {
        match self
            .session
            .connection()
            .map_err(database_error_kind)?
            .list_columns(table)
        {
            Ok(columns) => Ok(columns),
            Err(MySqlColumnMetadataError::TableNotFound) => Ok(Vec::new()),
            Err(error) => Err(column_metadata_error_kind(error)),
        }
    }

    fn filter_catalog_tables(
        &self,
        database: &str,
        visibility: CatalogVisibility,
        tables: Vec<turso_mysql::MySqlTable>,
    ) -> Result<Vec<turso_mysql::MySqlTable>, FrontendErrorKind> {
        if visibility == CatalogVisibility::All {
            return Ok(tables);
        }

        tables
            .into_iter()
            .try_fold(Vec::new(), |mut visible, table| {
                match self.authorizer.authorize_table(
                    &self.principal,
                    TableAction::Select {
                        database,
                        table: table.name(),
                    },
                ) {
                    Ok(()) => visible.push(table),
                    Err(AuthorizationError::Denied) => {}
                    Err(error) => return Err(authorization_frontend_error(error)),
                }
                Ok(visible)
            })
    }

    fn authorize_query_text(
        &self,
        database: &str,
        sql: &str,
    ) -> Result<Option<String>, FrontendErrorKind> {
        let source_table = parsed_source_table(sql);
        match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::Query { database })
        {
            Ok(()) => {
                if source_table
                    .as_deref()
                    .is_some_and(is_internal_catalog_table)
                {
                    return Err(FrontendErrorKind::Unsupported);
                }
                Ok(source_table)
            }
            Err(AuthorizationError::Denied) => {
                let Some(table) = source_table.as_deref() else {
                    return Err(FrontendErrorKind::AccessDenied);
                };
                if is_internal_catalog_table(table) {
                    return Err(FrontendErrorKind::AccessDenied);
                }
                self.authorize_table_select(database, table)?;
                Ok(source_table)
            }
            Err(error) => Err(authorization_frontend_error(error)),
        }
    }

    fn authorize_prepared_query(
        &self,
        database: &str,
        source_table: Option<&str>,
    ) -> Result<(), FrontendErrorKind> {
        match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::Query { database })
        {
            Ok(()) => Ok(()),
            Err(AuthorizationError::Denied) => {
                let Some(table) = source_table else {
                    return Err(FrontendErrorKind::AccessDenied);
                };
                self.authorize_table_select(database, table)
            }
            Err(error) => Err(authorization_frontend_error(error)),
        }
    }

    fn execute_admin_command(
        &mut self,
        command: MySqlAdminCommand,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        match &command {
            MySqlAdminCommand::CreateDatabase { name } => {
                let canonical_name =
                    canonicalize_database_name(name.as_str()).map_err(database_error_kind)?;
                self.authorize(DatabaseAction::Create {
                    database: &canonical_name,
                })?;
            }
            MySqlAdminCommand::DropDatabase { name } => {
                let canonical_name =
                    canonicalize_database_name(name.as_str()).map_err(database_error_kind)?;
                self.authorize(DatabaseAction::Drop {
                    database: &canonical_name,
                })?;
            }
            MySqlAdminCommand::Use { name } => {
                if self.session.connection().is_ok_and(|connection| {
                    !connection.is_auto_commit() || !connection.session_autocommit()
                }) {
                    return Err(FrontendErrorKind::Unsupported);
                }
                let canonical_name =
                    canonicalize_database_name(name.as_str()).map_err(database_error_kind)?;
                self.authorize(DatabaseAction::Connect {
                    database: Some(&canonical_name),
                })?;
            }
            MySqlAdminCommand::ListDatabases => {
                self.authorize(DatabaseAction::List)?;
            }
        }

        let result = self
            .session
            .execute_parsed_admin_command(command)
            .map_err(database_error_kind)?;
        admin_result_to_execution_result(result)
    }
}

#[cfg(unix)]
impl<A> CommandExecutor for AuthorizedDatabaseCommandAdapter<A>
where
    A: DatabaseAuthorizer,
{
    fn status_flags(&self) -> u16 {
        self.session
            .connection()
            .map(connection_status_flags)
            .unwrap_or(SERVER_STATUS_AUTOCOMMIT)
    }

    fn execute_init_db(
        &mut self,
        database: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        self.select_database(database)?;
        Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
    }

    fn execute_query(&mut self, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind> {
        let status_flags = self.status_flags();
        if let Some(result) = self.session_variables.execute_query(
            sql,
            self.bootstrap_settings,
            self.session.selected_database(),
            status_flags,
        )? {
            return Ok(result);
        }
        if let Some(result) =
            execute_bootstrap_query(sql, self.bootstrap_settings, self.status_flags())?
        {
            return Ok(result);
        }
        if let Some(command) = self
            .session
            .parse_admin_command(sql)
            .map_err(admin_error_kind)?
        {
            return self.execute_admin_command(command);
        }
        if parse_optional_information_schema_schemata(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
            .is_some()
        {
            self.authorize(DatabaseAction::List)?;
            let result = self
                .session
                .execute_parsed_admin_command(MySqlAdminCommand::ListDatabases)
                .map_err(database_error_kind)?;
            let MySqlAdminCommandResult::Listed { databases } = result else {
                unreachable!("SCHEMATA provider always lists databases");
            };
            return information_schema_schemata_result_to_execution_result(databases);
        }
        if parse_optional_information_schema_tables(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
            .is_some()
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            let visibility = self.authorize_catalog_visibility(&selected_database)?;
            let tables = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .list_tables()
                .map_err(|_| FrontendErrorKind::Internal)?;
            let tables = self.filter_catalog_tables(&selected_database, visibility, tables)?;
            return information_schema_tables_result_to_execution_result(
                &selected_database,
                tables,
                self.status_flags(),
            );
        }
        if let Some(query) =
            parse_optional_information_schema_columns(sql, SessionSqlMode::default())
                .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            let table = query.table();
            let visibility = self.authorize_catalog_visibility(&selected_database)?;
            let columns = match visibility {
                CatalogVisibility::All => self.list_information_schema_columns(table)?,
                CatalogVisibility::GrantedTables => match self.authorizer.authorize_table(
                    &self.principal,
                    TableAction::Select {
                        database: &selected_database,
                        table: table.as_str(),
                    },
                ) {
                    Ok(()) => self.list_information_schema_columns(table)?,
                    Err(AuthorizationError::Denied) => Vec::new(),
                    Err(error) => return Err(authorization_frontend_error(error)),
                },
            };
            return information_schema_columns_result_to_execution_result(
                columns,
                self.status_flags(),
            );
        }
        let full_tables = parse_optional_show_full_tables(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
            .is_some();
        if full_tables
            || matches!(
                parse_optional_show_tables(sql, SessionSqlMode::default())
                    .map_err(|_| FrontendErrorKind::Syntax)?,
                Some(MySqlShowCommand::Tables)
            )
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            let visibility = self.authorize_catalog_visibility(&selected_database)?;
            let tables = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .list_tables()
                .map_err(|_| FrontendErrorKind::Internal)?;
            let tables = self.filter_catalog_tables(&selected_database, visibility, tables)?;
            if full_tables {
                return show_full_tables_result_to_execution_result(
                    &selected_database,
                    tables,
                    self.status_flags(),
                );
            }
            return show_tables_result_to_execution_result(
                &selected_database,
                tables.into_iter().map(|table| table.name().to_owned()),
                self.status_flags(),
            );
        }

        if let Some(command) = parse_optional_show_index(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            reject_other_database_qualifier(command.database(), &selected_database)?;
            self.authorize_catalog_table(&selected_database, command.table().as_str())?;
            let entries = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .list_indexes(command.table())
                .map_err(show_create_table_error_kind)?;
            return show_index_result_to_execution_result(
                command.table().as_str(),
                entries,
                self.status_flags(),
            );
        }

        if let Some(command) = parse_optional_show_create_table(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            reject_other_database_qualifier(command.database(), &selected_database)?;
            self.authorize_catalog_table(&selected_database, command.table().as_str())?;
            let result = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .show_create_table(command.table())
                .map_err(show_create_table_error_kind)?;
            return show_create_table_result_to_execution_result(result, self.status_flags());
        }

        let column_command = match parse_optional_show_columns(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            Some(command) => Some(command),
            None => parse_optional_describe(sql, SessionSqlMode::default())
                .map_err(|_| FrontendErrorKind::Syntax)?,
        };
        if let Some(command) = column_command {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            reject_other_database_qualifier(command.database(), &selected_database)?;
            self.authorize_catalog_table(&selected_database, command.table().as_str())?;
            let columns = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .list_columns(command.table())
                .map_err(column_metadata_error_kind)?;
            return show_columns_result_to_execution_result(columns, self.status_flags());
        }

        let selected_database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?
            .to_owned();
        let source_table = self.authorize_query_text(&selected_database, sql)?;
        let connection = self.session.connection().map_err(database_error_kind)?;
        let affected_rows_mode = if self.command_options.client_found_rows() {
            MySqlAffectedRowsMode::Matched
        } else {
            MySqlAffectedRowsMode::Changed
        };
        execute_checked_query(
            connection,
            sql,
            Some(&selected_database),
            source_table.as_deref(),
            self.query_timeout,
            affected_rows_mode,
            self.session_variables.sql_notes(),
        )
    }

    fn execute_reset_connection(&mut self) -> Result<(), FrontendErrorKind> {
        self.session
            .reset_connection()
            .map_err(frontend_query_error)?;
        for statement in self.prepared_statements.statements.values() {
            statement.connection.clear_prepared_statements();
        }
        self.prepared_statements.statements.clear();
        self.session_variables = crate::session_variables::MySqlSessionVariables::default();
        self.pending_long_data = PendingLongData::default();
        Ok(())
    }

    fn execute_stmt_prepare(
        &mut self,
        sql: &str,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        let selected_database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?
            .to_owned();
        let source_table = self.authorize_query_text(&selected_database, sql)?;
        let connection = self
            .session
            .connection()
            .map_err(database_error_kind)?
            .clone();
        let metadata = connection
            .prepare_checked_statement(sql)
            .map_err(prepared_statement_error)?;
        let Some(type_metadata) =
            connection.prepared_statement_result_column_type_metadata(metadata.statement_id)
        else {
            connection.remove_prepared_statement(metadata.statement_id);
            return Err(FrontendErrorKind::Internal);
        };
        let connection_statement_id = metadata.statement_id;
        let Some(statement_id) = self.prepared_statements.next_statement_id else {
            connection.remove_prepared_statement(connection_statement_id);
            return Err(FrontendErrorKind::Internal);
        };
        let result = prepared_statement_result(
            &connection,
            MySqlPreparedStatementMetadata {
                statement_id,
                ..metadata
            },
            &type_metadata,
            Some(&selected_database),
            source_table.as_deref(),
        );
        if result.is_err() {
            connection.remove_prepared_statement(connection_statement_id);
            return result;
        }
        self.prepared_statements.next_statement_id = statement_id.checked_add(1);
        self.prepared_statements.statements.insert(
            statement_id,
            DatabasePreparedStatement {
                database: selected_database,
                source_table,
                connection,
                connection_statement_id,
                parameter_types: None,
            },
        );
        result
    }

    fn execute_stmt_close(&mut self, statement_id: u32) {
        if let Some(statement) = self.prepared_statements.statements.remove(&statement_id) {
            statement
                .connection
                .remove_prepared_statement(statement.connection_statement_id);
        }
        self.pending_long_data.clear_statement(statement_id);
    }

    fn execute_stmt_reset(&mut self, statement_id: u32) -> Result<(), FrontendErrorKind> {
        let statement = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
        let result = statement
            .connection
            .reset_prepared_statement(statement.connection_statement_id)
            .map_err(prepared_statement_error);
        if result.is_ok() {
            self.pending_long_data.clear_statement(statement_id);
        }
        result
    }

    fn execute_stmt_send_long_data(&mut self, statement_id: u32, parameter_id: u16, data: &[u8]) {
        let Some(parameter_count) = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .and_then(|statement| {
                statement
                    .connection
                    .prepared_statement_metadata(statement.connection_statement_id)
            })
            .map(|metadata| metadata.parameter_count)
        else {
            return;
        };
        self.pending_long_data
            .append(statement_id, parameter_id, data, parameter_count);
    }

    fn execute_stmt_execute(
        &mut self,
        statement_id: u32,
        parameter_payload: &[u8],
    ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
        let (database, source_table) = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .map(|statement| (statement.database.clone(), statement.source_table.clone()))
            .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
        self.authorize_prepared_query(&database, source_table.as_deref())?;
        let long_data = self.pending_long_data.take_statement(statement_id);
        let statement = self
            .prepared_statements
            .statements
            .get_mut(&statement_id)
            .expect("prepared statement was checked before authorization");
        let affected_rows_mode = if self.command_options.client_found_rows() {
            MySqlAffectedRowsMode::Matched
        } else {
            MySqlAffectedRowsMode::Changed
        };
        execute_database_prepared_statement(
            statement,
            parameter_payload,
            long_data,
            self.query_timeout,
            affected_rows_mode,
        )
    }
}

fn is_internal_catalog_table(table: &str) -> bool {
    turso_core::schema::is_system_table(table)
}

fn is_internal_catalog_select(sql: &str) -> bool {
    parse_select(sql, SessionSqlMode::default())
        .ok()
        .and_then(|translated| translated.source_table().map(str::to_owned))
        .is_some_and(|table| is_internal_catalog_table(&table))
}

#[cfg(unix)]
fn parsed_source_table(sql: &str) -> Option<String> {
    parse_select(sql, SessionSqlMode::default())
        .ok()
        .and_then(|translated| translated.source_table().map(str::to_owned))
}

#[cfg(unix)]
impl<A> InitialDatabaseSelector for AuthorizedDatabaseCommandAdapter<A>
where
    A: DatabaseAuthorizer,
{
    fn select_initial_database(&mut self, database: &str) -> Result<(), FrontendErrorKind> {
        self.select_database(database)
    }
}

#[cfg(unix)]
impl<A> AuthenticatedCommandExecutor for AuthorizedDatabaseCommandAdapter<A>
where
    A: DatabaseAuthorizer,
{
    fn authorize_connection(&mut self) -> Result<(), AuthorizationError> {
        self.authorizer
            .authorize(&self.principal, DatabaseAction::Connect { database: None })
    }
}

fn execute_checked_query(
    connection: &MySqlConnection,
    sql: &str,
    selected_database: Option<&str>,
    source_table: Option<&str>,
    query_timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
    sql_notes: bool,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let sql = strip_leading_sql_comments(sql);
    if let Some(command) = parse_optional_drop_table(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Syntax)?
    {
        let result = connection.drop_table(&command).map_err(|error| match error {
            MySqlDropTableError::MissingTable => FrontendErrorKind::UnknownTable,
            MySqlDropTableError::Engine(error) => frontend_error_kind(error),
        })?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            warnings: u16::from(!result.dropped && sql_notes),
            ..CommandOkResult::default()
        }));
    }
    if let Some(name) = parse_optional_drop_view(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Syntax)?
    {
        connection.drop_view(&name).map_err(|error| match error {
            turso_mysql::MySqlDropViewError::MissingView => FrontendErrorKind::UnknownView,
            turso_mysql::MySqlDropViewError::NotView => FrontendErrorKind::NotView,
            turso_mysql::MySqlDropViewError::Engine(error) => frontend_error_kind(error),
        })?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    match connection.is_autocommit_setting(sql) {
        Ok(true) => {
            connection
                .execute_autocommit_setting(sql)
                .map_err(frontend_query_error)?;
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: connection_status_flags(connection),
                ..CommandOkResult::default()
            }));
        }
        Ok(false) => {}
        Err(_) => return Err(FrontendErrorKind::Unsupported),
    }
    match connection.is_transaction_command(sql) {
        Ok(true) => {
            connection
                .execute_transaction_command(sql)
                .map_err(frontend_query_error)?;
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: connection_status_flags(connection),
                ..CommandOkResult::default()
            }));
        }
        Ok(false) => {}
        Err(_) => return Err(FrontendErrorKind::Unsupported),
    }
    if is_schema_statement(sql) {
        if let Some(checked) = parse_optional_create_table_with_keys(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Unsupported)?
        {
            connection
                .execute_create_table_with_keys(&checked)
                .map_err(frontend_query_error)?;
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: connection_status_flags(connection),
                ..CommandOkResult::default()
            }));
        }
        connection
            .execute_schema_ddl(sql)
            .map_err(frontend_query_error)?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if is_select_statement(sql) {
        let mut result = execute_checked_select_with_timeout(
            connection,
            sql,
            selected_database,
            source_table,
            query_timeout,
        )?;
        result.status_flags = connection_status_flags(connection);
        return Ok(CommandExecutionResult::ResultSet(result));
    }
    if !is_checked_write_statement(sql) {
        return Err(FrontendErrorKind::Unsupported);
    }
    let result = connection
        .execute_checked_write_with_affected_rows_mode(sql, query_timeout, affected_rows_mode)
        .map_err(|error| match error {
            MySqlQueryError::Engine(LimboError::Interrupt) if query_timeout.is_some() => {
                FrontendErrorKind::QueryTimeout
            }
            error => frontend_query_error(error),
        })?;
    Ok(CommandExecutionResult::Ok(CommandOkResult {
        affected_rows: result.affected_rows,
        last_insert_id: result.last_insert_id,
        status_flags: connection_status_flags(connection),
        ..CommandOkResult::default()
    }))
}

fn prepare_checked_statement(
    connection: &MySqlConnection,
    sql: &str,
) -> Result<PreparedStatementResult, FrontendErrorKind> {
    let metadata = connection
        .prepare_checked_statement(sql)
        .map_err(prepared_statement_error)?;
    let connection_statement_id = metadata.statement_id;
    let Some(type_metadata) =
        connection.prepared_statement_result_column_type_metadata(connection_statement_id)
    else {
        connection.remove_prepared_statement(connection_statement_id);
        return Err(FrontendErrorKind::Internal);
    };
    let result = prepared_statement_result(connection, metadata, &type_metadata, None, None);
    if result.is_err() {
        connection.remove_prepared_statement(connection_statement_id);
    }
    result
}

fn execute_prepared_statement(
    connection: &MySqlConnection,
    prepared_types: &mut HashMap<u32, Vec<StatementParameterType>>,
    statement_id: u32,
    parameter_payload: &[u8],
    long_data: StatementLongData,
    timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    if let Some(error) = long_data.error {
        return Err(pending_long_data_error(error));
    }
    let metadata = connection
        .prepared_statement_metadata(statement_id)
        .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
    let long_data = long_data
        .values
        .iter()
        .map(|value| value.as_deref())
        .collect::<Vec<_>>();
    let decoded = decode_statement_execute_parameters_with_long_data(
        parameter_payload,
        usize::from(metadata.parameter_count),
        prepared_types.get(&statement_id).map(Vec::as_slice),
        &long_data,
    )
    .map_err(statement_execute_decode_error)?;
    let decoded_types = decoded.types;
    let values = decoded
        .values
        .into_iter()
        .map(statement_parameter_to_frontend)
        .collect::<Vec<_>>();
    prepared_types.insert(statement_id, decoded_types);
    execute_prepared_values(
        connection,
        statement_id,
        values,
        timeout,
        affected_rows_mode,
        None,
        None,
    )
}

#[cfg(unix)]
fn execute_database_prepared_statement(
    statement: &mut DatabasePreparedStatement,
    parameter_payload: &[u8],
    long_data: StatementLongData,
    timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    if let Some(error) = long_data.error {
        return Err(pending_long_data_error(error));
    }
    let metadata = statement
        .connection
        .prepared_statement_metadata(statement.connection_statement_id)
        .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
    let long_data = long_data
        .values
        .iter()
        .map(|value| value.as_deref())
        .collect::<Vec<_>>();
    let decoded = decode_statement_execute_parameters_with_long_data(
        parameter_payload,
        usize::from(metadata.parameter_count),
        statement.parameter_types.as_deref(),
        &long_data,
    )
    .map_err(statement_execute_decode_error)?;
    let values = decoded
        .values
        .into_iter()
        .map(statement_parameter_to_frontend)
        .collect::<Vec<_>>();
    statement.parameter_types = Some(decoded.types);
    execute_prepared_values(
        &statement.connection,
        statement.connection_statement_id,
        values,
        timeout,
        affected_rows_mode,
        Some(statement.database.as_str()),
        statement.source_table.as_deref(),
    )
}

fn execute_prepared_values(
    connection: &MySqlConnection,
    statement_id: u32,
    values: Vec<MySqlPreparedValue>,
    timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
    selected_database: Option<&str>,
    source_table: Option<&str>,
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (selected_database, source_table);
    let mut retained_bytes = 0usize;
    let mut row_count = 0usize;
    let result = connection
        .execute_prepared_statement_with_row_callback(
            statement_id,
            &values,
            timeout,
            affected_rows_mode,
            |row| {
                if row_count >= MAX_DISPATCH_RESULT_ROWS {
                    return Err(LimboError::TooBig);
                }
                let row_bytes = checked_binary_result_row_bytes(row)?;
                retained_bytes = retained_bytes
                    .checked_add(row_bytes)
                    .ok_or(LimboError::TooBig)?;
                if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
                    return Err(LimboError::TooBig);
                }
                row_count += 1;
                Ok(())
            },
        )
        .map_err(|error| {
            if timeout.is_some()
                && matches!(
                    error,
                    MySqlPreparedStatementError::Engine(LimboError::Interrupt)
                )
            {
                FrontendErrorKind::QueryTimeout
            } else {
                prepared_statement_error(error)
            }
        })?;
    let rows = match result {
        MySqlPreparedExecutionResult::Rows(rows) => rows,
        MySqlPreparedExecutionResult::Write(result) => {
            return Ok(PreparedStatementExecutionResult::Ok(CommandOkResult {
                affected_rows: result.affected_rows,
                last_insert_id: result.last_insert_id,
                status_flags: connection_status_flags(connection),
                ..CommandOkResult::default()
            }));
        }
    };
    let metadata = connection
        .prepared_statement_metadata(statement_id)
        .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
    let type_metadata = connection
        .prepared_statement_result_column_type_metadata(statement_id)
        .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
    if rows
        .iter()
        .any(|row| row.len() != metadata.result_columns.len())
    {
        return Err(FrontendErrorKind::Internal);
    }
    let column_types = binary_result_column_types(&metadata, &type_metadata, &rows)?;
    #[cfg(unix)]
    let source_metadata = prepared_table_result_metadata(
        connection,
        &type_metadata,
        selected_database,
        source_table,
    )?;
    let columns = metadata
        .result_columns
        .into_iter()
        .enumerate()
        .zip(&column_types)
        .map(|((index, column), column_type)| {
            if let Some(metadata) = type_metadata[index].static_metadata() {
                return Ok(static_column_definition(column.name, metadata));
            }
            if let Some(marker) = type_metadata[index].parameter_marker() {
                if let Some(definition) =
                    marker_column_definition(column.name.clone(), marker.kind())
                {
                    return Ok(definition);
                }
            }
            #[cfg(unix)]
            if let Some(source_metadata) = source_metadata.as_ref() {
                return source_metadata.column_definition_for_reference(
                    type_metadata[index]
                        .source_reference()
                        .map(|(table, ordinal)| (table.to_owned(), ordinal)),
                    column.name,
                    Some(*column_type),
                );
            }
            Ok(column_definition(column.name, *column_type))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .zip(&column_types)
                .map(|(value, column_type)| binary_result_value(value, *column_type))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PreparedStatementExecutionResult::ResultSet(
        BinaryResultSet {
            columns,
            rows,
            warnings: 0,
            status_flags: connection_status_flags(connection),
        },
    ))
}

fn statement_parameter_to_frontend(value: StatementParameterValue) -> MySqlPreparedValue {
    match value {
        StatementParameterValue::Null => MySqlPreparedValue::Null,
        StatementParameterValue::Integer(value) => MySqlPreparedValue::Integer(value),
        StatementParameterValue::Float(value) => MySqlPreparedValue::Real(f64::from(value)),
        StatementParameterValue::Double(value) => MySqlPreparedValue::Real(value),
        StatementParameterValue::String(value) => MySqlPreparedValue::Text(value),
        StatementParameterValue::Bytes(value) => MySqlPreparedValue::Blob(value),
    }
}

#[cfg(test)]
fn prepared_result_set(result: PreparedStatementExecutionResult) -> BinaryResultSet {
    match result {
        PreparedStatementExecutionResult::ResultSet(result) => result,
        PreparedStatementExecutionResult::Ok(_) => panic!("expected prepared result set"),
    }
}

fn binary_result_column_types(
    metadata: &MySqlPreparedStatementMetadata,
    type_metadata: &[MySqlPreparedResultColumnTypeMetadata],
    rows: &[Vec<MySqlPreparedValue>],
) -> Result<Vec<u8>, FrontendErrorKind> {
    if metadata.result_columns.len() != type_metadata.len() {
        return Err(FrontendErrorKind::Internal);
    }
    for row in rows {
        if row.len() != metadata.result_columns.len() {
            return Err(FrontendErrorKind::Internal);
        }
    }

    metadata
        .result_columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let known_type = mysql_type_for_prepared_column(column, &type_metadata[index]);
            Ok(known_type.unwrap_or_else(|| {
                rows.iter()
                    .filter_map(|row| binary_result_value_type(&row[index]))
                    .next()
                    .unwrap_or(MYSQL_TYPE_NULL)
            }))
        })
        .collect()
}

fn binary_result_value_type(value: &MySqlPreparedValue) -> Option<u8> {
    match value {
        MySqlPreparedValue::Null => None,
        MySqlPreparedValue::Integer(_) => Some(MYSQL_TYPE_LONGLONG),
        MySqlPreparedValue::Real(_) => Some(MYSQL_TYPE_DOUBLE),
        MySqlPreparedValue::Text(_) => Some(MYSQL_TYPE_VAR_STRING),
        MySqlPreparedValue::Blob(_) => Some(MYSQL_TYPE_BLOB),
    }
}

fn binary_result_value(
    value: MySqlPreparedValue,
    column_type: u8,
) -> Result<BinaryResultValue, FrontendErrorKind> {
    match value {
        MySqlPreparedValue::Null => Ok(BinaryResultValue::Null),
        MySqlPreparedValue::Integer(value)
            if matches!(
                column_type,
                MYSQL_TYPE_TINY
                    | MYSQL_TYPE_SHORT
                    | MYSQL_TYPE_INT24
                    | MYSQL_TYPE_LONG
                    | MYSQL_TYPE_LONGLONG
            ) =>
        {
            Ok(BinaryResultValue::Integer(value))
        }
        MySqlPreparedValue::Real(value) if column_type == MYSQL_TYPE_DOUBLE => {
            Ok(BinaryResultValue::Real(value))
        }
        MySqlPreparedValue::Text(value) if column_type == MYSQL_TYPE_VAR_STRING => {
            Ok(BinaryResultValue::Text(value))
        }
        MySqlPreparedValue::Blob(value) if column_type == MYSQL_TYPE_BLOB => {
            Ok(BinaryResultValue::Blob(value))
        }
        _ => Err(FrontendErrorKind::Internal),
    }
}

fn checked_binary_result_row_bytes(row: &[MySqlPreparedValue]) -> Result<usize, LimboError> {
    let overhead = std::mem::size_of::<Vec<MySqlPreparedValue>>()
        .checked_add(
            std::mem::size_of::<MySqlPreparedValue>()
                .checked_mul(row.len())
                .ok_or(LimboError::TooBig)?,
        )
        .ok_or(LimboError::TooBig)?;
    row.iter().try_fold(overhead, |total, value| {
        let bytes = match value {
            MySqlPreparedValue::Null => 0,
            MySqlPreparedValue::Integer(_) | MySqlPreparedValue::Real(_) => 8,
            MySqlPreparedValue::Text(value) => value.len(),
            MySqlPreparedValue::Blob(value) => value.len(),
        };
        if bytes > MAX_TEXT_ROW_VALUE_LENGTH {
            return Err(LimboError::TooBig);
        }
        total.checked_add(bytes).ok_or(LimboError::TooBig)
    })
}

fn statement_execute_decode_error(_error: StatementExecuteDecodeError) -> FrontendErrorKind {
    FrontendErrorKind::Syntax
}

fn prepared_statement_result(
    connection: &MySqlConnection,
    metadata: MySqlPreparedStatementMetadata,
    type_metadata: &[MySqlPreparedResultColumnTypeMetadata],
    selected_database: Option<&str>,
    source_table: Option<&str>,
) -> Result<PreparedStatementResult, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (selected_database, source_table);
    if metadata.result_columns.len() != type_metadata.len() {
        return Err(FrontendErrorKind::Internal);
    }
    let parameters = (0..metadata.parameter_count)
        .map(|index| column_definition(format!("?{}", index + 1), MYSQL_TYPE_NULL))
        .collect();
    #[cfg(unix)]
    let source_metadata =
        prepared_table_result_metadata(connection, type_metadata, selected_database, source_table)?;
    let columns = metadata
        .result_columns
        .into_iter()
        .zip(type_metadata)
        .map(|(column, type_metadata)| {
            if let Some(metadata) = type_metadata.static_metadata() {
                return Ok(static_column_definition(column.name, metadata));
            }
            if let Some(marker) = type_metadata.parameter_marker() {
                if let Some(definition) =
                    marker_column_definition(column.name.clone(), marker.kind())
                {
                    return Ok(definition);
                }
            }
            let column_type =
                mysql_type_for_prepared_column(&column, type_metadata).unwrap_or(MYSQL_TYPE_NULL);
            #[cfg(unix)]
            if let Some(source_metadata) = source_metadata.as_ref() {
                return source_metadata.column_definition_for_reference(
                    type_metadata
                        .source_reference()
                        .map(|(table, ordinal)| (table.to_owned(), ordinal)),
                    column.name,
                    Some(column_type),
                );
            }
            Ok(column_definition(column.name, column_type))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PreparedStatementResult {
        statement_id: metadata.statement_id,
        parameters,
        columns,
        warnings: 0,
        status_flags: connection_status_flags(connection),
    })
}

fn prepared_statement_error(error: MySqlPreparedStatementError) -> FrontendErrorKind {
    match error {
        MySqlPreparedStatementError::MissingRequiredDefault(_) => {
            FrontendErrorKind::MissingRequiredDefault
        }
        MySqlPreparedStatementError::Prepare(error) => frontend_query_error(error),
        MySqlPreparedStatementError::PreparedStatementLimitReached { .. } => {
            FrontendErrorKind::PreparedStatementLimitReached
        }
        MySqlPreparedStatementError::StatementIdExhausted => FrontendErrorKind::Internal,
        MySqlPreparedStatementError::UnknownStatement { .. } => {
            FrontendErrorKind::UnknownPreparedStatement
        }
        MySqlPreparedStatementError::ParameterCountMismatch { .. } => FrontendErrorKind::Syntax,
        MySqlPreparedStatementError::Engine(error) => frontend_error_kind(error),
    }
}

fn frontend_query_error(error: MySqlQueryError) -> FrontendErrorKind {
    match error {
        MySqlQueryError::MissingRequiredDefault(_) => FrontendErrorKind::MissingRequiredDefault,
        MySqlQueryError::Syntax(_) => FrontendErrorKind::Syntax,
        MySqlQueryError::Unsupported(_) => FrontendErrorKind::Unsupported,
        MySqlQueryError::Engine(error) => frontend_error_kind(error),
    }
}

fn connection_status_flags(connection: &MySqlConnection) -> u16 {
    let mut flags = 0;
    if connection.session_autocommit() {
        flags |= SERVER_STATUS_AUTOCOMMIT;
    }
    if !connection.is_auto_commit() {
        flags |= SERVER_STATUS_IN_TRANS;
    }
    flags
}

fn whole_second_timeout(timeout: Duration) -> Duration {
    let seconds = timeout
        .as_secs()
        .checked_add(u64::from(timeout.subsec_nanos() != 0))
        .expect("runtime timeout seconds must fit in u64");
    Duration::from_secs(seconds.max(1))
}

fn execute_bootstrap_query(
    sql: &str,
    settings: MySqlBootstrapSettings,
    status_flags: u16,
) -> Result<Option<CommandExecutionResult>, FrontendErrorKind> {
    match parse_driver_bootstrap_query(sql) {
        Ok(MySqlDriverBootstrapQuery::MaxAllowedPacketAndWaitTimeout) => {}
        Err(_) if contains_unrecognized_system_variable(sql) => {
            return Err(FrontendErrorKind::Unsupported);
        }
        Err(_) => return Ok(None),
    }

    Ok(Some(CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![
            column_definition("@@max_allowed_packet".to_owned(), MYSQL_TYPE_LONGLONG),
            column_definition("@@wait_timeout".to_owned(), MYSQL_TYPE_LONGLONG),
        ],
        rows: vec![vec![
            Some(settings.max_allowed_packet().to_string().into_bytes()),
            Some(settings.wait_timeout_seconds().to_string().into_bytes()),
        ]],
        warnings: 0,
        status_flags,
    })))
}

fn contains_unrecognized_system_variable(sql: &str) -> bool {
    if !is_select_statement(sql) {
        return false;
    }

    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut quote = None;
    while index < bytes.len() {
        match quote {
            Some(b'\'') => {
                if bytes[index] == b'\\' {
                    index = index.saturating_add(2);
                } else if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                    } else {
                        quote = None;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            Some(b'"') => {
                if bytes[index] == b'\\' {
                    index = index.saturating_add(2);
                } else if bytes[index] == b'"' {
                    if bytes.get(index + 1) == Some(&b'"') {
                        index += 2;
                    } else {
                        quote = None;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            Some(b'`') => {
                if bytes[index] == b'`' {
                    if bytes.get(index + 1) == Some(&b'`') {
                        index += 2;
                    } else {
                        quote = None;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            None => {
                if bytes[index] == b'\'' || bytes[index] == b'"' || bytes[index] == b'`' {
                    quote = Some(bytes[index]);
                    index += 1;
                } else if bytes[index] == b'@' && bytes.get(index + 1) == Some(&b'@') {
                    return true;
                } else if bytes[index] == b'#'
                    || (bytes[index] == b'-'
                        && bytes.get(index + 1) == Some(&b'-')
                        && bytes.get(index + 2).is_some_and(|byte| {
                            byte.is_ascii_whitespace() || byte.is_ascii_control()
                        }))
                {
                    index = bytes[index..]
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map_or(bytes.len(), |offset| index + offset + 1);
                } else if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    let Some(end) = sql[index + 2..].find("*/") else {
                        return false;
                    };
                    index += end + 4;
                } else {
                    index += 1;
                }
            }
            Some(_) => unreachable!("system-variable scanner only enters known quote states"),
        }
    }
    false
}

fn execute_checked_select_with_timeout(
    connection: &MySqlConnection,
    sql: &str,
    selected_database: Option<&str>,
    source_table: Option<&str>,
    query_timeout: Option<Duration>,
) -> Result<TextResultSet, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (selected_database, source_table);
    if !is_select_statement(sql) {
        return Err(FrontendErrorKind::Unsupported);
    }
    let (mut statement, static_result_metadata) = connection
        .prepare_select_with_metadata(sql)
        .map_err(frontend_prepare_error)?;
    let column_count = statement.num_columns();
    if column_count == 0 || column_count > MAX_RESULT_COLUMNS {
        return Err(FrontendErrorKind::Unsupported);
    }
    if statement.parameters_count() != 0 {
        // COM_QUERY has no binary-protocol parameter payload. Parameter
        // markers remain available to the embedded prepare API only.
        return Err(FrontendErrorKind::Unsupported);
    }

    let column_types = (0..column_count)
        .map(|index| {
            if connection.is_last_insert_id_result(&statement, index) {
                return Ok(Some(MYSQL_TYPE_LONGLONG));
            }
            let declared_type = statement.get_column_decltype(index);
            if let Some(column_type) = declared_type
                .as_deref()
                .and_then(mysql_type_for_declared_name)
            {
                return Ok(Some(column_type));
            }
            let primitive = statement
                .get_column_type_name(index)
                .or_else(|| statement.get_column_inferred_type(index));
            primitive
                .map(|name| mysql_type_for_name(&name).ok_or(FrontendErrorKind::Unsupported))
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut rows = Vec::new();
    let mut retained_bytes = 0usize;
    if let Some(timeout) = query_timeout {
        statement.set_query_timeout_override(Some(Some(timeout)));
    }
    statement
        .run_with_row_callback(|row| {
            if rows.len() >= MAX_DISPATCH_RESULT_ROWS {
                return Err(LimboError::TooBig);
            }
            if row.len() != column_count {
                return Err(LimboError::InternalError(
                    "frontend result row has an unexpected shape".to_string(),
                ));
            }
            let payload_len = checked_text_row_payload_len(row.get_values())?;
            let heap_overhead = std::mem::size_of::<Vec<Option<Vec<u8>>>>()
                .checked_add(
                    std::mem::size_of::<Option<Vec<u8>>>()
                        .checked_mul(column_count)
                        .ok_or(LimboError::TooBig)?,
                )
                .ok_or(LimboError::TooBig)?;
            retained_bytes = retained_bytes
                .checked_add(payload_len)
                .and_then(|total| total.checked_add(heap_overhead))
                .ok_or(LimboError::TooBig)?;
            if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
                return Err(LimboError::TooBig);
            }
            let values = row
                .get_values()
                .map(value_to_text_ref)
                .collect::<Result<Vec<_>, _>>()?;
            rows.push(values);
            Ok(())
        })
        .map_err(|error| {
            if query_timeout.is_some() && matches!(error, LimboError::Interrupt) {
                FrontendErrorKind::QueryTimeout
            } else {
                frontend_error_kind(error)
            }
        })?;

    #[cfg(unix)]
    let source_metadata =
        table_result_metadata(connection, &statement, selected_database, source_table)?;

    let columns = (0..column_count)
        .map(|index| {
            let name = statement.get_column_name(index).into_owned();
            match (static_result_metadata.len() == column_count)
                .then(|| static_result_metadata[index].as_ref())
                .flatten()
            {
                Some(metadata) => Ok(static_column_definition(name, metadata)),
                None => {
                    #[cfg(unix)]
                    if let Some(source_metadata) = source_metadata.as_ref() {
                        return source_metadata.column_definition(
                            &statement,
                            index,
                            name,
                            column_types[index],
                        );
                    }
                    Ok(column_definition(
                        name,
                        column_types[index].unwrap_or(MYSQL_TYPE_NULL),
                    ))
                }
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TextResultSet {
        columns,
        rows,
        warnings: 0,
        status_flags: 0x0002,
    })
}

#[cfg(unix)]
struct TableResultMetadata {
    database: String,
    source_table: String,
    table_reference: String,
    columns: Vec<MySqlColumnMetadata>,
}

#[cfg(unix)]
impl TableResultMetadata {
    fn column_definition(
        &self,
        statement: &Statement,
        index: usize,
        name: String,
        fallback_type: Option<u8>,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        self.column_definition_for_reference(
            statement
                .get_column_source_reference(index)
                .map(|(table, ordinal)| (table.into_owned(), ordinal)),
            name,
            fallback_type,
        )
    }

    fn column_definition_for_reference(
        &self,
        source_reference: Option<(String, usize)>,
        name: String,
        fallback_type: Option<u8>,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        let Some((table_reference, ordinal)) = source_reference else {
            return Ok(column_definition(
                name,
                fallback_type.unwrap_or(MYSQL_TYPE_NULL),
            ));
        };
        if table_reference != self.table_reference {
            return Err(FrontendErrorKind::Unsupported);
        }
        let source = self
            .columns
            .get(ordinal)
            .ok_or(FrontendErrorKind::Internal)?;
        let column_type = mysql_type_for_declared_name(source.type_name())
            .or(fallback_type)
            .ok_or(FrontendErrorKind::Unsupported)?;
        let mut definition = column_definition(name, column_type);
        if let Some((precision, scale)) = source.decimal_size() {
            // Measured on MySQL 8.4.11: the precision, one for the sign, and one
            // more for the point when the scale is above zero. Held for
            // DECIMAL(10,2)=12, (5,0)=6, (65,30)=67, (10,0)=11, (1,1)=3 and
            // (20,4)=22.
            definition.column_length = precision + 1 + u32::from(scale > 0);
            definition.decimals = scale as u8;
        }
        if matches!(source.type_name(), "DATETIME" | "TIMESTAMP") {
            // Measured on MySQL 8.4.11: 19, the width of the text form, for
            // both.
            definition.column_length = 19;
        }
        if source.type_name() == "BOOLEAN" {
            // Measured on MySQL 8.4.11: a BOOLEAN column reports 1, the display
            // width in `tinyint(1)`, where a plain TINYINT reports 4.
            definition.column_length = 1;
        }
        if let Some(length) = source.character_length() {
            // Measured on MySQL 8.4.11: a `VARCHAR(4)` and a `CHAR(4)` both
            // report 16. The declared count is characters, and the reported
            // length reserves the four bytes utf8mb4 needs for one.
            definition.column_length = length.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        }
        definition.schema.clone_from(&self.database);
        definition.table = table_reference;
        definition.original_table.clone_from(&self.source_table);
        source.name().clone_into(&mut definition.original_name);
        definition.flags = mysql_table_column_flags(source);
        if matches!(source.type_name(), "DATETIME" | "TIMESTAMP") {
            // Measured: a temporal column carries the binary flag, because it
            // has no collation of its own.
            definition.flags |= MYSQL_BINARY_FLAG;
        }
        Ok(definition)
    }
}

#[cfg(unix)]
fn table_result_metadata(
    connection: &MySqlConnection,
    statement: &Statement,
    selected_database: Option<&str>,
    source_table: Option<&str>,
) -> Result<Option<TableResultMetadata>, FrontendErrorKind> {
    let source_references = (0..statement.num_columns())
        .filter_map(|index| {
            statement
                .get_column_source_reference(index)
                .map(|(table, ordinal)| (table.into_owned(), ordinal))
        })
        .collect::<Vec<_>>();
    table_result_metadata_for_references(
        connection,
        &source_references,
        selected_database,
        source_table,
    )
}

#[cfg(unix)]
fn prepared_table_result_metadata(
    connection: &MySqlConnection,
    type_metadata: &[MySqlPreparedResultColumnTypeMetadata],
    selected_database: Option<&str>,
    source_table: Option<&str>,
) -> Result<Option<TableResultMetadata>, FrontendErrorKind> {
    let source_references = type_metadata
        .iter()
        .filter_map(|metadata| {
            metadata
                .source_reference()
                .map(|(table, ordinal)| (table.to_owned(), ordinal))
        })
        .collect::<Vec<_>>();
    table_result_metadata_for_references(
        connection,
        &source_references,
        selected_database,
        source_table,
    )
}

#[cfg(unix)]
fn table_result_metadata_for_references(
    connection: &MySqlConnection,
    source_references: &[(String, usize)],
    selected_database: Option<&str>,
    source_table: Option<&str>,
) -> Result<Option<TableResultMetadata>, FrontendErrorKind> {
    let Some((table_reference, _)) = source_references.first() else {
        return Ok(None);
    };
    if source_references
        .iter()
        .any(|(reference, _)| reference != table_reference)
    {
        return Err(FrontendErrorKind::Unsupported);
    }
    let Some(selected_database) = selected_database else {
        return Ok(None);
    };
    let source_table = source_table.ok_or(FrontendErrorKind::Internal)?;

    // View output metadata has different visibility and key/default semantics
    // from its base table. Keep it on the established generic path until its
    // MySQL wire fields have an oracle-backed contract.
    let table_kind = connection
        .list_tables()
        .map_err(|_| FrontendErrorKind::Internal)?
        .into_iter()
        .find(|table| table.name().eq_ignore_ascii_case(source_table))
        .map(|table| table.kind())
        .ok_or(FrontendErrorKind::MissingObject)?;
    if table_kind != MySqlTableKind::BaseTable {
        return Ok(None);
    }

    let table = MySqlTableName::parse(source_table).map_err(|_| FrontendErrorKind::Syntax)?;
    let columns = connection
        .list_columns(&table)
        .map_err(column_metadata_error_kind)?;
    for (_, ordinal) in source_references {
        if columns.get(*ordinal).is_none() {
            return Err(FrontendErrorKind::Internal);
        }
    }
    Ok(Some(TableResultMetadata {
        database: selected_database.to_owned(),
        source_table: source_table.to_owned(),
        table_reference: table_reference.clone(),
        columns,
    }))
}

#[cfg(unix)]
fn mysql_table_column_flags(column: &MySqlColumnMetadata) -> u16 {
    let mut flags = 0;
    if !column.nullable() {
        flags |= MYSQL_NOT_NULL_FLAG;
    }
    match column.key() {
        MySqlColumnKey::Primary => {
            flags |= MYSQL_PRI_KEY_FLAG | MYSQL_PART_KEY_FLAG;
        }
        MySqlColumnKey::Unique => {
            flags |= MYSQL_UNIQUE_KEY_FLAG | MYSQL_PART_KEY_FLAG;
        }
        // The column is part of a key without being unique on its own.
        MySqlColumnKey::Multiple => {
            flags |= MYSQL_PART_KEY_FLAG;
        }
        MySqlColumnKey::None => {}
    }
    if !column.nullable()
        && column.default_value().is_none()
        && !column.extra().eq_ignore_ascii_case("AUTO_INCREMENT")
    {
        flags |= MYSQL_NO_DEFAULT_VALUE_FLAG;
    }
    if column.extra().eq_ignore_ascii_case("AUTO_INCREMENT") {
        flags |= MYSQL_AUTO_INCREMENT_FLAG;
    }
    flags
}

const MYSQL_TYPE_TINY: u8 = 0x01;
const MYSQL_TYPE_SHORT: u8 = 0x02;
const MYSQL_TYPE_INT24: u8 = 0x09;
const MYSQL_TYPE_LONG: u8 = 0x03;
const MYSQL_TYPE_DOUBLE: u8 = 0x05;
const MYSQL_TYPE_NULL: u8 = 0x06;
const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_TYPE_STRING: u8 = 0xfe;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
const MYSQL_TYPE_BLOB: u8 = 0xfc;
const MYSQL_TYPE_DATETIME: u8 = 0x0c;
const MYSQL_TYPE_TIMESTAMP: u8 = 0x07;
const MYSQL_TYPE_NEWDECIMAL: u8 = 0xf6;
pub(crate) const MYSQL_NOT_NULL_FLAG: u16 = 1;
#[cfg(unix)]
const MYSQL_PRI_KEY_FLAG: u16 = 2;
#[cfg(unix)]
const MYSQL_UNIQUE_KEY_FLAG: u16 = 4;
#[cfg(unix)]
const MYSQL_PART_KEY_FLAG: u16 = 16_384;
const MYSQL_BLOB_FLAG: u16 = 16;
const MYSQL_UNSIGNED_FLAG: u16 = 32;
const MYSQL_BINARY_FLAG: u16 = 128;
const MYSQL_ENUM_FLAG: u16 = 256;
#[cfg(unix)]
const MYSQL_AUTO_INCREMENT_FLAG: u16 = 512;
pub(crate) const MYSQL_NO_DEFAULT_VALUE_FLAG: u16 = 4096;
const MYSQL_BINARY_COLLATION: u16 = 63;
/// Bytes utf8mb4 reserves for one character, which MySQL multiplies a declared
/// character count by when it reports a column's length.
const UTF8MB4_MAX_BYTES_PER_CHARACTER: u32 = 4;
const MAX_FRONTEND_ADAPTER_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PREPARED_LONG_DATA_BYTES: usize = 8 * 1024 * 1024;

impl PendingLongData {
    fn append(&mut self, statement_id: u32, parameter_id: u16, data: &[u8], parameter_count: u16) {
        if self.errors.contains_key(&statement_id) {
            return;
        }
        if parameter_id >= parameter_count {
            self.errors
                .insert(statement_id, PendingLongDataError::InvalidParameter);
            return;
        }
        let Some(retained_bytes) = self.retained_bytes.checked_add(data.len()) else {
            self.fail_statement(statement_id, PendingLongDataError::TooLarge);
            return;
        };
        if retained_bytes > MAX_PREPARED_LONG_DATA_BYTES {
            self.fail_statement(statement_id, PendingLongDataError::TooLarge);
            return;
        }
        self.values
            .entry((statement_id, parameter_id))
            .or_default()
            .extend_from_slice(data);
        self.retained_bytes = retained_bytes;
    }

    fn take_statement(&mut self, statement_id: u32) -> StatementLongData {
        let error = self.errors.remove(&statement_id);
        let parameter_count = self
            .values
            .keys()
            .filter_map(|&(id, parameter_id)| {
                (id == statement_id).then_some(usize::from(parameter_id) + 1)
            })
            .max()
            .unwrap_or(0);
        let mut values = (0..parameter_count).map(|_| None).collect::<Vec<_>>();
        let parameter_ids = self
            .values
            .keys()
            .filter_map(|&(id, parameter_id)| (id == statement_id).then_some(parameter_id))
            .collect::<Vec<_>>();
        for parameter_id in parameter_ids {
            let value = self
                .values
                .remove(&(statement_id, parameter_id))
                .expect("pending long-data key was collected above");
            self.retained_bytes -= value.len();
            values[usize::from(parameter_id)] = Some(value);
        }
        StatementLongData { values, error }
    }

    fn clear_statement(&mut self, statement_id: u32) {
        let _ = self.take_statement(statement_id);
    }

    fn fail_statement(&mut self, statement_id: u32, error: PendingLongDataError) {
        let parameter_ids = self
            .values
            .keys()
            .filter_map(|&(id, parameter_id)| (id == statement_id).then_some(parameter_id))
            .collect::<Vec<_>>();
        for parameter_id in parameter_ids {
            let value = self
                .values
                .remove(&(statement_id, parameter_id))
                .expect("pending long-data key was collected above");
            self.retained_bytes -= value.len();
        }
        self.errors.insert(statement_id, error);
    }
}

fn pending_long_data_error(error: PendingLongDataError) -> FrontendErrorKind {
    match error {
        PendingLongDataError::InvalidParameter | PendingLongDataError::TooLarge => {
            FrontendErrorKind::Syntax
        }
    }
}

fn is_select_statement(sql: &str) -> bool {
    statement_keyword(sql).is_some_and(|keyword| keyword.eq_ignore_ascii_case("SELECT"))
}

fn is_checked_write_statement(sql: &str) -> bool {
    statement_keyword(sql).is_some_and(|keyword| {
        keyword.eq_ignore_ascii_case("INSERT")
            || keyword.eq_ignore_ascii_case("DELETE")
            || keyword.eq_ignore_ascii_case("UPDATE")
    })
}

fn is_schema_statement(sql: &str) -> bool {
    statement_keyword(sql).is_some_and(|keyword| {
        keyword.eq_ignore_ascii_case("CREATE") || keyword.eq_ignore_ascii_case("ALTER")
    })
}

fn statement_keyword(sql: &str) -> Option<&str> {
    let sql = strip_leading_sql_comments(sql);
    let end = sql
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .unwrap_or(sql.len());
    (!sql[..end].is_empty()).then_some(&sql[..end])
}

fn strip_leading_sql_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(comment) = sql.strip_prefix("/*") {
            let Some(end) = comment.find("*/") else {
                return sql;
            };
            sql = &comment[end + 2..];
            continue;
        }
        if let Some(comment) = sql.strip_prefix("--") {
            let Some(first) = comment.chars().next() else {
                return sql;
            };
            if !(first.is_ascii_whitespace() || first.is_control()) {
                return sql;
            }
            sql = comment.find('\n').map_or("", |index| &comment[index + 1..]);
            continue;
        }
        if let Some(comment) = sql.strip_prefix('#') {
            sql = comment.find('\n').map_or("", |index| &comment[index + 1..]);
            continue;
        }
        return sql;
    }
}

fn mysql_type_for_name(name: &str) -> Option<u8> {
    match name {
        "TINYINT" => Some(MYSQL_TYPE_TINY),
        "SMALLINT" => Some(MYSQL_TYPE_SHORT),
        "MEDIUMINT" => Some(MYSQL_TYPE_INT24),
        "INT" => Some(MYSQL_TYPE_LONG),
        "INTEGER" => Some(MYSQL_TYPE_LONGLONG),
        "BIGINT" => Some(MYSQL_TYPE_LONGLONG),
        "REAL" => Some(MYSQL_TYPE_DOUBLE),
        "TEXT" => Some(MYSQL_TYPE_VAR_STRING),
        "BLOB" => Some(MYSQL_TYPE_BLOB),
        _ => None,
    }
}

fn mysql_type_for_declared_name(name: &str) -> Option<u8> {
    if name.eq_ignore_ascii_case("VARCHAR") {
        return Some(MYSQL_TYPE_VAR_STRING);
    }
    // Measured on MySQL 8.4.11: a CHAR column reports 254, not 253.
    if name.eq_ignore_ascii_case("CHAR") {
        return Some(MYSQL_TYPE_STRING);
    }
    if name.eq_ignore_ascii_case("DOUBLE") {
        return Some(MYSQL_TYPE_DOUBLE);
    }
    if name.eq_ignore_ascii_case("BOOLEAN") {
        return Some(MYSQL_TYPE_TINY);
    }
    if name.eq_ignore_ascii_case("DATETIME") {
        return Some(MYSQL_TYPE_DATETIME);
    }
    if name.eq_ignore_ascii_case("TIMESTAMP") {
        return Some(MYSQL_TYPE_TIMESTAMP);
    }
    if name.eq_ignore_ascii_case("DECIMAL") {
        return Some(MYSQL_TYPE_NEWDECIMAL);
    }
    if name.eq_ignore_ascii_case("INTEGER") {
        return Some(MYSQL_TYPE_LONG);
    }
    if name.eq_ignore_ascii_case("TINYINT") {
        return Some(MYSQL_TYPE_TINY);
    }
    if name.eq_ignore_ascii_case("SMALLINT") {
        return Some(MYSQL_TYPE_SHORT);
    }
    if name.eq_ignore_ascii_case("MEDIUMINT") {
        return Some(MYSQL_TYPE_INT24);
    }
    if name.eq_ignore_ascii_case("INT") {
        return Some(MYSQL_TYPE_LONG);
    }
    if name.eq_ignore_ascii_case("BIGINT") {
        return Some(MYSQL_TYPE_LONGLONG);
    }
    if name.eq_ignore_ascii_case("REAL") {
        return Some(MYSQL_TYPE_DOUBLE);
    }
    if name.eq_ignore_ascii_case("TEXT") {
        return Some(MYSQL_TYPE_VAR_STRING);
    }
    if name.eq_ignore_ascii_case("BLOB") {
        return Some(MYSQL_TYPE_BLOB);
    }
    None
}

fn mysql_type_for_declared_or_inferred(
    declared_name: Option<&str>,
    inferred_name: Option<&str>,
) -> Option<u8> {
    declared_name
        .and_then(mysql_type_for_declared_name)
        .or_else(|| inferred_name.and_then(mysql_type_for_name))
}

fn mysql_type_for_prepared_column(
    column: &MySqlPreparedResultColumn,
    type_metadata: &MySqlPreparedResultColumnTypeMetadata,
) -> Option<u8> {
    if let Some(metadata) = type_metadata.static_metadata() {
        return Some(static_result_column_metadata(metadata).column_type);
    }
    if let Some(marker) = type_metadata.parameter_marker() {
        if let Some(column_type) = marker_column_type(marker.kind()) {
            return Some(column_type);
        }
    }
    mysql_type_for_declared_or_inferred(
        type_metadata.declared_type_name(),
        column.type_name.as_deref(),
    )
}

/// The type MySQL reports for a `?` result column, when this frontend can send
/// a matching row for it.
fn marker_column_type(kind: MySqlMarkerType) -> Option<u8> {
    match kind {
        MySqlMarkerType::Untyped => Some(MYSQL_TYPE_VAR_STRING),
        MySqlMarkerType::Integer => Some(MYSQL_TYPE_LONGLONG),
        MySqlMarkerType::Real => Some(MYSQL_TYPE_DOUBLE),
        MySqlMarkerType::RowDecides => None,
    }
}

/// The definition MySQL sends for a `?` result column.
///
/// These lengths and decimals differ from the ones a table column of the same
/// type gets, so they are kept apart from `column_definition`. Measured from
/// MySQL 8.4.11 over the wire.
fn marker_column_definition(name: String, kind: MySqlMarkerType) -> Option<ColumnDefinitionConfig> {
    let mut definition = ColumnDefinitionConfig::new(name, marker_column_type(kind)?);
    let (character_set, column_length, decimals, flags) = match kind {
        MySqlMarkerType::Untyped => (
            u16::from(DEFAULT_UTF8MB4_COLLATION),
            65_532,
            NOT_FIXED_DECIMALS,
            0,
        ),
        MySqlMarkerType::Integer => (MYSQL_BINARY_COLLATION, 21, 0, MYSQL_BINARY_FLAG),
        MySqlMarkerType::Real => (
            MYSQL_BINARY_COLLATION,
            23,
            NOT_FIXED_DECIMALS,
            MYSQL_BINARY_FLAG,
        ),
        MySqlMarkerType::RowDecides => return None,
    };
    definition.character_set = character_set;
    definition.column_length = column_length;
    definition.decimals = decimals;
    definition.flags = flags;
    Some(definition)
}

/// MySQL's "no fixed number of decimals" marker.
pub(crate) const NOT_FIXED_DECIMALS: u8 = 31;

fn column_definition(name: String, column_type: u8) -> ColumnDefinitionConfig {
    let mut definition = ColumnDefinitionConfig::new(name, column_type);
    // Measured on MySQL 8.4.11: a CHAR column carries the text collation just
    // as a VARCHAR one does, though it reports type 254 rather than 253.
    definition.character_set = if matches!(column_type, MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING) {
        u16::from(DEFAULT_UTF8MB4_COLLATION)
    } else {
        MYSQL_BINARY_COLLATION
    };
    definition.column_length = match column_type {
        MYSQL_TYPE_TINY => 4,
        MYSQL_TYPE_SHORT => 6,
        MYSQL_TYPE_INT24 => 9,
        MYSQL_TYPE_LONG => 11,
        MYSQL_TYPE_LONGLONG => 20,
        MYSQL_TYPE_DOUBLE => 22,
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_BLOB => MAX_TEXT_ROW_VALUE_LENGTH as u32,
        MYSQL_TYPE_NULL => 0,
        _ => 0,
    };
    // Measured on MySQL 8.4.11: a DOUBLE column reports 31, the value that
    // says the count of decimal places is not fixed.
    if column_type == MYSQL_TYPE_DOUBLE {
        definition.decimals = NOT_FIXED_DECIMALS;
    }
    definition
}

fn checked_text_row_payload_len<'a>(
    values: impl Iterator<Item = &'a Value>,
) -> Result<usize, LimboError> {
    let mut payload_len = 0usize;
    for value in values {
        let value_len = match value {
            Value::Null => 1,
            Value::Numeric(Numeric::Integer(_)) | Value::Numeric(Numeric::Float(_)) => {
                let bytes = value.to_string().len();
                length_encoded_value_len(bytes)?
            }
            Value::Text(text) => length_encoded_value_len(text.as_str().len())?,
            Value::Blob(blob) => length_encoded_value_len(blob.len())?,
        };
        payload_len = payload_len
            .checked_add(value_len)
            .ok_or(LimboError::TooBig)?;
    }
    if payload_len > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
        return Err(LimboError::TooBig);
    }
    Ok(payload_len)
}

fn length_encoded_value_len(bytes: usize) -> Result<usize, LimboError> {
    if bytes > MAX_TEXT_ROW_VALUE_LENGTH {
        return Err(LimboError::TooBig);
    }
    let prefix: usize = match bytes {
        0..=250 => 1,
        251..=65_535 => 3,
        65_536..=16_777_215 => 4,
        _ => 9,
    };
    prefix.checked_add(bytes).ok_or(LimboError::TooBig)
}

fn value_to_text_ref(value: &Value) -> Result<Option<Vec<u8>>, LimboError> {
    match value {
        Value::Null => Ok(None),
        Value::Numeric(Numeric::Integer(_)) | Value::Numeric(Numeric::Float(_)) => {
            Ok(Some(value.to_string().into_bytes()))
        }
        Value::Text(text) => {
            if text.as_str().len() > MAX_TEXT_ROW_VALUE_LENGTH {
                return Err(LimboError::TooBig);
            }
            Ok(Some(text.as_str().as_bytes().to_vec()))
        }
        Value::Blob(blob) => {
            if blob.len() > MAX_TEXT_ROW_VALUE_LENGTH {
                return Err(LimboError::TooBig);
            }
            Ok(Some(blob.to_vec()))
        }
    }
}

fn frontend_error_kind(error: LimboError) -> FrontendErrorKind {
    match error {
        LimboError::NotNullConstraint { .. } => FrontendErrorKind::NotNullViolation,
        LimboError::NoSuchColumn { .. } => FrontendErrorKind::UnknownColumn,
        LimboError::Assignment(error)
            if matches!(*error, turso_core::AssignmentError::TooLong { .. }) =>
        {
            FrontendErrorKind::DataTooLong
        }
        LimboError::Assignment(error)
            if matches!(*error, turso_core::AssignmentError::IncorrectType { .. }) =>
        {
            FrontendErrorKind::IncorrectValue
        }
        LimboError::Assignment(error)
            if matches!(
                *error,
                turso_core::AssignmentError::IncorrectTemporal { .. }
            ) =>
        {
            FrontendErrorKind::IncorrectTemporalValue
        }
        LimboError::Constraint(_)
        | LimboError::ForeignKeyConstraint(_)
        | LimboError::Raise(..)
        | LimboError::NullValue => FrontendErrorKind::ConstraintViolation,
        _ => FrontendErrorKind::Unsupported,
    }
}

fn frontend_prepare_error(error: MySqlQueryError) -> FrontendErrorKind {
    match error {
        MySqlQueryError::MissingRequiredDefault(_) => FrontendErrorKind::MissingRequiredDefault,
        MySqlQueryError::Syntax(_) => FrontendErrorKind::Syntax,
        MySqlQueryError::Unsupported(_) => FrontendErrorKind::Unsupported,
        MySqlQueryError::Engine(error) => frontend_error_kind(error),
    }
}

#[cfg(unix)]
fn admin_error_kind(error: MySqlAdminCommandError) -> FrontendErrorKind {
    match error {
        MySqlAdminCommandError::Syntax => FrontendErrorKind::Syntax,
        MySqlAdminCommandError::Database(error) => database_error_kind(error),
    }
}

#[cfg(unix)]
fn admin_result_to_execution_result(
    result: MySqlAdminCommandResult,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    match result {
        MySqlAdminCommandResult::Created { .. }
        | MySqlAdminCommandResult::Dropped { .. }
        | MySqlAdminCommandResult::Selected { .. } => {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }
        MySqlAdminCommandResult::Listed { databases } => {
            if databases.len() > MAX_DISPATCH_RESULT_ROWS {
                return Err(FrontendErrorKind::Internal);
            }
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![database_list_column()],
                rows: databases
                    .into_iter()
                    .map(|database| Some(database.into_bytes()))
                    .map(|value| vec![value])
                    .collect(),
                warnings: 0,
                status_flags: 0x0002,
            }))
        }
    }
}

#[cfg(unix)]
fn information_schema_schemata_result_to_execution_result(
    databases: Vec<String>,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let result = admin_result_to_execution_result(MySqlAdminCommandResult::Listed { databases })?;
    let CommandExecutionResult::ResultSet(mut result) = result else {
        unreachable!("SCHEMATA provider always returns a result set");
    };
    result.columns = vec![information_schema_schemata_column()];
    Ok(CommandExecutionResult::ResultSet(result))
}

#[cfg(unix)]
fn information_schema_schemata_column() -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new("SCHEMA_NAME", MYSQL_TYPE_VAR_STRING);
    "information_schema".clone_into(&mut column.schema);
    "SCHEMATA".clone_into(&mut column.table);
    "SCHEMATA".clone_into(&mut column.original_table);
    "SCHEMA_NAME".clone_into(&mut column.original_name);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 256;
    column.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG;
    column
}

#[cfg(unix)]
fn database_list_column() -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new("Database", MYSQL_TYPE_VAR_STRING);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 64;
    column
}

#[cfg(unix)]
fn show_tables_result_to_execution_result(
    database: &str,
    tables: impl IntoIterator<Item = String>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let tables = tables.into_iter().collect::<Vec<_>>();
    if tables.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let rows = tables
        .into_iter()
        .map(|name| {
            if name.len() > MAX_TEXT_ROW_VALUE_LENGTH {
                return Err(FrontendErrorKind::Internal);
            }
            retained_bytes = retained_bytes
                .checked_add(name.len())
                .and_then(|total| {
                    total.checked_add(
                        std::mem::size_of::<Vec<Option<Vec<u8>>>>()
                            + std::mem::size_of::<Option<Vec<u8>>>(),
                    )
                })
                .ok_or(FrontendErrorKind::Internal)?;
            if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
                return Err(FrontendErrorKind::Internal);
            }
            Ok(vec![Some(name.into_bytes())])
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![show_tables_column(database)],
        rows,
        warnings: 0,
        status_flags,
    }))
}

#[cfg(unix)]
fn show_full_tables_result_to_execution_result(
    database: &str,
    tables: impl IntoIterator<Item = turso_mysql::MySqlTable>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let CommandExecutionResult::ResultSet(mut result) =
        information_schema_tables_result_to_execution_result(database, tables, status_flags)?
    else {
        unreachable!("catalog provider always returns a result set");
    };
    for row in &mut result.rows {
        row.remove(0);
    }
    let mut name = show_tables_column(database);
    name.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let mut kind = ColumnDefinitionConfig::new("Table_type", MYSQL_TYPE_STRING);
    kind.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    kind.column_length = 44;
    kind.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    for column in [&mut name, &mut kind] {
        column.catalog = "def".into();
        column.table = "TABLES".into();
        column.original_table = "tables".into();
        column.original_name = column.name.clone();
    }
    result.columns = vec![name, kind];
    Ok(CommandExecutionResult::ResultSet(result))
}

#[cfg(unix)]
fn information_schema_tables_result_to_execution_result(
    database: &str,
    tables: impl IntoIterator<Item = turso_mysql::MySqlTable>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let tables = tables.into_iter().collect::<Vec<_>>();
    if tables.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(tables.len());
    for table in tables {
        let table_type = match table.kind() {
            MySqlTableKind::BaseTable => b"BASE TABLE".as_slice(),
            MySqlTableKind::View => b"VIEW".as_slice(),
        };
        let row = vec![
            Some(database.as_bytes().to_vec()),
            Some(table.name().as_bytes().to_vec()),
            Some(table_type.to_vec()),
        ];
        if row
            .iter()
            .flatten()
            .any(|value| value.len() > MAX_TEXT_ROW_VALUE_LENGTH)
        {
            return Err(FrontendErrorKind::Internal);
        }
        checked_text_result_row_payload_len(&row)?;

        let row_bytes = row
            .iter()
            .flatten()
            .map(Vec::len)
            .try_fold(0usize, usize::checked_add)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<Option<Vec<u8>>>>()))
            .and_then(|bytes| {
                std::mem::size_of::<Option<Vec<u8>>>()
                    .checked_mul(row.len())
                    .and_then(|row_storage| bytes.checked_add(row_storage))
            })
            .ok_or(FrontendErrorKind::Internal)?;
        retained_bytes = retained_bytes
            .checked_add(row_bytes)
            .ok_or(FrontendErrorKind::Internal)?;
        if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
            return Err(FrontendErrorKind::Internal);
        }
        rows.push(row);
    }

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: information_schema_tables_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

#[cfg(unix)]
fn information_schema_tables_columns() -> Vec<ColumnDefinitionConfig> {
    // TABLE_SCHEMA's original table really is `schemata` in MySQL. Every value here comes from the
    // pinned MySQL 8.4.11 golden `information-schema-tables.json`.
    [
        (
            "TABLE_SCHEMA",
            "schemata",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        (
            "TABLE_NAME",
            "tables",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        (
            "TABLE_TYPE",
            "tables",
            MYSQL_TYPE_STRING,
            44,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
    ]
    .into_iter()
    .map(
        |(name, original_table, column_type, column_length, flags)| {
            let mut column = ColumnDefinitionConfig::new(name, column_type);
            "information_schema".clone_into(&mut column.schema);
            "TABLES".clone_into(&mut column.table);
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            column.column_length = column_length;
            column.flags = flags;
            column
        },
    )
    .collect()
}

#[cfg(unix)]
fn information_schema_columns_result_to_execution_result(
    columns: Vec<MySqlColumnMetadata>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if columns.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(columns.len());
    for (ordinal, column) in columns.into_iter().enumerate() {
        if column.name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.extra().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let column_type = show_column_type_name(&column)?;
        let extra = show_column_extra(column.extra())?;
        let default = match column.default_value() {
            Some(MySqlColumnDefault::Text(value)) if value.len() > MAX_TEXT_ROW_VALUE_LENGTH => {
                return Err(FrontendErrorKind::Internal);
            }
            _ => show_column_default_value(column.default_value())?,
        };
        let ordinal = (ordinal + 1).to_string().into_bytes();
        let nullable = if column.nullable() {
            b"YES".as_slice()
        } else {
            b"NO".as_slice()
        };
        let key = match column.key() {
            MySqlColumnKey::None => b"".as_slice(),
            MySqlColumnKey::Multiple => b"MUL".as_slice(),
            MySqlColumnKey::Unique => b"UNI".as_slice(),
            MySqlColumnKey::Primary => b"PRI".as_slice(),
        };
        let value_lengths = [
            column.name().len(),
            ordinal.len(),
            default.as_ref().map_or(0, Vec::len),
            nullable.len(),
            column_type.len(),
            key.len(),
            extra.len(),
        ];
        if value_lengths
            .iter()
            .any(|length| *length > MAX_TEXT_ROW_VALUE_LENGTH)
        {
            return Err(FrontendErrorKind::Internal);
        }
        let payload_len = value_lengths
            .iter()
            .try_fold(0usize, |payload_len, length| {
                length_encoded_value_len(*length)
                    .map_err(|_| FrontendErrorKind::Internal)?
                    .checked_add(payload_len)
                    .ok_or(FrontendErrorKind::Internal)
            })?;
        if payload_len > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
            return Err(FrontendErrorKind::Internal);
        }
        let row_bytes = value_lengths
            .iter()
            .try_fold(0usize, |row_bytes, length| {
                row_bytes
                    .checked_add(*length)
                    .ok_or(FrontendErrorKind::Internal)
            })?
            .checked_add(std::mem::size_of::<Vec<Option<Vec<u8>>>>())
            .and_then(|bytes| {
                std::mem::size_of::<Option<Vec<u8>>>()
                    .checked_mul(value_lengths.len())
                    .and_then(|row_storage| bytes.checked_add(row_storage))
            })
            .ok_or(FrontendErrorKind::Internal)?;
        retained_bytes = retained_bytes
            .checked_add(row_bytes)
            .ok_or(FrontendErrorKind::Internal)?;
        if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
            return Err(FrontendErrorKind::Internal);
        }

        rows.push(vec![
            Some(column.name().as_bytes().to_vec()),
            Some(ordinal),
            default,
            Some(nullable.to_vec()),
            Some(column_type.to_vec()),
            Some(key.to_vec()),
            Some(extra.to_vec()),
        ]);
    }

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: information_schema_columns_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

#[cfg(unix)]
fn information_schema_columns_columns() -> Vec<ColumnDefinitionConfig> {
    let column_name = information_schema_column_definition(
        "COLUMN_NAME",
        MYSQL_TYPE_VAR_STRING,
        256,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );

    let mut ordinal_position = information_schema_column_definition(
        "ORDINAL_POSITION",
        MYSQL_TYPE_LONG,
        10,
        MYSQL_BINARY_COLLATION,
        true,
    );
    ordinal_position.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;

    let mut column_default = information_schema_column_definition(
        "COLUMN_DEFAULT",
        MYSQL_TYPE_BLOB,
        262_140,
        DEFAULT_UTF8MB4_COLLATION.into(),
        true,
    );
    column_default.flags = MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;

    let mut is_nullable = information_schema_column_definition(
        "IS_NULLABLE",
        MYSQL_TYPE_VAR_STRING,
        12,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );
    is_nullable.flags = MYSQL_NOT_NULL_FLAG;

    let mut column_type = information_schema_column_definition(
        "COLUMN_TYPE",
        MYSQL_TYPE_BLOB,
        67_108_860,
        DEFAULT_UTF8MB4_COLLATION.into(),
        true,
    );
    column_type.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;

    let mut column_key = information_schema_column_definition(
        "COLUMN_KEY",
        MYSQL_TYPE_STRING,
        12,
        DEFAULT_UTF8MB4_COLLATION.into(),
        true,
    );
    column_key.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;

    let extra = information_schema_column_definition(
        "EXTRA",
        MYSQL_TYPE_VAR_STRING,
        1024,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );

    vec![
        column_name,
        ordinal_position,
        column_default,
        is_nullable,
        column_type,
        column_key,
        extra,
    ]
}

#[cfg(unix)]
fn information_schema_column_definition(
    name: &str,
    column_type: u8,
    column_length: u32,
    character_set: u16,
    has_original_table: bool,
) -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new(name, column_type);
    "def".clone_into(&mut column.catalog);
    "information_schema".clone_into(&mut column.schema);
    "COLUMNS".clone_into(&mut column.table);
    if has_original_table {
        "columns".clone_into(&mut column.original_table);
    }
    name.clone_into(&mut column.original_name);
    column.character_set = character_set;
    column.column_length = column_length;
    column
}

/// Refuses a `database.` qualifier that names anything but the selected
/// database.
///
/// MySQL resolves such a qualifier against any database the caller can reach,
/// which means authorizing against the named one rather than the selected one.
/// Until that is built, only the redundant qualifier clients write right after
/// `USE` is taken.
#[cfg(unix)]
fn reject_other_database_qualifier(
    qualifier: Option<&MySqlDatabaseName>,
    selected_database: &str,
) -> Result<(), FrontendErrorKind> {
    match qualifier {
        None => Ok(()),
        Some(qualifier) if qualifier.as_str().eq_ignore_ascii_case(selected_database) => Ok(()),
        Some(_) => Err(FrontendErrorKind::Unsupported),
    }
}

/// The fifteen columns `SHOW INDEX` returns, in MySQL's order.
///
/// Cardinality is always NULL: it is a statistic MySQL gathers and Turso does
/// not, and MySQL itself sends NULL when it has none.
#[cfg(unix)]
fn show_index_result_to_execution_result(
    table: &str,
    entries: Vec<MySqlIndexEntry>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if entries.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.key_name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || entry.column_name().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        rows.push(vec![
            Some(table.as_bytes().to_vec()),
            Some(if entry.unique() {
                b"0".to_vec()
            } else {
                b"1".to_vec()
            }),
            Some(entry.key_name().as_bytes().to_vec()),
            Some(entry.sequence_in_index().to_string().into_bytes()),
            Some(entry.column_name().as_bytes().to_vec()),
            Some(b"A".to_vec()),
            None,
            None,
            None,
            Some(if entry.nullable() {
                b"YES".to_vec()
            } else {
                Vec::new()
            }),
            Some(b"BTREE".to_vec()),
            Some(Vec::new()),
            Some(Vec::new()),
            Some(b"YES".to_vec()),
            None,
        ]);
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_index_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

#[cfg(unix)]
fn show_index_columns() -> Vec<ColumnDefinitionConfig> {
    [
        ("Table", MYSQL_TYPE_VAR_STRING, 256u32),
        ("Non_unique", MYSQL_TYPE_LONGLONG, 1),
        ("Key_name", MYSQL_TYPE_VAR_STRING, 256),
        ("Seq_in_index", MYSQL_TYPE_LONGLONG, 21),
        ("Column_name", MYSQL_TYPE_VAR_STRING, 256),
        ("Collation", MYSQL_TYPE_VAR_STRING, 4),
        ("Cardinality", MYSQL_TYPE_LONGLONG, 21),
        ("Sub_part", MYSQL_TYPE_LONGLONG, 21),
        ("Packed", MYSQL_TYPE_VAR_STRING, 40),
        ("Null", MYSQL_TYPE_VAR_STRING, 12),
        ("Index_type", MYSQL_TYPE_VAR_STRING, 44),
        ("Comment", MYSQL_TYPE_VAR_STRING, 32),
        ("Index_comment", MYSQL_TYPE_VAR_STRING, 1024),
        ("Visible", MYSQL_TYPE_VAR_STRING, 12),
        ("Expression", MYSQL_TYPE_BLOB, abs_expression_length()),
    ]
    .into_iter()
    .map(|(name, column_type, column_length)| {
        let mut column = ColumnDefinitionConfig::new(name, column_type);
        column.character_set = if column_type == MYSQL_TYPE_LONGLONG {
            MYSQL_BINARY_COLLATION
        } else {
            u16::from(DEFAULT_UTF8MB4_COLLATION)
        };
        column.column_length = column_length;
        column
    })
    .collect()
}

#[cfg(unix)]
const fn abs_expression_length() -> u32 {
    MAX_TEXT_ROW_VALUE_LENGTH as u32
}

#[cfg(unix)]
fn show_create_table_error_kind(error: MySqlShowCreateTableError) -> FrontendErrorKind {
    match error {
        MySqlShowCreateTableError::MissingTable => FrontendErrorKind::MissingObject,
        MySqlShowCreateTableError::NotTable => FrontendErrorKind::NotView,
        MySqlShowCreateTableError::Unsupported => FrontendErrorKind::Unsupported,
        MySqlShowCreateTableError::Engine(error) => frontend_error_kind(error),
    }
}

#[cfg(unix)]
fn show_create_table_result_to_execution_result(
    result: MySqlShowCreateTableResult,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if result.table().len() > MAX_TEXT_ROW_VALUE_LENGTH
        || result.create_statement().len() > MAX_TEXT_ROW_VALUE_LENGTH
    {
        return Err(FrontendErrorKind::Internal);
    }
    let rows = vec![vec![
        Some(result.table().as_bytes().to_vec()),
        Some(result.create_statement().as_bytes().to_vec()),
    ]];
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_create_table_columns(result.create_statement().len()),
        rows,
        warnings: 0,
        status_flags,
    }))
}

/// MySQL sizes the `Create Table` column from the statement it is about:
/// `max(1024, byte length) * 4`, the 4 being utf8mb4's widest character.
#[cfg(unix)]
fn show_create_table_columns(statement_length: usize) -> Vec<ColumnDefinitionConfig> {
    let statement_width = u32::try_from(statement_length.max(1024) * 4).unwrap_or(u32::MAX);
    [("Table", 256u32), ("Create Table", statement_width)]
        .into_iter()
        .map(|(name, column_length)| {
            let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
            column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            column.column_length = column_length;
            column.decimals = 31;
            column.flags = MYSQL_NOT_NULL_FLAG;
            column
        })
        .collect()
}

#[cfg(unix)]
fn show_columns_result_to_execution_result(
    columns: Vec<MySqlColumnMetadata>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if columns.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(columns.len());
    for column in columns {
        if column.name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.extra().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let row = vec![
            Some(column.name().as_bytes().to_vec()),
            Some(show_column_type_name(&column)?),
            Some(if column.nullable() {
                b"YES".to_vec()
            } else {
                b"NO".to_vec()
            }),
            Some(match column.key() {
                MySqlColumnKey::None => Vec::new(),
                MySqlColumnKey::Multiple => b"MUL".to_vec(),
                MySqlColumnKey::Unique => b"UNI".to_vec(),
                MySqlColumnKey::Primary => b"PRI".to_vec(),
            }),
            show_column_default_value(column.default_value())?,
            Some(show_column_extra(column.extra())?.to_vec()),
        ];
        checked_text_result_row_payload_len(&row)?;

        let row_bytes = row
            .iter()
            .flatten()
            .map(Vec::len)
            .try_fold(0usize, usize::checked_add)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<Option<Vec<u8>>>>()))
            .and_then(|bytes| {
                std::mem::size_of::<Option<Vec<u8>>>()
                    .checked_mul(row.len())
                    .and_then(|row_storage| bytes.checked_add(row_storage))
            })
            .ok_or(FrontendErrorKind::Internal)?;
        retained_bytes = retained_bytes
            .checked_add(row_bytes)
            .ok_or(FrontendErrorKind::Internal)?;
        if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
            return Err(FrontendErrorKind::Internal);
        }
        rows.push(row);
    }

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_columns_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

#[cfg(unix)]
fn show_columns_columns() -> Vec<ColumnDefinitionConfig> {
    [
        ("Field", 64),
        ("Type", MAX_TEXT_ROW_VALUE_LENGTH as u32),
        ("Null", 3),
        ("Key", 3),
        ("Default", MAX_TEXT_ROW_VALUE_LENGTH as u32),
        ("Extra", 40),
    ]
    .into_iter()
    .map(|(name, column_length)| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = column_length;
        column
    })
    .collect()
}

#[cfg(unix)]
fn checked_text_result_row_payload_len(
    values: &[Option<Vec<u8>>],
) -> Result<usize, FrontendErrorKind> {
    let payload_len = values.iter().try_fold(0usize, |payload_len, value| {
        let value_len = match value {
            None => 1,
            Some(bytes) => {
                length_encoded_value_len(bytes.len()).map_err(|_| FrontendErrorKind::Internal)?
            }
        };
        payload_len
            .checked_add(value_len)
            .ok_or(FrontendErrorKind::Internal)
    })?;
    if payload_len > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
        return Err(FrontendErrorKind::Internal);
    }
    Ok(payload_len)
}

#[cfg(unix)]
/// Renders the type the way MySQL 8.4.11 reports it here, lower case and
/// carrying the declared length where the type has one.
fn show_column_type_name(column: &MySqlColumnMetadata) -> Result<Vec<u8>, FrontendErrorKind> {
    if let Some((precision, scale)) = column.decimal_size() {
        return match column.type_name() {
            "DECIMAL" => Ok(format!("decimal({precision},{scale})").into_bytes()),
            _ => Err(FrontendErrorKind::Internal),
        };
    }
    if let Some(length) = column.character_length() {
        return match column.type_name() {
            "VARCHAR" => Ok(format!("varchar({length})").into_bytes()),
            "CHAR" => Ok(format!("char({length})").into_bytes()),
            _ => Err(FrontendErrorKind::Internal),
        };
    }
    let name: &[u8] = match column.type_name() {
        "TINYINT" => b"tinyint",
        "SMALLINT" => b"smallint",
        "MEDIUMINT" => b"mediumint",
        "INT" | "INTEGER" => b"int",
        "BIGINT" => b"bigint",
        "TEXT" => b"text",
        "BLOB" => b"blob",
        "DOUBLE" => b"double",
        "BOOLEAN" => b"tinyint(1)",
        "DATETIME" => b"datetime",
        "TIMESTAMP" => b"timestamp",
        _ => return Err(FrontendErrorKind::Internal),
    };
    Ok(name.to_vec())
}

#[cfg(unix)]
fn show_column_extra(extra: &str) -> Result<&'static [u8], FrontendErrorKind> {
    match extra {
        "" => Ok(b""),
        "AUTO_INCREMENT" => Ok(b"auto_increment"),
        _ => Err(FrontendErrorKind::Internal),
    }
}

#[cfg(unix)]
fn show_column_default_value(
    default_value: Option<&MySqlColumnDefault>,
) -> Result<Option<Vec<u8>>, FrontendErrorKind> {
    let Some(default_value) = default_value else {
        return Ok(None);
    };
    let value = match default_value {
        MySqlColumnDefault::Null => return Ok(None),
        MySqlColumnDefault::Integer { value, .. } => {
            let value = value.to_string();
            if value.len() > MAX_TEXT_ROW_VALUE_LENGTH {
                return Err(FrontendErrorKind::Internal);
            }
            return Ok(Some(value.into_bytes()));
        }
        MySqlColumnDefault::Text(text) => text.as_bytes(),
        MySqlColumnDefault::Boolean(value) => {
            return Ok(Some(if *value { b"1".to_vec() } else { b"0".to_vec() }));
        }
    };
    if value.len() > MAX_TEXT_ROW_VALUE_LENGTH {
        return Err(FrontendErrorKind::Internal);
    }
    Ok(Some(value.to_vec()))
}

#[cfg(unix)]
fn show_tables_column(database: &str) -> ColumnDefinitionConfig {
    let mut column =
        ColumnDefinitionConfig::new(format!("Tables_in_{database}"), MYSQL_TYPE_VAR_STRING);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 256;
    column
}

#[cfg(unix)]
fn column_metadata_error_kind(error: MySqlColumnMetadataError) -> FrontendErrorKind {
    match error {
        MySqlColumnMetadataError::TableNotFound => FrontendErrorKind::MissingObject,
        MySqlColumnMetadataError::UnsupportedDefinition => FrontendErrorKind::Unsupported,
        MySqlColumnMetadataError::CorruptDefinition | MySqlColumnMetadataError::Engine(_) => {
            FrontendErrorKind::Internal
        }
    }
}

#[cfg(unix)]
fn database_error_kind(error: MySqlDatabaseError) -> FrontendErrorKind {
    match error {
        MySqlDatabaseError::InvalidDatabaseName | MySqlDatabaseError::DatabaseNotFound(_) => {
            FrontendErrorKind::UnknownDatabase
        }
        MySqlDatabaseError::DatabaseAlreadyExists(_) => FrontendErrorKind::DuplicateDatabase,
        MySqlDatabaseError::DatabaseBusy(_) => FrontendErrorKind::DatabaseBusy,
        MySqlDatabaseError::NoDatabaseSelected => FrontendErrorKind::NoDatabaseSelected,
        MySqlDatabaseError::DatabaseNotReady(_)
        | MySqlDatabaseError::DatabaseIntegrity
        | MySqlDatabaseError::CatalogUnavailable
        | MySqlDatabaseError::ConnectionUnavailable => FrontendErrorKind::Internal,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use super::*;
    #[cfg(unix)]
    use crate::AccountId;
    use crate::{
        dispatch_command_frame, AuthenticationResponse, ClassicConnection,
        ClientHandshakeResponseConfig, ConnectionState, InitialAuthenticationResult,
        InitialHandshakeSettings, PacketCodec, TextRowValue, TransportSecurity,
        CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_CONNECT_WITH_DB, CLIENT_FOUND_ROWS,
        COMMAND_SEQUENCE_ID, COM_INIT_DB, COM_QUERY, DEFAULT_UTF8MB4_COLLATION,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use turso_core::{
        storage::database::DatabaseFile, Database, DatabaseOpts, MemoryIO, OpenFlags, OpenOptions,
        IO,
    };
    #[cfg(unix)]
    use turso_mysql::{MySqlDatabaseCatalog, MySqlPreparedStatementAuthority};
    use turso_mysql::{
        schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext},
        MySqlDialect,
    };
    #[cfg(unix)]
    use turso_mysql_parser::MySqlTableName;

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

    #[test]
    fn varchar_columns_answer_what_mysql_8_4_answers() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([9; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        adapter
            .execute_query(
                "CREATE TABLE v (id INT NOT NULL PRIMARY KEY, name VARCHAR(4) NOT NULL, note VARCHAR(10), tag CHAR(2), ratio DOUBLE, live BOOLEAN, seen DATETIME)",
            )
            .unwrap();

        // Measured on MySQL 8.4.11: the length counts characters, so four
        // multi-byte characters fit a VARCHAR(4) and five do not.
        adapter
            .execute_query("INSERT INTO v (id, name) VALUES (1, 'abcd')")
            .unwrap();
        adapter
            .execute_query("INSERT INTO v (id, name) VALUES (3, 'あいうえ')")
            .unwrap();
        for sql in [
            "INSERT INTO v (id, name) VALUES (2, 'abcde')",
            "INSERT INTO v (id, name) VALUES (4, 'あいうえお')",
        ] {
            assert_eq!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::DataTooLong),
                "{sql}"
            );
        }

        // A CHAR column is held to its length the same way.
        assert_eq!(
            adapter.execute_query("INSERT INTO v (id, name, tag) VALUES (5, 'ab', 'xyz')"),
            Err(FrontendErrorKind::DataTooLong)
        );

        // A DOUBLE keeps its value: MySQL's DOUBLE and the engine's REAL are
        // both IEEE 754 binary64. A fractional value that meets an integer
        // column is refused with 1366 instead; MySQL rounds it away from zero,
        // measured, which a validator cannot do after the record is built.
        adapter
            .execute_query("INSERT INTO v (id, name, ratio) VALUES (6, 'x', 1.5)")
            .unwrap();
        assert_eq!(
            adapter.execute_query("INSERT INTO v (id, name, ratio) VALUES (1.5, 'y', 2.5)"),
            Err(FrontendErrorKind::IncorrectValue)
        );
        let CommandExecutionResult::ResultSet(ratio) = adapter
            .execute_query("SELECT ratio FROM v WHERE id = 6")
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(ratio.rows[0][0].clone().unwrap()).unwrap(),
            "1.5"
        );

        // A BOOLEAN is a TINYINT, so it takes a TINYINT's range and refuses a
        // value outside it.
        adapter
            .execute_query("INSERT INTO v (id, name, live) VALUES (7, 'z', 1)")
            .unwrap();
        assert!(adapter
            .execute_query("INSERT INTO v (id, name, live) VALUES (8, 'w', 999)")
            .is_err());

        // A DATETIME keeps the text it was given, and the calendar is checked:
        // measured on MySQL 8.4.11, February the thirtieth is 1292 there too.
        adapter
            .execute_query("INSERT INTO v (id, name, seen) VALUES (9, 'q', '2026-09-06 01:02:03')")
            .unwrap();
        for sql in [
            "INSERT INTO v (id, name, seen) VALUES (10, 'r', '2026-02-30 00:00:00')",
            "INSERT INTO v (id, name, seen) VALUES (11, 's', 'not a date')",
            "INSERT INTO v (id, name, seen) VALUES (12, 't', '2026-9-6 1:2:3')",
        ] {
            assert_eq!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::IncorrectTemporalValue),
                "{sql}"
            );
        }
        let CommandExecutionResult::ResultSet(seen) = adapter
            .execute_query("SELECT seen FROM v WHERE id = 9")
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(seen.rows[0][0].clone().unwrap()).unwrap(),
            "2026-09-06 01:02:03"
        );

        let CommandExecutionResult::ResultSet(created) =
            adapter.execute_query("SHOW CREATE TABLE v").unwrap()
        else {
            panic!("SHOW CREATE TABLE must return a result set");
        };
        assert_eq!(
            String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
            concat!(
                "CREATE TABLE `v` (\n",
                "  `id` int NOT NULL,\n",
                "  `name` varchar(4) NOT NULL,\n",
                "  `note` varchar(10) DEFAULT NULL,\n",
                "  `tag` char(2) DEFAULT NULL,\n",
                "  `ratio` double DEFAULT NULL,\n",
                "  `live` tinyint(1) DEFAULT NULL,\n",
                "  `seen` datetime DEFAULT NULL,\n",
                "  PRIMARY KEY (`id`)\n",
                ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            )
        );

        let CommandExecutionResult::ResultSet(columns) =
            adapter.execute_query("SHOW COLUMNS FROM v").unwrap()
        else {
            panic!("SHOW COLUMNS must return a result set");
        };
        assert_eq!(
            columns
                .rows
                .iter()
                .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            vec![
                "int",
                "varchar(4)",
                "varchar(10)",
                "char(2)",
                "double",
                "tinyint(1)",
                "datetime"
            ]
        );

        let CommandExecutionResult::ResultSet(selected) = adapter
            .execute_query("SELECT id, name, note, tag, ratio, live, seen FROM v")
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        // Measured: MySQL reports the declared character count times four, the
        // bytes utf8mb4 reserves for one character.
        assert_eq!(
            selected
                .columns
                .iter()
                .map(|column| (
                    column.column_type,
                    column.column_length,
                    column.character_set
                ))
                .collect::<Vec<_>>(),
            vec![
                (MYSQL_TYPE_LONG, 11, MYSQL_BINARY_COLLATION),
                (
                    MYSQL_TYPE_VAR_STRING,
                    16,
                    u16::from(DEFAULT_UTF8MB4_COLLATION)
                ),
                (
                    MYSQL_TYPE_VAR_STRING,
                    40,
                    u16::from(DEFAULT_UTF8MB4_COLLATION)
                ),
                // Measured: a CHAR column reports 254 and carries the same
                // text collation and length rule as a VARCHAR one.
                (MYSQL_TYPE_STRING, 8, u16::from(DEFAULT_UTF8MB4_COLLATION)),
                (MYSQL_TYPE_DOUBLE, 22, MYSQL_BINARY_COLLATION),
                // Measured: a BOOLEAN reports the TINYINT type with the display
                // width from `tinyint(1)`, where a plain TINYINT reports 4.
                (MYSQL_TYPE_TINY, 1, MYSQL_BINARY_COLLATION),
                // Measured: a DATETIME reports the width of its text form and
                // the binary flag, because it carries no collation.
                (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_COLLATION),
            ]
        );
        assert_eq!(
            selected.columns[6].flags & MYSQL_BINARY_FLAG,
            MYSQL_BINARY_FLAG
        );
        // Measured: a DOUBLE column reports 31 decimals, meaning not fixed.
        assert_eq!(selected.columns[4].decimals, NOT_FIXED_DECIMALS);
        assert_eq!(
            selected
                .rows
                .iter()
                .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            vec!["abcd", "あいうえ", "x", "z", "q"]
        );
    }

    #[test]
    fn secondary_indexes_reach_the_catalog_the_way_mysql_8_4_reports_them() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([12; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        for sql in [
            "CREATE TABLE k (id INT NOT NULL PRIMARY KEY, a VARCHAR(8), b VARCHAR(8))",
            "CREATE INDEX idx_a ON k (a)",
            "CREATE INDEX idx_ab ON k (a, b)",
            "CREATE UNIQUE INDEX uq_b ON k (b)",
        ] {
            adapter.execute_query(sql).unwrap_or_else(|error| {
                panic!("{sql}: {error:?}");
            });
        }

        // Byte for byte what MySQL 8.4.11 prints for the same table: the
        // primary key, then the unique keys, then the plain ones, each group in
        // creation order, and a multi-column key with no space after the comma.
        let CommandExecutionResult::ResultSet(created) =
            adapter.execute_query("SHOW CREATE TABLE k").unwrap()
        else {
            panic!("SHOW CREATE TABLE must return a result set");
        };
        assert_eq!(
            String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
            concat!(
                "CREATE TABLE `k` (\n",
                "  `id` int NOT NULL,\n",
                "  `a` varchar(8) DEFAULT NULL,\n",
                "  `b` varchar(8) DEFAULT NULL,\n",
                "  PRIMARY KEY (`id`),\n",
                "  UNIQUE KEY `uq_b` (`b`),\n",
                "  KEY `idx_a` (`a`),\n",
                "  KEY `idx_ab` (`a`,`b`)\n",
                ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            )
        );

        // Measured: only a leading column carries a key, `UNI` when a
        // single-column unique index makes that column unique and `MUL`
        // otherwise. `b` leads `uq_b` and also sits second in `idx_ab`, and
        // MySQL reports the stronger of the two.
        let CommandExecutionResult::ResultSet(columns) =
            adapter.execute_query("SHOW COLUMNS FROM k").unwrap()
        else {
            panic!("SHOW COLUMNS must return a result set");
        };
        assert_eq!(
            columns
                .rows
                .iter()
                .map(|row| (
                    String::from_utf8(row[0].clone().unwrap()).unwrap(),
                    String::from_utf8(row[3].clone().unwrap()).unwrap(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("id".to_owned(), "PRI".to_owned()),
                ("a".to_owned(), "MUL".to_owned()),
                ("b".to_owned(), "UNI".to_owned()),
            ]
        );
    }

    #[test]
    fn an_inline_key_creates_its_index_or_no_table_at_all() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([13; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        adapter
            .execute_query(
                "CREATE TABLE k (id INT NOT NULL PRIMARY KEY, a VARCHAR(8), b VARCHAR(8), KEY idx_a (a), KEY idx_ab (a, b))",
            )
            .unwrap();

        let CommandExecutionResult::ResultSet(created) =
            adapter.execute_query("SHOW CREATE TABLE k").unwrap()
        else {
            panic!("SHOW CREATE TABLE must return a result set");
        };
        assert_eq!(
            String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
            concat!(
                "CREATE TABLE `k` (\n",
                "  `id` int NOT NULL,\n",
                "  `a` varchar(8) DEFAULT NULL,\n",
                "  `b` varchar(8) DEFAULT NULL,\n",
                "  PRIMARY KEY (`id`),\n",
                "  KEY `idx_a` (`a`),\n",
                "  KEY `idx_ab` (`a`,`b`)\n",
                ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            )
        );

        // `b` only follows `a` in idx_ab, so it carries no key, which is what
        // MySQL 8.4.11 reports for a column that leads nothing.
        let CommandExecutionResult::ResultSet(columns) =
            adapter.execute_query("SHOW COLUMNS FROM k").unwrap()
        else {
            panic!("SHOW COLUMNS must return a result set");
        };
        assert_eq!(
            columns
                .rows
                .iter()
                .map(|row| String::from_utf8(row[3].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            vec!["PRI".to_owned(), "MUL".to_owned(), String::new()]
        );

        // The statement applies whole or not at all: a key naming a column the
        // table does not have leaves no table behind.
        assert!(adapter
            .execute_query("CREATE TABLE bad (id INT NOT NULL PRIMARY KEY, KEY idx_z (zz))")
            .is_err());
        let CommandExecutionResult::ResultSet(tables) =
            adapter.execute_query("SHOW TABLES").unwrap()
        else {
            panic!("SHOW TABLES must return a result set");
        };
        assert!(
            !tables.rows.iter().any(|row| row[0]
                .as_ref()
                .is_some_and(|name| name.as_slice() == b"bad")),
            "the failed CREATE TABLE left a table behind"
        );
    }

    #[test]
    fn an_update_or_delete_can_name_the_rows_it_touches() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([14; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        adapter
            .execute_query("CREATE TABLE u (id INT NOT NULL PRIMARY KEY, name TEXT, n INT)")
            .unwrap();
        adapter
            .execute_query("INSERT INTO u (id, name, n) VALUES (1, 'a', 10), (2, 'b', 20)")
            .unwrap();

        let affected = |adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
                        sql: &str| match adapter.execute_query(sql) {
            Ok(CommandExecutionResult::Ok(result)) => result.affected_rows,
            other => panic!("{sql}: {other:?}"),
        };
        assert_eq!(
            affected(&mut adapter, "UPDATE u SET name = 'z' WHERE id = 1"),
            1
        );
        assert_eq!(affected(&mut adapter, "UPDATE u SET n = 5 WHERE n > 15"), 1);
        assert_eq!(affected(&mut adapter, "DELETE FROM u WHERE id = 2"), 1);

        let CommandExecutionResult::ResultSet(left) =
            adapter.execute_query("SELECT id, name FROM u").unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            left.rows
                .iter()
                .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            vec!["z".to_owned()]
        );

        // A text comparison runs here for the same reason it runs in a SELECT,
        // and ignores case the same way — see the collation note in COMPAT.md.
        let CommandExecutionResult::Ok(deleted) = adapter
            .execute_query("DELETE FROM u WHERE name = 'Z'")
            .unwrap()
        else {
            panic!("DELETE must report affected rows");
        };
        assert_eq!(deleted.affected_rows, 1);

        // A string still cannot meet an integer column, which MySQL answers by
        // coercing the string.
        assert_eq!(
            adapter.execute_query("DELETE FROM u WHERE id = 'z'"),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[test]
    fn count_answers_what_mysql_8_4_answers() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([15; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        adapter
            .execute_query("CREATE TABLE c (id INT NOT NULL PRIMARY KEY, n INT)")
            .unwrap();
        adapter
            .execute_query("INSERT INTO c (id, n) VALUES (1, 10), (2, NULL), (3, 30)")
            .unwrap();

        // Measured on MySQL 8.4.11: LONGLONG, binary collation, length 21,
        // NOT_NULL and BINARY set, no decimals — whatever is counted. The column
        // is named after the call as written, case included and unquoted, and an
        // alias replaces that name. `COUNT(col)` skips NULLs.
        for (sql, name, count) in [
            ("SELECT COUNT(*) FROM c", "COUNT(*)", "3"),
            ("SELECT COUNT(n) FROM c", "COUNT(n)", "2"),
            ("SELECT count(*) FROM c", "count(*)", "3"),
            ("SELECT COUNT(*) AS total FROM c", "total", "3"),
            ("SELECT COUNT(*) FROM c WHERE id = 1", "COUNT(*)", "1"),
        ] {
            let CommandExecutionResult::ResultSet(result) =
                adapter.execute_query(sql).unwrap_or_else(|error| {
                    panic!("{sql}: {error:?}");
                })
            else {
                panic!("{sql} must return a result set");
            };
            assert_eq!(result.columns[0].name, name, "{sql}");
            assert_eq!(
                (
                    result.columns[0].column_type,
                    result.columns[0].column_length,
                    result.columns[0].flags,
                    result.columns[0].decimals,
                ),
                (MYSQL_TYPE_LONGLONG, 21, 129, 0),
                "{sql}"
            );
            assert_eq!(
                String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap(),
                count,
                "{sql}"
            );
        }

        // Refused: DISTINCT has its own meaning, and SUM and AVG answer DECIMAL,
        // which this frontend does not have.
        for sql in [
            "SELECT COUNT(DISTINCT n) FROM c",
            "SELECT SUM(n) FROM c",
            "SELECT AVG(n) FROM c",
            "SELECT MIN(n) FROM c",
        ] {
            assert!(adapter.execute_query(sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn decimal_columns_report_what_mysql_8_4_reports() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([17; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        adapter
            .execute_query(
                "CREATE TABLE d (id INT NOT NULL PRIMARY KEY, a DECIMAL(10,2), b DECIMAL(5,0), c DECIMAL)",
            )
            .unwrap();
        adapter
            .execute_query("INSERT INTO d (id, a, b, c) VALUES (1, 1.5, 7, 9)")
            .unwrap();

        // A bare DECIMAL means DECIMAL(10,0), which is what MySQL prints for it.
        let CommandExecutionResult::ResultSet(created) =
            adapter.execute_query("SHOW CREATE TABLE d").unwrap()
        else {
            panic!("SHOW CREATE TABLE must return a result set");
        };
        assert_eq!(
            String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
            concat!(
                "CREATE TABLE `d` (\n",
                "  `id` int NOT NULL,\n",
                "  `a` decimal(10,2) DEFAULT NULL,\n",
                "  `b` decimal(5,0) DEFAULT NULL,\n",
                "  `c` decimal(10,0) DEFAULT NULL,\n",
                "  PRIMARY KEY (`id`)\n",
                ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            )
        );

        let CommandExecutionResult::ResultSet(columns) =
            adapter.execute_query("SHOW COLUMNS FROM d").unwrap()
        else {
            panic!("SHOW COLUMNS must return a result set");
        };
        assert_eq!(
            columns
                .rows
                .iter()
                .skip(1)
                .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            vec!["decimal(10,2)", "decimal(5,0)", "decimal(10,0)"]
        );

        // Measured on MySQL 8.4.11: NEWDECIMAL, and a length of the precision
        // plus one for the sign plus one more for the point when the scale is
        // above zero.
        let CommandExecutionResult::ResultSet(selected) =
            adapter.execute_query("SELECT a, b, c FROM d").unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            selected
                .columns
                .iter()
                .map(|column| (column.column_type, column.column_length, column.decimals))
                .collect::<Vec<_>>(),
            vec![
                (MYSQL_TYPE_NEWDECIMAL, 12, 2),
                (MYSQL_TYPE_NEWDECIMAL, 6, 0),
                (MYSQL_TYPE_NEWDECIMAL, 11, 0),
            ]
        );
        // The value is a binary64, not the exact decimal MySQL keeps, so it is
        // rendered as it is rather than padded to the declared scale: MySQL
        // answers `1.50` here.
        assert_eq!(
            String::from_utf8(selected.rows[0][0].clone().unwrap()).unwrap(),
            "1.5"
        );
    }

    #[test]
    fn timestamp_reads_back_the_moment_it_was_given() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([18; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        adapter
            .execute_query(
                "CREATE TABLE ts (id INT NOT NULL PRIMARY KEY, dt DATETIME, t TIMESTAMP NULL)",
            )
            .unwrap();
        adapter
            .execute_query(
                "INSERT INTO ts (id, dt, t) VALUES (1, '2026-09-06 01:02:03', '2026-09-06 01:02:03')",
            )
            .unwrap();
        // The calendar check is the same one a DATETIME gets.
        assert_eq!(
            adapter.execute_query("INSERT INTO ts (id, t) VALUES (2, '2026-02-30 00:00:00')"),
            Err(FrontendErrorKind::IncorrectTemporalValue)
        );

        // Measured on MySQL 8.4.11: a nullable TIMESTAMP prints its NULL where a
        // nullable DATETIME prints only the DEFAULT.
        let CommandExecutionResult::ResultSet(created) =
            adapter.execute_query("SHOW CREATE TABLE ts").unwrap()
        else {
            panic!("SHOW CREATE TABLE must return a result set");
        };
        assert_eq!(
            String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
            concat!(
                "CREATE TABLE `ts` (\n",
                "  `id` int NOT NULL,\n",
                "  `dt` datetime DEFAULT NULL,\n",
                "  `t` timestamp NULL DEFAULT NULL,\n",
                "  PRIMARY KEY (`id`)\n",
                ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            )
        );

        let CommandExecutionResult::ResultSet(columns) =
            adapter.execute_query("SHOW COLUMNS FROM ts").unwrap()
        else {
            panic!("SHOW COLUMNS must return a result set");
        };
        assert_eq!(
            columns
                .rows
                .iter()
                .skip(1)
                .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            vec!["datetime", "timestamp"]
        );

        // Measured: TIMESTAMP reports type 7 where DATETIME reports 12, both
        // with the width of the text form and the binary flag.
        let CommandExecutionResult::ResultSet(selected) =
            adapter.execute_query("SELECT dt, t FROM ts").unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            selected
                .columns
                .iter()
                .map(|column| (column.column_type, column.column_length))
                .collect::<Vec<_>>(),
            vec![(MYSQL_TYPE_DATETIME, 19), (MYSQL_TYPE_TIMESTAMP, 19)]
        );
        assert_eq!(
            String::from_utf8(selected.rows[0][1].clone().unwrap()).unwrap(),
            "2026-09-06 01:02:03"
        );
    }

    /// MySQL's default collation ignores both case and accents. A comparison
    /// asks the engine for NOCASE and a LIKE needs nothing, and both reproduce
    /// the case half and not the accent half; measured on 8.4.11.
    #[cfg(unix)]
    #[test]
    fn a_text_where_ignores_case_but_not_accents() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([19; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
        adapter
            .execute_query("CREATE TABLE people (id INT NOT NULL PRIMARY KEY, name VARCHAR(32))")
            .unwrap();
        adapter
            .execute_query(
                "INSERT INTO people (id, name) VALUES (1, 'abc'), (2, 'ABC'), (3, 'Abc'), (4, 'B'), (5, 'cafe'), (6, 'caf\u{e9}')",
            )
            .unwrap();

        let mut ids = |sql: &str| {
            let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap()
            else {
                panic!("SELECT must return a result set");
            };
            result
                .rows
                .iter()
                .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
                .collect::<Vec<_>>()
        };

        // Measured: MySQL answers 1, 2 and 3 here, where byte order answers 2.
        assert_eq!(
            ids("SELECT id FROM people WHERE name = 'ABC'"),
            ["1", "2", "3"]
        );
        // Ordering goes through the same collation as equality. Measured:
        // 'B' > 'a' is true in MySQL and false byte for byte, so byte order
        // would answer 1 alone where MySQL and NOCASE answer all four.
        assert_eq!(
            ids("SELECT id FROM people WHERE name > 'a' AND name < 'ca'"),
            ["1", "2", "3", "4"]
        );
        // Measured: MySQL answers 5 and 6, because its collation ignores the
        // accent too. NOCASE does not, so this answers 5 alone.
        assert_eq!(ids("SELECT id FROM people WHERE name = 'cafe'"), ["5"]);
        // LIKE needs no collation of its own: the engine already matches it
        // without regard to ASCII case, which is what MySQL's default
        // collation does.
        assert_eq!(
            ids("SELECT id FROM people WHERE name LIKE 'A%'"),
            ["1", "2", "3"]
        );
        assert_eq!(
            ids("SELECT id FROM people WHERE name NOT LIKE '%c'"),
            ["4", "5", "6"]
        );
        assert_eq!(
            ids("SELECT id FROM people WHERE name LIKE '_bc'"),
            ["1", "2", "3"]
        );

        // An UPDATE and a DELETE go through the same renderer and the same
        // rule, so the rows a WHERE names cannot depend on the statement.
        assert_eq!(ids("SELECT id FROM people WHERE name = 'b'"), ["4"]);
        adapter
            .execute_query("UPDATE people SET name = 'done' WHERE name = 'b'")
            .unwrap();
        adapter
            .execute_query("DELETE FROM people WHERE name = 'ABC'")
            .unwrap();
        let CommandExecutionResult::ResultSet(left) = adapter
            .execute_query("SELECT id FROM people WHERE name = 'DONE'")
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(left.rows, vec![vec![Some(b"4".to_vec())]]);

        // A string still cannot meet an integer column, which MySQL answers by
        // coercing the string.
        assert_eq!(
            adapter.execute_query("SELECT id FROM people WHERE id = 'abc'"),
            Err(FrontendErrorKind::Unsupported)
        );
        // A backslash means an escape in MySQL and a literal byte in the
        // engine, so a pattern carrying one is refused rather than mismatched.
        assert_eq!(
            adapter.execute_query("SELECT id FROM people WHERE name LIKE 'a\\%'"),
            Err(FrontendErrorKind::Syntax)
        );
    }

    fn adapter() -> MySqlCommandAdapter {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        static NEXT_DATABASE: AtomicUsize = AtomicUsize::new(0);
        let path = format!(
            "mysql-server-frontend-adapter-{}.db",
            NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
        );
        let file = io.open_file(&path, OpenFlags::Create, true).unwrap();
        let database = Database::open(
            io,
            &path,
            OpenOptions::new(Arc::new(MySqlDialect))
                .storage(Arc::new(DatabaseFile::new(file)))
                .flags(OpenFlags::Create)
                .db_opts(DatabaseOpts::new().with_vacuum(true).with_views(true)),
        )
        .unwrap();
        let inner = database.connect().unwrap();
        let frontend = MySqlConnection::new(inner.clone(), binary_context()).unwrap();
        frontend
            .execute("CREATE TABLE `result_values` (`id` INTEGER, `payload` BLOB)")
            .unwrap();
        inner
            .execute("INSERT INTO result_values VALUES (1, X'00ff'), (2, NULL)")
            .unwrap();
        frontend
            .execute("CREATE TABLE `many_rows` (`id` INTEGER)")
            .unwrap();
        frontend
            .execute("CREATE TABLE `wide_values` (`left_value` BLOB, `right_value` BLOB)")
            .unwrap();
        inner
            .execute(
                "WITH RECURSIVE ids(id) AS (VALUES(1) UNION ALL SELECT id + 1 FROM ids WHERE id <= 4096) INSERT INTO many_rows SELECT id FROM ids",
            )
            .unwrap();
        inner
            .execute("INSERT INTO wide_values VALUES (zeroblob(2048), zeroblob(2048))")
            .unwrap();
        MySqlCommandAdapter::new(frontend)
    }

    #[cfg(unix)]
    fn auto_increment_adapter() -> (tempfile::TempDir, MySqlCommandAdapter) {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        catalog.create("reset").unwrap();
        let mut session = catalog.new_session(binary_context());
        session.select_database("reset").unwrap();
        let connection = session.connection().unwrap().clone();
        connection
            .execute(
                "CREATE TABLE generated_records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
            )
            .unwrap();
        drop(session);
        drop(catalog);
        (directory, MySqlCommandAdapter::new(connection))
    }

    #[test]
    fn bootstrap_settings_round_positive_idle_durations_up_to_seconds() {
        assert_eq!(
            MySqlBootstrapSettings::new(4096, Duration::from_secs(7)).wait_timeout_seconds(),
            7
        );
        assert_eq!(
            MySqlBootstrapSettings::new(4096, Duration::from_millis(500)).wait_timeout_seconds(),
            1
        );
        assert_eq!(
            MySqlBootstrapSettings::new(4096, Duration::from_millis(1500)).wait_timeout_seconds(),
            2
        );
    }

    #[test]
    fn direct_adapter_serves_the_typed_driver_bootstrap_result() {
        let mut adapter = adapter()
            .with_bootstrap_settings(MAX_COMMAND_PAYLOAD_LENGTH, Duration::from_millis(1500));
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT @@max_allowed_packet,@@wait_timeout")
            .unwrap()
        else {
            panic!("driver bootstrap query must produce a result set");
        };

        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (column.name.as_str(), column.column_type))
                .collect::<Vec<_>>(),
            vec![
                ("@@max_allowed_packet", MYSQL_TYPE_LONGLONG),
                ("@@wait_timeout", MYSQL_TYPE_LONGLONG),
            ]
        );
        assert_eq!(
            result.rows,
            vec![vec![
                Some(MAX_COMMAND_PAYLOAD_LENGTH.to_string().into_bytes()),
                Some(b"2".to_vec()),
            ]]
        );
        assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
    }

    #[test]
    fn unknown_system_variables_do_not_enter_the_bootstrap_path() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.execute_query("SELECT @@socket,@@wait_timeout"),
            Err(FrontendErrorKind::Unsupported)
        );
        assert!(matches!(
            adapter.execute_query("SELECT '@@socket'"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
    }

    #[test]
    fn direct_adapter_orders_and_limits_text_and_prepared_results() {
        let mut adapter = adapter();
        adapter
            .execute_query("CREATE TABLE records (id INT, label TEXT)")
            .unwrap();
        adapter
            .execute_query(
                "INSERT INTO records (id, label) VALUES (3, 'b'), (1, 'A'), (2, 'a'), (4, NULL)",
            )
            .unwrap();
        for sql in [
            "SELECT id AS ranked, label FROM records ORDER BY label ASC, id DESC LIMIT 2 OFFSET 1",
            "SELECT id AS ranked, label FROM records ORDER BY label ASC, id DESC LIMIT 1, 2",
        ] {
            let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap()
            else {
                panic!("ordered SELECT must return rows");
            };
            assert_eq!(
                result.rows,
                vec![
                    vec![Some(b"1".to_vec()), Some(b"A".to_vec())],
                    vec![Some(b"2".to_vec()), Some(b"a".to_vec())]
                ]
            );
            assert_eq!(result.columns[0].name, "ranked");
            assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONG);
        }
        let prepared = adapter
            .execute_stmt_prepare(
                "SELECT id AS ranked, ? AS marker FROM records ORDER BY ranked DESC LIMIT 2",
            )
            .unwrap();
        assert_eq!(prepared.parameters.len(), 1);
        assert_eq!(prepared.columns[0].name, "ranked");
        assert_eq!(prepared.columns[0].column_type, MYSQL_TYPE_LONG);
        let result = adapter
            .execute_stmt_execute(
                prepared.statement_id,
                &[0, 1, MYSQL_TYPE_VAR_STRING, 0, 1, b'x'],
            )
            .unwrap();
        assert_eq!(
            prepared_result_set(result).rows,
            vec![
                vec![
                    BinaryResultValue::Integer(4),
                    BinaryResultValue::Text("x".into())
                ],
                vec![
                    BinaryResultValue::Integer(3),
                    BinaryResultValue::Text("x".into())
                ]
            ]
        );
    }

    #[test]
    fn direct_adapter_prepares_and_retains_checked_selects() {
        let mut adapter = adapter();

        let first = adapter.execute_stmt_prepare("SELECT ? AS value").unwrap();
        let second = adapter.execute_stmt_prepare("SELECT 1 AS one").unwrap();

        assert_eq!((first.statement_id, second.statement_id), (1, 2));
        assert_eq!(
            first
                .parameters
                .iter()
                .map(|parameter| parameter.name.as_str())
                .collect::<Vec<_>>(),
            ["?1"]
        );
        assert_eq!(first.columns.len(), 1);
        assert_eq!(first.columns[0].name, "value");
        // A marker starts generic, the way MySQL 8.4.11 answers a fresh
        // `SELECT ? AS value` before anything has been bound.
        assert_eq!(first.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
    }

    #[test]
    fn direct_adapter_maps_invalid_and_unsupported_prepares() {
        let mut adapter = adapter();

        assert_eq!(
            adapter.execute_stmt_prepare("SELECT FROM"),
            Err(FrontendErrorKind::Syntax)
        );
        let delete = adapter
            .execute_stmt_prepare("DELETE FROM result_values")
            .unwrap();
        assert!(delete.columns.is_empty());
    }

    #[test]
    fn direct_adapter_closes_and_resets_only_known_statements() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();

        assert_eq!(adapter.execute_stmt_reset(prepared.statement_id), Ok(()));
        adapter.execute_stmt_close(prepared.statement_id);
        assert_eq!(
            adapter.execute_stmt_reset(prepared.statement_id),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );
        adapter.execute_stmt_close(prepared.statement_id);
    }

    #[cfg(unix)]
    #[test]
    fn direct_adapter_reset_rolls_back_and_clears_session_state() {
        let (_directory, mut adapter) = auto_increment_adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        let prepared_result = adapter
            .execute_stmt_execute(
                prepared.statement_id,
                &[0, 1, MYSQL_TYPE_VAR_STRING, 0, 1, b'x'],
            )
            .unwrap();
        assert_eq!(
            prepared_result_set(prepared_result).rows,
            [vec![BinaryResultValue::Text("x".to_string())]]
        );
        adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"discarded");

        let CommandExecutionResult::Ok(disabled) =
            adapter.execute_query("SET SESSION autocommit = 0").unwrap()
        else {
            panic!("SET autocommit must produce an OK result");
        };
        assert_eq!(disabled.status_flags, 0);
        adapter
            .execute_query("INSERT INTO generated_records (label) VALUES ('discarded')")
            .unwrap();
        assert_eq!(adapter.status_flags(), SERVER_STATUS_IN_TRANS);
        assert_eq!(adapter.connection.last_insert_id(), 1);
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SELECT LAST_INSERT_ID()").unwrap()
        else {
            panic!("LAST_INSERT_ID must produce a result set");
        };
        assert_eq!(result.rows, [vec![Some(b"1".to_vec())]]);

        adapter.execute_reset_connection().unwrap();

        assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(adapter.connection.last_insert_id(), 0);
        assert!(adapter.prepared_types.is_empty());
        assert!(adapter.pending_long_data.values.is_empty());
        assert!(adapter.pending_long_data.errors.is_empty());
        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[0, 0, MYSQL_TYPE_VAR_STRING, 0]),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );

        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id, LAST_INSERT_ID() FROM generated_records")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert!(result.rows.is_empty());
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SELECT LAST_INSERT_ID()").unwrap()
        else {
            panic!("LAST_INSERT_ID must produce a result set");
        };
        assert_eq!(result.rows, [vec![Some(b"0".to_vec())]]);

        adapter
            .execute_query("INSERT INTO generated_records (label) VALUES ('committed')")
            .unwrap();
        assert_eq!(adapter.connection.last_insert_id(), 2);
        adapter.execute_reset_connection().unwrap();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id FROM generated_records")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert_eq!(result.rows, [vec![Some(b"2".to_vec())]]);
    }

    #[test]
    fn direct_adapter_reset_stops_after_a_rollback_failure() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"retained");
        adapter.execute_query("SET SESSION autocommit = 0").unwrap();
        adapter
            .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'discarded')")
            .unwrap();
        adapter.connection.close().unwrap();

        assert!(adapter.execute_reset_connection().is_err());
        assert!(!adapter.connection.session_autocommit());
        assert!(!adapter.connection.is_auto_commit());
        assert!(adapter
            .connection
            .prepared_statement_metadata(prepared.statement_id)
            .is_none());
        assert_eq!(
            adapter
                .pending_long_data
                .values
                .get(&(prepared.statement_id, 0)),
            Some(&b"retained".to_vec())
        );
    }

    #[test]
    fn direct_adapter_executes_binary_parameters_and_reuses_cached_types() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?, ?, ?").unwrap();
        let mut first_payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0, MYSQL_TYPE_VAR_STRING, 0];
        first_payload.extend_from_slice(&[MYSQL_TYPE_BLOB, 0]);
        first_payload.extend_from_slice(&(-7i64).to_le_bytes());
        first_payload.extend_from_slice(&[3, b'A', b'd', b'a']);
        first_payload.extend_from_slice(&[2, 0, 0xff]);

        let first = adapter
            .execute_stmt_execute(prepared.statement_id, &first_payload)
            .unwrap();
        let first = prepared_result_set(first);
        assert_eq!(
            first.rows,
            [vec![
                BinaryResultValue::Integer(-7),
                BinaryResultValue::Text("Ada".to_string()),
                BinaryResultValue::Blob(vec![0, 0xff]),
            ]]
        );
        assert_eq!(
            first
                .columns
                .iter()
                .map(|column| column.column_type)
                .collect::<Vec<_>>(),
            [MYSQL_TYPE_LONGLONG, MYSQL_TYPE_VAR_STRING, MYSQL_TYPE_BLOB]
        );

        let mut second_payload = vec![0, 0];
        second_payload.extend_from_slice(&8i64.to_le_bytes());
        second_payload.extend_from_slice(&[5, b'G', b'r', b'a', b'c', b'e']);
        second_payload.extend_from_slice(&[1, 1]);
        let second = adapter
            .execute_stmt_execute(prepared.statement_id, &second_payload)
            .unwrap();
        let second = prepared_result_set(second);
        assert_eq!(
            second.rows,
            [vec![
                BinaryResultValue::Integer(8),
                BinaryResultValue::Text("Grace".to_string()),
                BinaryResultValue::Blob(vec![1]),
            ]]
        );
        assert_eq!(
            second
                .columns
                .iter()
                .map(|column| column.column_type)
                .collect::<Vec<_>>(),
            [MYSQL_TYPE_LONGLONG, MYSQL_TYPE_VAR_STRING, MYSQL_TYPE_BLOB]
        );
    }

    #[test]
    fn direct_adapter_appends_long_data_and_consumes_it_on_execute() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?, ?").unwrap();
        adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"long ");
        adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"text");
        adapter.execute_stmt_send_long_data(prepared.statement_id, 1, &[0, 0xff]);
        let payload = [0, 1, MYSQL_TYPE_VAR_STRING, 0, MYSQL_TYPE_BLOB, 0];

        let result = adapter
            .execute_stmt_execute(prepared.statement_id, &payload)
            .unwrap();
        assert_eq!(
            prepared_result_set(result).rows,
            [vec![
                BinaryResultValue::Text("long text".to_string()),
                BinaryResultValue::Blob(vec![0, 0xff]),
            ]]
        );

        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[0, 0]),
            Err(FrontendErrorKind::Syntax)
        );
    }

    #[test]
    fn direct_adapter_reset_forgets_long_data_and_send_errors_are_delayed() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"forgotten");
        assert_eq!(adapter.execute_stmt_reset(prepared.statement_id), Ok(()));
        let mut ordinary = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0];
        ordinary.extend_from_slice(&[4, b'k', b'e', b'p', b't']);
        assert_eq!(
            prepared_result_set(
                adapter
                    .execute_stmt_execute(prepared.statement_id, &ordinary)
                    .unwrap()
            )
            .rows,
            [vec![BinaryResultValue::Text("kept".to_string())]]
        );

        adapter.execute_stmt_send_long_data(prepared.statement_id, 1, b"invalid");
        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[0, 0, 0]),
            Err(FrontendErrorKind::Syntax)
        );
        assert!(adapter.pending_long_data.values.is_empty());
        assert!(adapter.pending_long_data.errors.is_empty());

        adapter.execute_stmt_send_long_data(u32::MAX, 0, b"unknown");
        assert_eq!(
            adapter.execute_stmt_execute(u32::MAX, &[]),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );
        assert!(adapter.pending_long_data.values.is_empty());
        assert!(adapter.pending_long_data.errors.is_empty());
        assert_eq!(adapter.pending_long_data.retained_bytes, 0);
    }

    #[test]
    fn direct_adapter_drops_long_data_for_unknown_statement_flood() {
        let mut adapter = adapter();
        for statement_id in 1..=100_000 {
            adapter.execute_stmt_send_long_data(statement_id, 0, b"unknown");
        }

        assert!(adapter.pending_long_data.values.is_empty());
        assert!(adapter.pending_long_data.errors.is_empty());
        assert_eq!(adapter.pending_long_data.retained_bytes, 0);
        assert_eq!(
            adapter.execute_stmt_execute(100_000, &[]),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );
    }

    #[test]
    fn pending_long_data_limit_fails_without_retaining_the_overflowing_chunk() {
        let mut pending = PendingLongData::default();
        let full = vec![0xaa; MAX_PREPARED_LONG_DATA_BYTES];
        pending.append(1, 0, &full, 1);
        pending.append(1, 0, &[0xbb], 1);
        assert_eq!(pending.retained_bytes, 0);
        let statement = pending.take_statement(1);
        assert_eq!(statement.error, Some(PendingLongDataError::TooLarge));
        assert!(statement.values.is_empty());
        assert_eq!(pending.retained_bytes, 0);
    }

    #[test]
    fn direct_adapter_executes_prepared_insert_update_and_delete_as_ok_results() {
        let mut adapter = adapter();
        let insert = adapter
            .execute_stmt_prepare("INSERT INTO result_values (id, payload) VALUES (?, ?)")
            .unwrap();
        assert!(insert.columns.is_empty());
        let mut insert_payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0, MYSQL_TYPE_BLOB, 0];
        insert_payload.extend_from_slice(&3i64.to_le_bytes());
        insert_payload.extend_from_slice(&[2, 0xaa, 0xbb]);
        assert_eq!(
            adapter
                .execute_stmt_execute(insert.statement_id, &insert_payload)
                .unwrap(),
            PreparedStatementExecutionResult::Ok(CommandOkResult {
                affected_rows: 1,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
                ..CommandOkResult::default()
            })
        );

        let update = adapter
            .execute_stmt_prepare("UPDATE result_values SET payload = ? WHERE TRUE")
            .expect("prepared UPDATE should compile");
        let mut update_payload = vec![0, 1, MYSQL_TYPE_BLOB, 0];
        update_payload.extend_from_slice(&[1, 0xcc]);
        assert!(matches!(
            adapter
                .execute_stmt_execute(update.statement_id, &update_payload)
                .unwrap(),
            PreparedStatementExecutionResult::Ok(CommandOkResult {
                affected_rows: 3,
                ..
            })
        ));

        let delete = adapter
            .execute_stmt_prepare("DELETE FROM result_values WHERE TRUE")
            .unwrap();
        assert!(matches!(
            adapter
                .execute_stmt_execute(delete.statement_id, &[])
                .unwrap(),
            PreparedStatementExecutionResult::Ok(CommandOkResult {
                affected_rows: 3,
                ..
            })
        ));
    }

    #[test]
    fn prepared_result_metadata_matches_unknown_parameter_values() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?, ?, ?, ?").unwrap();
        let mut payload = vec![0, 1];
        payload.extend_from_slice(&[
            MYSQL_TYPE_LONGLONG,
            0,
            MYSQL_TYPE_DOUBLE,
            0,
            MYSQL_TYPE_VAR_STRING,
            0,
            MYSQL_TYPE_BLOB,
            0,
        ]);
        payload.extend_from_slice(&(-7i64).to_le_bytes());
        payload.extend_from_slice(&1.5f64.to_le_bytes());
        payload.extend_from_slice(&[3, b'A', b'd', b'a']);
        payload.extend_from_slice(&[2, 0, 0xff]);

        let result = adapter
            .execute_stmt_execute(prepared.statement_id, &payload)
            .unwrap();
        let result = prepared_result_set(result);

        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| column.column_type)
                .collect::<Vec<_>>(),
            [
                MYSQL_TYPE_LONGLONG,
                MYSQL_TYPE_DOUBLE,
                MYSQL_TYPE_VAR_STRING,
                MYSQL_TYPE_BLOB,
            ]
        );
        assert_eq!(
            result.rows,
            [vec![
                BinaryResultValue::Integer(-7),
                BinaryResultValue::Real(1.5),
                BinaryResultValue::Text("Ada".to_string()),
                BinaryResultValue::Blob(vec![0, 0xff]),
            ]]
        );
    }

    #[test]
    fn a_marker_keeps_the_type_its_first_non_null_value_established() {
        // Measured against MySQL 8.4.11: a marker starts generic, an integer
        // settles it on LONGLONG with its own length and flags, and a later
        // NULL keeps that type rather than returning to generic.
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();

        let generic = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
                .unwrap(),
        );
        assert_eq!(generic.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
        assert_eq!(generic.columns[0].column_length, 65_532);
        assert_eq!(generic.columns[0].decimals, 31);
        assert_eq!(generic.columns[0].flags, 0);

        let mut integer = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
        integer.extend_from_slice(&7i64.to_le_bytes());
        let typed = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &integer)
                .unwrap(),
        );
        assert_eq!(typed.columns[0].column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(typed.columns[0].column_length, 21);
        assert_eq!(typed.columns[0].decimals, 0);
        assert_eq!(typed.columns[0].flags, MYSQL_BINARY_FLAG);
        assert_eq!(typed.rows, [vec![BinaryResultValue::Integer(7)]]);

        let after_null = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
                .unwrap(),
        );
        assert_eq!(after_null.columns[0].column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(after_null.columns[0].column_length, 21);
        assert_eq!(after_null.rows, [vec![BinaryResultValue::Null]]);
    }

    #[cfg(unix)]
    #[test]
    fn a_real_marker_reports_the_double_metadata_mysql_sends() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        let mut real = vec![0, 1, MYSQL_TYPE_DOUBLE, 0];
        real.extend_from_slice(&1.5f64.to_le_bytes());
        let typed = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &real)
                .unwrap(),
        );
        assert_eq!(typed.columns[0].column_type, MYSQL_TYPE_DOUBLE);
        assert_eq!(typed.columns[0].column_length, 23);
        assert_eq!(typed.columns[0].decimals, 31);
        assert_eq!(typed.columns[0].character_set, MYSQL_BINARY_COLLATION);
        assert_eq!(typed.columns[0].flags, MYSQL_BINARY_FLAG);

        let after_null = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
                .unwrap(),
        );
        assert_eq!(after_null.columns[0].column_type, MYSQL_TYPE_DOUBLE);
    }

    #[cfg(unix)]
    #[test]
    fn a_prepare_reports_the_same_marker_metadata_an_execute_does() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        let column = &prepared.columns[0];
        assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
        assert_eq!(column.column_length, 65_532);
        assert_eq!(column.decimals, 31);
        assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
        assert_eq!(column.flags, 0);

        let executed = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
                .unwrap(),
        );
        assert_eq!(executed.columns[0].column_type, column.column_type);
        assert_eq!(executed.columns[0].column_length, column.column_length);
        assert_eq!(executed.columns[0].decimals, column.decimals);
        assert_eq!(executed.columns[0].character_set, column.character_set);
        assert_eq!(executed.columns[0].flags, column.flags);
    }

    #[test]
    fn prepared_result_keeps_known_and_all_null_column_types() {
        let mut adapter = adapter();
        let unknown = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        let null_result = adapter
            .execute_stmt_execute(unknown.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
            .unwrap();
        let null_result = prepared_result_set(null_result);
        // MySQL 8.4.11 answers a marker that has only ever seen NULL with its
        // generic string type, not MYSQL_TYPE_NULL.
        assert_eq!(null_result.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
        assert_eq!(null_result.columns[0].column_length, 65_532);
        assert_eq!(null_result.columns[0].decimals, 31);
        assert_eq!(null_result.rows, [vec![BinaryResultValue::Null]]);

        let known = adapter
            .execute_stmt_prepare("SELECT id FROM result_values")
            .unwrap();
        let known_result = adapter
            .execute_stmt_execute(known.statement_id, &[])
            .unwrap();
        let known_result = prepared_result_set(known_result);
        assert_eq!(known_result.columns[0].column_type, MYSQL_TYPE_LONG);
        assert_eq!(
            known_result.rows,
            [
                vec![BinaryResultValue::Integer(1)],
                vec![BinaryResultValue::Integer(2)],
            ]
        );
    }

    #[test]
    fn prepared_result_preserves_declared_integer_wire_widths_for_empty_and_null_rows() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute(
                "CREATE TABLE declared_widths (tiny TINYINT, small SMALLINT, int_value INT, integer_value INTEGER, big BIGINT)",
            )
            .unwrap();
        let prepared = adapter
            .execute_stmt_prepare(
                "SELECT tiny, small, int_value, integer_value, big FROM declared_widths",
            )
            .unwrap();
        let expected_types = [
            MYSQL_TYPE_TINY,
            MYSQL_TYPE_SHORT,
            MYSQL_TYPE_LONG,
            MYSQL_TYPE_LONG,
            MYSQL_TYPE_LONGLONG,
        ];
        assert_eq!(
            prepared
                .columns
                .iter()
                .map(|column| column.column_type)
                .collect::<Vec<_>>(),
            expected_types.to_vec()
        );
        assert_eq!(
            prepared
                .columns
                .iter()
                .map(|column| column.column_length)
                .collect::<Vec<_>>(),
            [4, 6, 11, 11, 20].to_vec()
        );

        let empty = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty
                .columns
                .iter()
                .map(|column| column.column_type)
                .collect::<Vec<_>>(),
            expected_types.to_vec()
        );

        adapter
            .connection
            .execute(
                "INSERT INTO declared_widths (tiny, small, int_value, integer_value, big) VALUES (NULL, NULL, NULL, NULL, NULL)",
            )
            .unwrap();
        let all_null = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert_eq!(
            all_null
                .columns
                .iter()
                .map(|column| column.column_type)
                .collect::<Vec<_>>(),
            expected_types.to_vec()
        );
        assert_eq!(
            all_null.rows,
            [vec![
                BinaryResultValue::Null,
                BinaryResultValue::Null,
                BinaryResultValue::Null,
                BinaryResultValue::Null,
                BinaryResultValue::Null,
            ]]
        );

        adapter
            .connection
            .execute(
                "INSERT INTO declared_widths (tiny, small, int_value, integer_value, big) VALUES (1, 2, 3, 4, 5)",
            )
            .unwrap();
        let values = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert_eq!(
            values.rows,
            [
                vec![
                    BinaryResultValue::Null,
                    BinaryResultValue::Null,
                    BinaryResultValue::Null,
                    BinaryResultValue::Null,
                    BinaryResultValue::Null,
                ],
                vec![
                    BinaryResultValue::Integer(1),
                    BinaryResultValue::Integer(2),
                    BinaryResultValue::Integer(3),
                    BinaryResultValue::Integer(4),
                    BinaryResultValue::Integer(5),
                ],
            ]
        );

        let expression = adapter
            .execute_stmt_prepare(
                "SELECT tiny AS tiny_alias, 1 AS literal_expression, NULL AS null_expression FROM declared_widths",
            )
            .unwrap();
        assert_eq!(
            expression
                .columns
                .iter()
                .map(|column| column.column_type)
                .collect::<Vec<_>>(),
            [MYSQL_TYPE_TINY, MYSQL_TYPE_LONGLONG, MYSQL_TYPE_NULL].to_vec()
        );
        let expression_result = prepared_result_set(
            adapter
                .execute_stmt_execute(expression.statement_id, &[])
                .unwrap(),
        );
        assert_eq!(
            expression_result.rows,
            [
                vec![
                    BinaryResultValue::Null,
                    BinaryResultValue::Integer(1),
                    BinaryResultValue::Null,
                ],
                vec![
                    BinaryResultValue::Integer(1),
                    BinaryResultValue::Integer(1),
                    BinaryResultValue::Null,
                ],
            ]
        );
    }

    #[test]
    fn prepared_mediumint_result_preserves_boundaries_nulls_and_empty_metadata() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute("CREATE TABLE prepared_mediumint (value MEDIUMINT)")
            .unwrap();
        let prepared = adapter
            .execute_stmt_prepare("SELECT value FROM prepared_mediumint")
            .unwrap();
        assert_eq!(prepared.columns[0].column_type, MYSQL_TYPE_INT24);
        assert_eq!(prepared.columns[0].column_length, 9);

        let empty = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert!(empty.rows.is_empty());
        assert_eq!(empty.columns[0].column_type, MYSQL_TYPE_INT24);
        assert_eq!(empty.columns[0].column_length, 9);

        adapter
            .connection
            .execute("INSERT INTO prepared_mediumint (value) VALUES (-8388608), (8388607), (NULL)")
            .unwrap();
        let result = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert_eq!(result.columns[0].column_type, MYSQL_TYPE_INT24);
        assert_eq!(result.columns[0].column_length, 9);
        assert_eq!(
            result.rows,
            [
                vec![BinaryResultValue::Integer(-8_388_608)],
                vec![BinaryResultValue::Integer(8_388_607)],
                vec![BinaryResultValue::Null],
            ]
        );
    }

    #[cfg(unix)]
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedDatabaseAction {
        Connect(Option<String>),
        Query(String),
        TableSelect { database: String, table: String },
        Create(String),
        Drop(String),
        List,
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct RecordingAuthorizer {
        decisions: Mutex<VecDeque<Result<(), AuthorizationError>>>,
        table_decisions: Mutex<VecDeque<Result<(), AuthorizationError>>>,
        actions: Mutex<Vec<RecordedDatabaseAction>>,
        account_ids: Mutex<Vec<AccountId>>,
    }

    #[cfg(unix)]
    impl RecordingAuthorizer {
        fn with_decisions(
            decisions: impl IntoIterator<Item = Result<(), AuthorizationError>>,
        ) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().collect()),
                ..Self::default()
            }
        }

        fn with_decisions_and_table_decisions(
            decisions: impl IntoIterator<Item = Result<(), AuthorizationError>>,
            table_decisions: impl IntoIterator<Item = Result<(), AuthorizationError>>,
        ) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().collect()),
                table_decisions: Mutex::new(table_decisions.into_iter().collect()),
                ..Self::default()
            }
        }

        fn actions(&self) -> Vec<RecordedDatabaseAction> {
            self.actions.lock().unwrap().clone()
        }
    }

    #[cfg(unix)]
    impl DatabaseAuthorizer for RecordingAuthorizer {
        fn authorize(
            &self,
            principal: &AuthenticatedPrincipal,
            action: DatabaseAction<'_>,
        ) -> Result<(), AuthorizationError> {
            self.account_ids
                .lock()
                .unwrap()
                .push(principal.account_id().clone());
            let action = match action {
                DatabaseAction::Connect { database } => {
                    RecordedDatabaseAction::Connect(database.map(str::to_owned))
                }
                DatabaseAction::Query { database } => {
                    RecordedDatabaseAction::Query(database.to_owned())
                }
                DatabaseAction::Create { database } => {
                    RecordedDatabaseAction::Create(database.to_owned())
                }
                DatabaseAction::Drop { database } => {
                    RecordedDatabaseAction::Drop(database.to_owned())
                }
                DatabaseAction::List => RecordedDatabaseAction::List,
            };
            self.actions.lock().unwrap().push(action);
            self.decisions.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }

        fn authorize_table(
            &self,
            principal: &AuthenticatedPrincipal,
            action: TableAction<'_>,
        ) -> Result<(), AuthorizationError> {
            self.account_ids
                .lock()
                .unwrap()
                .push(principal.account_id().clone());
            let TableAction::Select { database, table } = action;
            self.actions
                .lock()
                .unwrap()
                .push(RecordedDatabaseAction::TableSelect {
                    database: database.to_owned(),
                    table: table.to_owned(),
                });
            self.table_decisions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(AuthorizationError::Denied))
        }
    }

    #[cfg(unix)]
    fn catalog_factory(
        authorizer: Arc<RecordingAuthorizer>,
    ) -> (
        tempfile::TempDir,
        Arc<MySqlDatabaseCatalog>,
        AuthorizedDatabaseAdapterFactory<RecordingAuthorizer>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
        catalog.create("reports").unwrap();
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("reports").unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE TABLE records (id INT, label TEXT)")
            .unwrap();
        seed.connection()
            .unwrap()
            .execute("INSERT INTO records (id, label) VALUES (7, 'kept')")
            .unwrap();
        drop(seed);
        let factory =
            AuthorizedDatabaseAdapterFactory::new(catalog.clone(), binary_context(), authorizer);
        (directory, catalog, factory)
    }

    #[cfg(unix)]
    #[test]
    fn authorized_text_select_uses_durable_table_metadata_for_alias_and_star() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("reports").unwrap();
        seed.connection()
            .unwrap()
            .execute(
                "CREATE TABLE metadata (id INTEGER NOT NULL PRIMARY KEY, label TEXT DEFAULT 'x' UNIQUE, payload BLOB)",
            )
            .unwrap();

        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([41; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id AS alias, label FROM metadata AS source")
            .unwrap()
        else {
            panic!("table SELECT must return a result set");
        };
        assert_eq!(result.columns[0].name, "alias");
        assert_eq!(result.columns[0].original_name, "id");
        assert_eq!(result.columns[0].schema, "reports");
        assert_eq!(result.columns[0].table, "source");
        assert_eq!(result.columns[0].original_table, "metadata");
        assert_eq!(
            result.columns[0].flags,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_PRI_KEY_FLAG
                | MYSQL_PART_KEY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
        );
        assert_eq!(result.columns[1].name, "label");
        assert_eq!(result.columns[1].original_name, "label");
        assert_eq!(result.columns[1].table, "source");
        assert_eq!(result.columns[1].original_table, "metadata");
        assert_eq!(
            result.columns[1].flags,
            MYSQL_UNIQUE_KEY_FLAG | MYSQL_PART_KEY_FLAG
        );
        let codec = PacketCodec::new(4096).unwrap();
        let frame = result.columns[0].encode(codec, 1).unwrap();
        let decoded = crate::ColumnDefinitionPacket::decode(codec, &frame).unwrap();
        let expected_flags = mysql_common::constants::ColumnFlags::NOT_NULL_FLAG.bits()
            | mysql_common::constants::ColumnFlags::PRI_KEY_FLAG.bits()
            | mysql_common::constants::ColumnFlags::PART_KEY_FLAG.bits()
            | mysql_common::constants::ColumnFlags::NO_DEFAULT_VALUE_FLAG.bits();
        assert_eq!(decoded.flags, result.columns[0].flags);
        assert_eq!(decoded.flags, expected_flags);

        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SELECT * FROM metadata").unwrap()
        else {
            panic!("star SELECT must return a result set");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.original_name.as_str(),
                    column.table.as_str(),
                    column.original_table.as_str(),
                    column.schema.as_str(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("id", "id", "metadata", "metadata", "reports"),
                ("label", "label", "metadata", "metadata", "reports"),
                ("payload", "payload", "metadata", "metadata", "reports"),
            ]
        );
        assert_eq!(result.columns[2].flags, 0);

        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SELECT 1 AS literal").unwrap()
        else {
            panic!("literal SELECT must return a result set");
        };
        assert!(result.columns[0].schema.is_empty());
        assert!(result.columns[0].table.is_empty());
        assert!(result.columns[0].original_table.is_empty());
        assert!(result.columns[0].original_name.is_empty());

        let prepared = adapter
            .execute_stmt_prepare("SELECT id AS alias, label FROM metadata AS source")
            .unwrap();
        assert_eq!(prepared.columns[0].name, "alias");
        assert_eq!(prepared.columns[0].original_name, "id");
        assert_eq!(prepared.columns[0].schema, "reports");
        assert_eq!(prepared.columns[0].table, "source");
        assert_eq!(prepared.columns[0].original_table, "metadata");
        assert_eq!(
            prepared.columns[0].flags,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_PRI_KEY_FLAG
                | MYSQL_PART_KEY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
        );
        let PreparedStatementExecutionResult::ResultSet(result) = adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap()
        else {
            panic!("prepared table SELECT must return a result set");
        };
        assert_eq!(result.columns[0], prepared.columns[0]);
        assert_eq!(result.columns[1], prepared.columns[1]);
    }

    #[cfg(unix)]
    #[test]
    fn authorized_factory_forwards_optional_query_timeout() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut default_adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([23; 32]),
            ))
            .unwrap();
        assert_eq!(default_adapter.query_timeout, None);
        default_adapter.authorize_connection().unwrap();
        default_adapter.execute_init_db("reports").unwrap();
        assert!(matches!(
            default_adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));

        let timeout = Duration::from_secs(2);
        let configured_adapter = AuthorizedDatabaseAdapterFactory::new(
            catalog.clone(),
            binary_context(),
            authorizer.clone(),
        )
        .with_query_timeout(timeout)
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([24; 32]),
        ))
        .unwrap();
        assert_eq!(configured_adapter.query_timeout, Some(timeout));

        let bootstrap_timeout = Duration::from_millis(1500);
        let bootstrap_adapter = AuthorizedDatabaseAdapterFactory::new(
            catalog.clone(),
            binary_context(),
            authorizer.clone(),
        )
        .with_bootstrap_settings(8192, bootstrap_timeout)
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([27; 32]),
        ))
        .unwrap();
        assert_eq!(
            bootstrap_adapter.bootstrap_settings,
            MySqlBootstrapSettings::new(8192, bootstrap_timeout)
        );

        let options = CommandExecutionOptions::from_capability_flags(CLIENT_FOUND_ROWS);
        let option_adapter =
            AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
                .build_with_options(
                    AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes(
                        [25; 32],
                    )),
                    options,
                )
                .unwrap();
        assert_eq!(option_adapter.command_options(), options);
        assert!(option_adapter.command_options().client_found_rows());
    }

    #[cfg(unix)]
    #[test]
    fn authorized_factories_share_the_injected_prepared_statement_authority() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, _factory) = catalog_factory(authorizer.clone());
        let authority = MySqlPreparedStatementAuthority::new(1).unwrap();
        let first_factory = AuthorizedDatabaseAdapterFactory::new(
            catalog.clone(),
            binary_context(),
            authorizer.clone(),
        )
        .with_prepared_statement_authority(authority.clone());
        let second_factory =
            AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
                .with_prepared_statement_authority(authority.clone());
        let principal = |id| {
            AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([id; 32]))
        };
        let mut first = first_factory.build(principal(31)).unwrap();
        let mut second = second_factory.build(principal(32)).unwrap();
        first.authorize_connection().unwrap();
        second.authorize_connection().unwrap();
        first.execute_init_db("reports").unwrap();
        second.execute_init_db("reports").unwrap();

        first.execute_stmt_prepare("SELECT 1").unwrap();
        assert_eq!(authority.active_count(), 1);
        assert_eq!(
            second.execute_stmt_prepare("SELECT 2"),
            Err(FrontendErrorKind::PreparedStatementLimitReached)
        );
        first.execute_stmt_close(1);
        assert_eq!(authority.active_count(), 0);
        second.execute_stmt_prepare("SELECT 2").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn authorized_prepare_requires_selection_and_query_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Err(AuthorizationError::Denied),
            Ok(()),
        ]));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([26; 32]),
            ))
            .unwrap();

        assert_eq!(
            adapter.execute_stmt_prepare("SELECT 1"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        adapter.execute_init_db("reports").unwrap();
        assert_eq!(
            adapter.execute_stmt_prepare("SELECT 1"),
            Err(FrontendErrorKind::AccessDenied)
        );
        let prepared = adapter.execute_stmt_prepare("SELECT ? AS value").unwrap();
        assert_eq!(prepared.statement_id, 1);
        assert_eq!(prepared.parameters.len(), 1);
        assert_eq!(prepared.columns[0].name, "value");
        assert_eq!(
            authorizer.actions(),
            [
                RecordedDatabaseAction::Connect(Some("reports".to_string())),
                RecordedDatabaseAction::Query("reports".to_string()),
                RecordedDatabaseAction::Query("reports".to_string()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_database_select_falls_back_to_canonical_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied), Ok(())],
            [Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([32; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();

        assert!(matches!(
            adapter.execute_query("SELECT id FROM `RECORDS`"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_database_select_does_not_try_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
            [],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([33; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_query("SELECT id FROM records"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_database_prepare_does_not_try_table_permission_or_provider() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
            [],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([39; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_stmt_prepare("SELECT id FROM records"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_database_select_checks_table_before_missing_table_lookup() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
            [Err(AuthorizationError::Denied)],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([34; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_query("SELECT id FROM missing_table"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "missing_table".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_database_query_does_not_fallback_for_scalar_dml_or_qualified_select() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [
                Ok(()),
                Ok(()),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
            ],
            [],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([35; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for sql in [
            "SELECT 1",
            "INSERT INTO records (id, label) VALUES (8, 'blocked')",
            "SELECT id FROM main.records",
        ] {
            assert_eq!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::AccessDenied),
                "authorization must reject {sql:?} before execution"
            );
        }
        assert_eq!(
            adapter.execute_query("SELECT table_name FROM information_schema.tables"),
            Err(FrontendErrorKind::Syntax)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_prepare_does_not_fallback_for_non_simple_select_or_dml() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [
                Ok(()),
                Ok(()),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
            ],
            [],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([40; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for sql in [
            "SELECT id FROM main.records",
            "INSERT INTO records (id, label) VALUES (8, 'blocked')",
        ] {
            assert_eq!(
                adapter.execute_stmt_prepare(sql),
                Err(FrontendErrorKind::AccessDenied),
                "authorization must reject {sql:?} before preparation"
            );
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepared_select_reauthorizes_table_permission_and_keeps_origin_database() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [
                Ok(()),
                Ok(()),
                Err(AuthorizationError::Denied),
                Ok(()),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
            ],
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        ));
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        catalog.create("archive").unwrap();
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([36; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        let prepared = adapter
            .execute_stmt_prepare("SELECT id FROM `RECORDS`")
            .unwrap();
        adapter.execute_init_db("archive").unwrap();

        assert!(matches!(
            adapter.execute_stmt_execute(prepared.statement_id, &[]),
            Ok(PreparedStatementExecutionResult::ResultSet(_))
        ));
        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[]),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
                RecordedDatabaseAction::Connect(Some("archive".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepared_execute_preserves_long_data_until_query_authorization_succeeds() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
            [],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([37; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"kept");

        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[]),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            adapter
                .pending_long_data
                .values
                .get(&(prepared.statement_id, 0)),
            Some(&b"kept".to_vec())
        );
        assert_eq!(adapter.pending_long_data.retained_bytes, 4);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );

        let payload = [0, 1, MYSQL_TYPE_VAR_STRING, 0];
        assert!(matches!(
            adapter.execute_stmt_execute(prepared.statement_id, &payload),
            Ok(PreparedStatementExecutionResult::ResultSet(_))
        ));
        assert!(!adapter
            .pending_long_data
            .values
            .contains_key(&(prepared.statement_id, 0)));
        assert_eq!(adapter.pending_long_data.retained_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn unknown_prepared_execute_does_not_retain_pending_long_data() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([38; 32]),
            ))
            .unwrap();
        adapter.execute_stmt_send_long_data(u32::MAX, 0, b"unknown");

        assert_eq!(
            adapter.execute_stmt_execute(u32::MAX, &[]),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );
        assert!(adapter.pending_long_data.values.is_empty());
        assert!(adapter.pending_long_data.errors.is_empty());
        assert_eq!(adapter.pending_long_data.retained_bytes, 0);
        assert!(authorizer.actions().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn authorized_adapter_reset_keeps_database_and_clears_connection_state() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([30; 32]),
            ))
            .unwrap();
        adapter.execute_reset_connection().unwrap();
        assert_eq!(adapter.session.selected_database(), None);
        assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
        adapter.execute_init_db("reports").unwrap();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"discarded");
        adapter
            .execute_query(
                "CREATE TABLE generated_records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
            )
            .unwrap();
        adapter
            .execute_query("INSERT INTO generated_records (label) VALUES ('before_reset')")
            .unwrap();
        adapter.execute_query("BEGIN").unwrap();
        adapter
            .execute_query("INSERT INTO records (id, label) VALUES (8, 'discarded')")
            .unwrap();

        adapter.execute_reset_connection().unwrap();

        assert_eq!(adapter.session.selected_database(), Some("reports"));
        assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(adapter.session.connection().unwrap().last_insert_id(), 0);
        assert!(adapter.pending_long_data.values.is_empty());
        assert!(adapter.pending_long_data.errors.is_empty());
        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[0, 0, MYSQL_TYPE_VAR_STRING, 0]),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );

        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id, label FROM records")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert_eq!(
            result.rows,
            [vec![Some(b"7".to_vec()), Some(b"kept".to_vec())]]
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_adapter_reset_clears_prepared_state_across_database_switches() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        catalog.create("archive").unwrap();
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([31; 32]),
            ))
            .unwrap();
        adapter.execute_init_db("reports").unwrap();
        let reports = adapter
            .execute_stmt_prepare("SELECT ? AS report_value")
            .unwrap();
        adapter.execute_stmt_send_long_data(reports.statement_id, 0, b"reports");

        adapter.execute_init_db("archive").unwrap();
        let archive = adapter
            .execute_stmt_prepare("SELECT ? AS archive_value")
            .unwrap();
        adapter.execute_stmt_send_long_data(archive.statement_id, 0, b"archive");

        adapter.execute_reset_connection().unwrap();

        assert_eq!(adapter.session.selected_database(), Some("archive"));
        assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
        assert!(adapter.pending_long_data.values.is_empty());
        assert!(adapter.pending_long_data.errors.is_empty());
        assert_eq!(
            adapter.execute_stmt_execute(reports.statement_id, &[]),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );
        assert_eq!(
            adapter.execute_stmt_execute(archive.statement_id, &[]),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );
        catalog.drop_database("reports").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn authorized_prepared_statements_keep_origin_connections_across_database_switches() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        catalog.create("archive").unwrap();
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([28; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        let reports = adapter
            .execute_stmt_prepare("SELECT ? AS report_value")
            .unwrap();
        adapter.execute_stmt_send_long_data(reports.statement_id, 0, b"origin");

        adapter.execute_init_db("archive").unwrap();
        let archive = adapter
            .execute_stmt_prepare("SELECT 1 AS archive_value")
            .unwrap();
        assert_eq!((reports.statement_id, archive.statement_id), (1, 2));
        assert!(matches!(
            catalog.drop_database("reports"),
            Err(MySqlDatabaseError::DatabaseBusy(name)) if name == "reports"
        ));

        let first_payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0];
        let first = adapter
            .execute_stmt_execute(reports.statement_id, &first_payload)
            .unwrap();
        let first = prepared_result_set(first);
        assert_eq!(
            first.rows,
            [vec![BinaryResultValue::Text("origin".to_string())]]
        );

        let mut cached_type_payload = vec![0, 0];
        cached_type_payload.extend_from_slice(&[6, b'c', b'a', b'c', b'h', b'e', b'd']);
        let cached_type = adapter
            .execute_stmt_execute(reports.statement_id, &cached_type_payload)
            .unwrap();
        let cached_type = prepared_result_set(cached_type);
        assert_eq!(
            cached_type.rows,
            [vec![BinaryResultValue::Text("cached".to_string())]]
        );

        assert_eq!(adapter.execute_stmt_reset(reports.statement_id), Ok(()));
        adapter.execute_stmt_close(reports.statement_id);
        assert_eq!(
            adapter.execute_stmt_reset(reports.statement_id),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        );
        assert!(matches!(
            adapter.execute_stmt_execute(reports.statement_id, &cached_type_payload),
            Err(FrontendErrorKind::UnknownPreparedStatement)
        ));
        catalog.drop_database("reports").unwrap();

        let next = adapter
            .execute_stmt_prepare("SELECT 2 AS next_value")
            .unwrap();
        assert_eq!(next.statement_id, 3);
        assert_eq!(
            authorizer.actions(),
            [
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_string())),
                RecordedDatabaseAction::Query("reports".to_string()),
                RecordedDatabaseAction::Connect(Some("archive".to_string())),
                RecordedDatabaseAction::Query("archive".to_string()),
                RecordedDatabaseAction::Query("reports".to_string()),
                RecordedDatabaseAction::Query("reports".to_string()),
                RecordedDatabaseAction::Query("archive".to_string()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_database_switch_rejects_autocommit_disabled_before_a_transaction_starts() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        catalog.create("archive").unwrap();
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([29; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        adapter.execute_query("SET autocommit = 0").unwrap();

        assert_eq!(
            adapter.execute_init_db("archive"),
            Err(FrontendErrorKind::Unsupported)
        );
        assert_eq!(
            adapter.execute_query("USE archive"),
            Err(FrontendErrorKind::Unsupported)
        );
        assert_eq!(adapter.session.selected_database(), Some("reports"));
    }

    #[test]
    fn prepared_select_rejects_rows_beyond_dispatch_limit_during_execution() {
        let mut adapter = adapter();
        let prepared = adapter
            .execute_stmt_prepare("SELECT id FROM many_rows")
            .unwrap();

        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[]),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[test]
    fn prepared_execute_keeps_parameter_types_after_execution_error() {
        let mut adapter = adapter();
        let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        let mut first_payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
        first_payload.extend_from_slice(&1i64.to_le_bytes());

        let result = execute_prepared_statement(
            &adapter.connection,
            &mut adapter.prepared_types,
            prepared.statement_id,
            &first_payload,
            StatementLongData {
                values: Vec::new(),
                error: None,
            },
            Some(Duration::ZERO),
            MySqlAffectedRowsMode::Changed,
        );
        assert_eq!(result, Err(FrontendErrorKind::QueryTimeout));

        let mut retry_payload = vec![0, 0];
        retry_payload.extend_from_slice(&2i64.to_le_bytes());
        let retried = adapter
            .execute_stmt_execute(prepared.statement_id, &retry_payload)
            .unwrap();
        let retried = prepared_result_set(retried);
        assert_eq!(retried.rows, [vec![BinaryResultValue::Integer(2)]]);
    }

    #[cfg(unix)]
    #[test]
    #[should_panic(expected = "query timeout must be non-zero")]
    fn authorized_factory_rejects_zero_query_timeout() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        catalog.create("archive").unwrap();
        let _ = factory.with_query_timeout(Duration::ZERO);
    }

    #[test]
    fn select_result_preserves_null_and_binary_values() {
        let mut adapter = adapter();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id, payload FROM result_values")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };

        assert_eq!(result.columns.len(), 2);
        assert_eq!(
            result.rows,
            vec![
                vec![Some(b"1".to_vec()), Some(vec![0, 0xff])],
                vec![Some(b"2".to_vec()), None]
            ]
        );
        assert_eq!(result.columns[1].column_type, MYSQL_TYPE_BLOB);
        assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONG);
    }

    #[test]
    fn last_insert_id_is_available_through_the_checked_select_path() {
        let mut adapter = adapter();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT LAST_INSERT_ID() AS generated_id")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };

        assert_eq!(result.rows, vec![vec![Some(b"0".to_vec())]]);
        assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    }

    #[test]
    fn drop_view_dispatch_preserves_backticks_in_select_and_insert_strings() {
        let mut adapter = adapter();
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SELECT '`'").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(result.rows, vec![vec![Some(b"`".to_vec())]]);
        adapter
            .execute_query("CREATE TABLE quoted_values (label TEXT)")
            .unwrap();
        adapter
            .execute_query("INSERT INTO quoted_values (label) VALUES ('`')")
            .unwrap();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT label FROM quoted_values")
            .unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(result.rows, vec![vec![Some(b"`".to_vec())]]);
    }

    #[test]
    fn drop_view_commits_before_success_and_object_errors() {
        let mut adapter = adapter();
        adapter
            .execute_query("CREATE TABLE records (id INT)")
            .unwrap();
        adapter
            .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
            .unwrap();
        for (sql, expected) in [
            ("DROP VIEW records_view", None),
            (
                "DROP VIEW missing_view",
                Some(FrontendErrorKind::UnknownView),
            ),
            ("DROP VIEW records", Some(FrontendErrorKind::NotView)),
        ] {
            adapter.execute_query("BEGIN").unwrap();
            adapter
                .execute_query("INSERT INTO records (id) VALUES (7)")
                .unwrap();
            let result = adapter.execute_query(sql);
            if let Some(error) = expected {
                assert_eq!(result, Err(error));
            } else {
                let CommandExecutionResult::Ok(result) = result.unwrap() else {
                    panic!("DROP must return OK");
                };
                assert_eq!(result.affected_rows, 0);
                assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
            }
            assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
            adapter.execute_query("ROLLBACK").unwrap();
        }
        let CommandExecutionResult::ResultSet(rows) =
            adapter.execute_query("SELECT id FROM records").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(rows.rows.len(), 3);
        assert_eq!(
            adapter.execute_query("DROP VIEW records_view"),
            Err(FrontendErrorKind::UnknownView)
        );
        assert!(adapter
            .execute_query("SELECT id FROM records_view")
            .is_err());
    }

    #[test]
    fn drop_table_commits_and_respects_if_exists_warning_notes() {
        let mut adapter = adapter();
        adapter
            .execute_query("CREATE TABLE records (id INT)")
            .unwrap();
        adapter.execute_query("BEGIN").unwrap();
        adapter
            .execute_query("INSERT INTO records (id) VALUES (7)")
            .unwrap();
        assert_eq!(
            adapter.execute_query("DROP TABLE missing_records"),
            Err(FrontendErrorKind::UnknownTable)
        );
        adapter.execute_query("ROLLBACK").unwrap();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id FROM records")
            .unwrap()
        else {
            panic!("the failed DROP TABLE must commit preceding writes");
        };
        assert_eq!(result.rows, vec![vec![Some(b"7".to_vec())]]);
        let CommandExecutionResult::Ok(result) = adapter
            .execute_query("DROP TABLE records")
            .unwrap()
        else {
            panic!("DROP TABLE must return OK");
        };
        assert_eq!(result.warnings, 0);
        assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(adapter.execute_query("DROP TABLE records"), Err(FrontendErrorKind::UnknownTable));

        adapter.execute_query("SET sql_notes = 0").unwrap();
        let CommandExecutionResult::Ok(result) = adapter
            .execute_query("DROP TABLE IF EXISTS records")
            .unwrap()
        else {
            panic!("DROP TABLE IF EXISTS must return OK");
        };
        assert_eq!(result.warnings, 0);

        adapter.execute_query("SET sql_notes = 1").unwrap();
        let CommandExecutionResult::Ok(result) = adapter
            .execute_query("DROP TABLE IF EXISTS records")
            .unwrap()
        else {
            panic!("DROP TABLE IF EXISTS must return OK");
        };
        assert_eq!(result.warnings, 1);

        adapter
            .execute_query("CREATE TABLE records (id INT)")
            .unwrap();
        adapter
            .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
            .unwrap();
        assert_eq!(
            adapter.execute_query("DROP TABLE records_view"),
            Err(FrontendErrorKind::UnknownTable)
        );
        let CommandExecutionResult::Ok(result) = adapter
            .execute_query("DROP TABLE IF EXISTS records_view")
            .unwrap()
        else {
            panic!("DROP TABLE IF EXISTS must return OK for a view");
        };
        assert_eq!(result.warnings, 1);
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id FROM records_view")
            .unwrap()
        else {
            panic!("the view must remain after DROP TABLE IF EXISTS");
        };
        assert!(result.rows.is_empty());
    }

    #[test]
    fn sql_notes_is_isolated_and_resets_only_after_success() {
        let mut first = adapter();
        let mut second = adapter();
        first.execute_query("BEGIN").unwrap();
        let CommandExecutionResult::Ok(result) = first.execute_query("SET sql_notes = 0").unwrap()
        else {
            panic!("SET must return OK");
        };
        assert_eq!(
            result.status_flags,
            SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
        );
        for (adapter, expected) in [(&mut first, b"0"), (&mut second, b"1")] {
            let CommandExecutionResult::ResultSet(result) =
                adapter.execute_query("SELECT @@sql_notes").unwrap()
            else {
                panic!("SELECT must return rows");
            };
            assert_eq!(result.rows, vec![vec![Some(expected.to_vec())]]);
        }
        first.execute_reset_connection().unwrap();
        let CommandExecutionResult::ResultSet(result) =
            first.execute_query("SELECT @@sql_notes").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(result.rows, vec![vec![Some(b"1".to_vec())]]);
        first.execute_query("SET sql_notes = 0").unwrap();
        first.execute_query("BEGIN").unwrap();
        first
            .execute_query("INSERT INTO result_values (id) VALUES (3)")
            .unwrap();
        first.connection.close().unwrap();
        assert!(first.execute_reset_connection().is_err());
        let CommandExecutionResult::ResultSet(result) =
            first.execute_query("SELECT @@sql_notes").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(result.rows, vec![vec![Some(b"0".to_vec())]]);
    }

    #[test]
    fn checked_schema_commands_commit_pending_writes_and_report_idle_status() {
        for ddl in [
            "CREATE INDEX records_id ON records (id)",
            "CREATE VIEW records_view AS SELECT id FROM records",
            "ALTER TABLE records ADD COLUMN label TEXT",
        ] {
            let mut adapter = adapter();
            adapter
                .execute_query("CREATE TABLE records (id INT)")
                .unwrap();
            adapter.execute_query("SET autocommit = 0").unwrap();
            adapter
                .execute_query("INSERT INTO records (id) VALUES (7)")
                .unwrap();
            assert_eq!(adapter.status_flags(), SERVER_STATUS_IN_TRANS);
            let CommandExecutionResult::Ok(result) = adapter.execute_query(ddl).unwrap() else {
                panic!("schema command must return OK: {ddl}");
            };
            assert_eq!(result.status_flags, 0, "{ddl}");
            assert_eq!(result.affected_rows, 0, "{ddl}");
            assert_eq!(result.last_insert_id, 0, "{ddl}");
            adapter.execute_query("ROLLBACK").unwrap();
            let CommandExecutionResult::ResultSet(rows) =
                adapter.execute_query("SELECT id FROM records").unwrap()
            else {
                panic!("SELECT must return rows");
            };
            assert_eq!(rows.rows, vec![vec![Some(b"7".to_vec())]], "{ddl}");
        }
    }

    #[test]
    fn checked_schema_commands_preserve_view_and_altered_column_metadata() {
        let mut adapter = adapter();
        adapter
            .execute_query("CREATE TABLE records (id SMALLINT)")
            .unwrap();
        adapter
            .execute_query("INSERT INTO records (id) VALUES (7)")
            .unwrap();
        adapter
            .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
            .unwrap();
        let CommandExecutionResult::ResultSet(view) = adapter
            .execute_query("SELECT id FROM records_view")
            .unwrap()
        else {
            panic!("view SELECT must return rows");
        };
        assert_eq!(view.rows, vec![vec![Some(b"7".to_vec())]]);
        adapter
            .execute_query("ALTER TABLE records ADD COLUMN label TEXT DEFAULT 'new'")
            .unwrap();
        let CommandExecutionResult::ResultSet(altered) =
            adapter.execute_query("SELECT label FROM records").unwrap()
        else {
            panic!("altered column SELECT must return rows");
        };
        assert_eq!(altered.rows, vec![vec![Some(b"new".to_vec())]]);
        assert!(adapter
            .execute_query("ALTER TABLE records RENAME TO renamed_records")
            .is_err());
        for sql in [
            "DROP INDEX records_id ON records",
            "ALTER TABLE records ADD COLUMN a INT, ADD COLUMN b INT",
        ] {
            assert!(
                adapter.execute_query(sql).is_err(),
                "unsupported DDL accepted: {sql}"
            );
        }
    }

    #[test]
    fn empty_insert_distinguishes_missing_default_from_explicit_null() {
        let mut adapter = adapter();
        adapter
            .execute_query(
                "CREATE TABLE required_values (required INT NOT NULL, optional INT DEFAULT 7)",
            )
            .unwrap();
        for (sql, expected) in [
            (
                "INSERT INTO required_values () VALUES ()",
                FrontendErrorKind::MissingRequiredDefault,
            ),
            (
                "INSERT INTO required_values (required) VALUES (NULL)",
                FrontendErrorKind::NotNullViolation,
            ),
        ] {
            assert_eq!(adapter.execute_query(sql), Err(expected));
            let prepared = adapter.execute_stmt_prepare(sql).unwrap();
            assert_eq!(
                adapter.execute_stmt_execute(prepared.statement_id, &[]),
                Err(expected)
            );
            adapter.execute_stmt_close(prepared.statement_id);
        }
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT required FROM required_values")
            .unwrap()
        else {
            panic!("expected rows")
        };
        assert!(result.rows.is_empty());
        adapter
            .execute_query("CREATE TABLE default_values (value INT DEFAULT 7)")
            .unwrap();
        let prepared = adapter
            .execute_stmt_prepare("INSERT INTO default_values () VALUES ()")
            .unwrap();
        adapter
            .execute_query("ALTER TABLE default_values ADD COLUMN required INT NOT NULL")
            .unwrap();
        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[]),
            Err(FrontendErrorKind::MissingRequiredDefault)
        );
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT value FROM default_values")
            .unwrap()
        else {
            panic!("expected rows")
        };
        assert!(result.rows.is_empty());
    }

    #[test]
    fn checked_insert_and_delete_return_ok_results() {
        let mut adapter = adapter();
        let CommandExecutionResult::Ok(inserted) = adapter
            .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'kept')")
            .unwrap()
        else {
            panic!("INSERT must produce an OK result");
        };
        assert_eq!(inserted.affected_rows, 1);
        assert_eq!(inserted.last_insert_id, 0);

        let CommandExecutionResult::Ok(deleted) = adapter
            .execute_query("DELETE FROM result_values WHERE payload IS NULL")
            .unwrap()
        else {
            panic!("DELETE must produce an OK result");
        };
        assert_eq!(deleted.affected_rows, 1);
        assert_eq!(deleted.last_insert_id, 0);

        let CommandExecutionResult::Ok(deleted_again) = adapter
            .execute_query("DELETE FROM result_values WHERE payload IS NULL")
            .unwrap()
        else {
            panic!("DELETE must produce an OK result");
        };
        assert_eq!(deleted_again.affected_rows, 0);
    }

    #[test]
    fn explicit_transactions_report_status_and_rollback_rows() {
        let mut adapter = adapter();
        let CommandExecutionResult::Ok(begin) = adapter.execute_query("BEGIN").unwrap() else {
            panic!("BEGIN must produce an OK result");
        };
        assert_eq!(
            begin.status_flags,
            SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
        );

        let CommandExecutionResult::Ok(inserted) = adapter
            .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'discarded')")
            .unwrap()
        else {
            panic!("INSERT must produce an OK result");
        };
        assert_eq!(inserted.status_flags, begin.status_flags);

        let CommandExecutionResult::ResultSet(selected) = adapter
            .execute_query("SELECT id, payload FROM result_values")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert_eq!(selected.status_flags, begin.status_flags);

        let CommandExecutionResult::Ok(rollback) = adapter.execute_query("ROLLBACK").unwrap()
        else {
            panic!("ROLLBACK must produce an OK result");
        };
        assert_eq!(rollback.status_flags, SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);

        let CommandExecutionResult::ResultSet(selected) = adapter
            .execute_query("SELECT id, payload FROM result_values")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert_eq!(selected.rows.len(), 2);
    }

    #[test]
    fn autocommit_status_tracks_setting_and_lazy_write_transaction() {
        let mut adapter = adapter();
        let CommandExecutionResult::Ok(disabled) =
            adapter.execute_query("SET SESSION autocommit = 0").unwrap()
        else {
            panic!("SET autocommit must produce an OK result");
        };
        assert_eq!(disabled.status_flags, 0);

        let CommandExecutionResult::ResultSet(constant) =
            adapter.execute_query("SELECT 1 AS value").unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert_eq!(constant.status_flags, 0);

        let CommandExecutionResult::Ok(inserted) = adapter
            .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'pending')")
            .unwrap()
        else {
            panic!("INSERT must produce an OK result");
        };
        assert_eq!(inserted.status_flags, SERVER_STATUS_IN_TRANS);

        let CommandExecutionResult::Ok(committed) =
            adapter.execute_query("SET autocommit = 1").unwrap()
        else {
            panic!("SET autocommit must produce an OK result");
        };
        assert_eq!(committed.status_flags, SERVER_STATUS_AUTOCOMMIT);
    }

    #[cfg(unix)]
    #[test]
    fn active_transaction_rejects_database_switch_without_losing_state() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([27; 32]),
            ))
            .unwrap();
        adapter.execute_init_db("reports").unwrap();
        adapter.execute_query("BEGIN").unwrap();

        assert_eq!(
            adapter.execute_init_db("archive"),
            Err(FrontendErrorKind::Unsupported)
        );
        assert_eq!(
            adapter.execute_query("USE archive"),
            Err(FrontendErrorKind::Unsupported)
        );
        assert_eq!(
            adapter.status_flags(),
            SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
        );
        assert_eq!(adapter.session.selected_database(), Some("reports"));
    }

    #[cfg(unix)]
    #[test]
    fn authorized_adapter_applies_found_rows_to_update_ok_results() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, _factory) = catalog_factory(authorizer.clone());
        let principal =
            AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([26; 32]));

        let mut changed_rows = AuthorizedDatabaseAdapterFactory::new(
            catalog.clone(),
            binary_context(),
            authorizer.clone(),
        )
        .build(principal)
        .unwrap();
        changed_rows.authorize_connection().unwrap();
        changed_rows.execute_init_db("reports").unwrap();
        let CommandExecutionResult::Ok(result) = changed_rows
            .execute_query("UPDATE records SET label = 'kept' WHERE TRUE")
            .unwrap()
        else {
            panic!("UPDATE must produce an OK result");
        };
        assert_eq!(result.affected_rows, 0);

        let mut matched_rows =
            AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
                .build_with_options(
                    AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes(
                        [27; 32],
                    )),
                    CommandExecutionOptions::from_capability_flags(CLIENT_FOUND_ROWS),
                )
                .unwrap();
        matched_rows.authorize_connection().unwrap();
        matched_rows.execute_init_db("reports").unwrap();

        let CommandExecutionResult::Ok(no_op) = matched_rows
            .execute_query("UPDATE records SET label = 'kept' WHERE TRUE")
            .unwrap()
        else {
            panic!("UPDATE must produce an OK result");
        };
        assert_eq!(no_op.affected_rows, 1);

        let CommandExecutionResult::Ok(actual) = matched_rows
            .execute_query("UPDATE records SET label = 'changed' WHERE TRUE")
            .unwrap()
        else {
            panic!("UPDATE must produce an OK result");
        };
        assert_eq!(actual.affected_rows, 1);
    }

    #[cfg(unix)]
    #[test]
    fn authorized_prepared_update_applies_found_rows_option() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, _factory) = catalog_factory(authorizer.clone());

        let mut changed_rows = AuthorizedDatabaseAdapterFactory::new(
            catalog.clone(),
            binary_context(),
            authorizer.clone(),
        )
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([30; 32]),
        ))
        .unwrap();
        changed_rows.authorize_connection().unwrap();
        changed_rows.execute_init_db("reports").unwrap();
        let changed = changed_rows
            .execute_stmt_prepare("UPDATE records SET label = ? WHERE TRUE")
            .unwrap();
        let payload = [0, 1, MYSQL_TYPE_VAR_STRING, 0, 4, b'k', b'e', b'p', b't'];
        assert!(matches!(
            changed_rows
                .execute_stmt_execute(changed.statement_id, &payload)
                .unwrap(),
            PreparedStatementExecutionResult::Ok(CommandOkResult {
                affected_rows: 0,
                ..
            })
        ));

        let mut matched_rows =
            AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
                .build_with_options(
                    AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes(
                        [31; 32],
                    )),
                    CommandExecutionOptions::from_capability_flags(CLIENT_FOUND_ROWS),
                )
                .unwrap();
        matched_rows.authorize_connection().unwrap();
        matched_rows.execute_init_db("reports").unwrap();
        let matched = matched_rows
            .execute_stmt_prepare("UPDATE records SET label = ? WHERE TRUE")
            .unwrap();
        assert!(matches!(
            matched_rows
                .execute_stmt_execute(matched.statement_id, &payload)
                .unwrap(),
            PreparedStatementExecutionResult::Ok(CommandOkResult {
                affected_rows: 1,
                ..
            })
        ));
    }

    #[test]
    fn checked_writes_allow_leading_comments() {
        let mut adapter = adapter();
        let CommandExecutionResult::Ok(inserted) = adapter
            .execute_query(
                "/* leading comment */ INSERT INTO result_values (id, payload) VALUES (3, 'kept')",
            )
            .unwrap()
        else {
            panic!("INSERT must produce an OK result");
        };
        assert_eq!(inserted.affected_rows, 1);

        let CommandExecutionResult::Ok(deleted) = adapter
            .execute_query("-- leading comment\nDELETE FROM result_values")
            .unwrap()
        else {
            panic!("DELETE must produce an OK result");
        };
        assert_eq!(deleted.affected_rows, 3);
    }

    #[test]
    fn metadata_type_survives_all_null_result() {
        let mut adapter = adapter();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT payload FROM result_values WHERE payload IS NULL")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };

        assert_eq!(result.rows, vec![vec![None]]);
        assert_eq!(result.columns[0].column_type, MYSQL_TYPE_BLOB);

        let CommandExecutionResult::ResultSet(empty) = adapter
            .execute_query("SELECT payload FROM result_values WHERE id IS NULL")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert!(empty.rows.is_empty());
        assert_eq!(empty.columns[0].column_type, MYSQL_TYPE_BLOB);
    }

    #[test]
    fn literal_metadata_has_stable_mysql_types_and_collations() {
        let mut adapter = adapter();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT 1 AS i, 'x' AS t, TRUE AS b, NULL AS n")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };

        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (column.column_type, column.character_set))
                .collect::<Vec<_>>(),
            vec![
                (MYSQL_TYPE_LONGLONG, MYSQL_BINARY_COLLATION),
                (MYSQL_TYPE_VAR_STRING, u16::from(DEFAULT_UTF8MB4_COLLATION)),
                (MYSQL_TYPE_LONGLONG, MYSQL_BINARY_COLLATION),
                (MYSQL_TYPE_NULL, MYSQL_BINARY_COLLATION),
            ]
        );
    }

    #[test]
    fn static_literal_metadata_matches_oracle_for_text_prepare_and_empty_binary() {
        let sql = "SELECT 0 AS zero, -0 AS negative_zero, +0 AS positive_zero, 1 AS one, -1 AS neg_one, 0001 AS leading_zero, -0001 AS negative_leading_zero, +0001 AS positive_leading_zero, 9223372036854775807 AS max_i64, -9223372036854775808 AS min_i64, NULL AS null_value, TRUE AS true_value, FALSE AS false_value, +1 AS positive_sign LIMIT 0";
        let integer_metadata = |column_length| {
            (
                MYSQL_TYPE_LONGLONG,
                MYSQL_BINARY_COLLATION,
                column_length,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
                0,
            )
        };
        let expected = [
            integer_metadata(2),
            integer_metadata(2),
            integer_metadata(2),
            integer_metadata(2),
            integer_metadata(2),
            integer_metadata(5),
            integer_metadata(5),
            integer_metadata(5),
            integer_metadata(20),
            integer_metadata(20),
            (MYSQL_TYPE_NULL, MYSQL_BINARY_COLLATION, 0, MYSQL_BINARY_FLAG, 0),
            integer_metadata(1),
            integer_metadata(1),
            integer_metadata(2),
        ];
        let metadata = |columns: &[ColumnDefinitionConfig]| {
            columns
                .iter()
                .map(|column| {
                    (
                        column.column_type,
                        column.character_set,
                        column.column_length,
                        column.flags,
                        column.decimals,
                    )
                })
                .collect::<Vec<_>>()
        };

        let mut adapter = adapter();
        let CommandExecutionResult::ResultSet(text) = adapter.execute_query(sql).unwrap() else {
            panic!("static literal query must produce a result set");
        };
        assert!(text.rows.is_empty());
        assert_eq!(metadata(&text.columns), expected);

        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(metadata(&prepared.columns), expected);
        let binary = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert!(binary.rows.is_empty());
        assert_eq!(metadata(&binary.columns), expected);
    }

    #[test]
    fn static_literal_metadata_survives_wildcard_expansion() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute("CREATE TABLE wildcard_metadata (id INT, label TEXT)")
            .unwrap();
        let sql = "SELECT *, 0001 AS literal_value FROM wildcard_metadata LIMIT 0";
        let expected = (
            MYSQL_TYPE_LONGLONG,
            MYSQL_BINARY_COLLATION,
            5,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            0,
        );

        let CommandExecutionResult::ResultSet(text) = adapter.execute_query(sql).unwrap() else {
            panic!("wildcard SELECT must produce a result set");
        };
        assert!(text.rows.is_empty());
        assert_eq!(
            (
                text.columns[2].column_type,
                text.columns[2].character_set,
                text.columns[2].column_length,
                text.columns[2].flags,
                text.columns[2].decimals,
            ),
            expected
        );

        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(
            (
                prepared.columns[2].column_type,
                prepared.columns[2].character_set,
                prepared.columns[2].column_length,
                prepared.columns[2].flags,
                prepared.columns[2].decimals,
            ),
            expected
        );
        let binary = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert!(binary.rows.is_empty());
        assert_eq!(
            (
                binary.columns[2].column_type,
                binary.columns[2].character_set,
                binary.columns[2].column_length,
                binary.columns[2].flags,
                binary.columns[2].decimals,
            ),
            expected
        );
    }

    #[test]
    fn multiple_wildcards_fall_back_without_metadata_index_panic() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute("CREATE TABLE multiple_wildcards (id INT, label TEXT)")
            .unwrap();
        let sql = "SELECT *, * FROM multiple_wildcards LIMIT 0";

        let CommandExecutionResult::ResultSet(text) = adapter.execute_query(sql).unwrap() else {
            panic!("multiple-wildcard SELECT must produce a result set");
        };
        assert!(text.rows.is_empty());
        assert_eq!(text.columns.len(), 4);

        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(prepared.columns.len(), 4);
        let binary = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert!(binary.rows.is_empty());
        assert_eq!(binary.columns.len(), 4);
    }

    #[test]
    fn static_literal_metadata_survives_prepared_reprepare() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute("CREATE TABLE reprepare_metadata (id INT)")
            .unwrap();
        let sql = "SELECT *, 0001 AS literal_value FROM reprepare_metadata LIMIT 0";
        let expected = (
            MYSQL_TYPE_LONGLONG,
            MYSQL_BINARY_COLLATION,
            5,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            0,
        );
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        adapter
            .connection
            .execute("ALTER TABLE reprepare_metadata ADD COLUMN ignored TEXT")
            .unwrap();
        let result = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert!(result.rows.is_empty());
        assert_eq!(result.columns.len(), 3);
        assert_eq!(
            (
                result.columns[2].column_type,
                result.columns[2].character_set,
                result.columns[2].column_length,
                result.columns[2].flags,
                result.columns[2].decimals,
            ),
            expected
        );
    }

    #[test]
    fn static_literal_metadata_survives_prepared_reprepare_with_rows() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute("CREATE TABLE reprepare_rows (id INT)")
            .unwrap();
        adapter
            .connection
            .execute("INSERT INTO reprepare_rows (id) VALUES (7)")
            .unwrap();
        let sql = "SELECT *, 0001 AS literal_value FROM reprepare_rows";
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        adapter
            .connection
            .execute("ALTER TABLE reprepare_rows ADD COLUMN ignored TEXT")
            .unwrap();
        let result = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        assert_eq!(result.columns.len(), 3);
        assert_eq!(
            result.rows,
            vec![vec![
                BinaryResultValue::Integer(7),
                BinaryResultValue::Null,
                BinaryResultValue::Integer(1),
            ]]
        );
        assert_eq!(result.columns[2].column_length, 5);
        assert_eq!(result.columns[2].flags, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
    }

    #[test]
    fn declared_integer_text_metadata_preserves_mysql_wire_widths() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute(
                "CREATE TABLE text_integer_widths (tiny TINYINT, small SMALLINT, int_value INT, integer_value INTEGER, big BIGINT)",
            )
            .unwrap();
        adapter
            .connection
            .execute(
                "INSERT INTO text_integer_widths (tiny, small, int_value, integer_value, big) VALUES (-128, -32768, -2147483648, -2147483648, -9223372036854775808), (127, 32767, 2147483647, 2147483647, 9223372036854775807), (NULL, NULL, NULL, NULL, NULL)",
            )
            .unwrap();

        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(
                "SELECT tiny, small, int_value, integer_value, big FROM text_integer_widths",
            )
            .unwrap()
        else {
            panic!("declared integer query must produce a result set");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (column.column_type, column.column_length))
                .collect::<Vec<_>>(),
            [
                (MYSQL_TYPE_TINY, 4),
                (MYSQL_TYPE_SHORT, 6),
                (MYSQL_TYPE_LONG, 11),
                (MYSQL_TYPE_LONG, 11),
                (MYSQL_TYPE_LONGLONG, 20),
            ]
        );
        assert_eq!(
            result.rows,
            [
                vec![
                    Some(b"-128".to_vec()),
                    Some(b"-32768".to_vec()),
                    Some(b"-2147483648".to_vec()),
                    Some(b"-2147483648".to_vec()),
                    Some(b"-9223372036854775808".to_vec()),
                ],
                vec![
                    Some(b"127".to_vec()),
                    Some(b"32767".to_vec()),
                    Some(b"2147483647".to_vec()),
                    Some(b"2147483647".to_vec()),
                    Some(b"9223372036854775807".to_vec()),
                ],
                vec![None, None, None, None, None],
            ]
        );
    }

    #[test]
    fn mediumint_text_metadata_preserves_boundaries_and_nulls() {
        let mut adapter = adapter();
        adapter
            .connection
            .execute("CREATE TABLE text_mediumint (value MEDIUMINT)")
            .unwrap();
        adapter
            .connection
            .execute("INSERT INTO text_mediumint (value) VALUES (-8388608), (8388607), (NULL)")
            .unwrap();

        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT value FROM text_mediumint")
            .unwrap()
        else {
            panic!("MEDIUMINT query must produce a result set");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (column.column_type, column.column_length))
                .collect::<Vec<_>>(),
            [(MYSQL_TYPE_INT24, 9)]
        );
        assert_eq!(
            result.rows,
            [
                vec![Some(b"-8388608".to_vec())],
                vec![Some(b"8388607".to_vec())],
                vec![None],
            ]
        );
    }

    #[test]
    fn declared_type_metadata_normalizes_case_and_falls_back_for_unknown_types() {
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("tInYiNt"), Some("INTEGER")),
            Some(MYSQL_TYPE_TINY)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("sMaLlInT"), Some("INTEGER")),
            Some(MYSQL_TYPE_SHORT)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("mEdIuMiNt"), Some("INTEGER")),
            Some(MYSQL_TYPE_INT24)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("InTeGeR"), Some("INTEGER")),
            Some(MYSQL_TYPE_LONG)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("iNt"), Some("INTEGER")),
            Some(MYSQL_TYPE_LONG)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("bIgInT"), Some("INTEGER")),
            Some(MYSQL_TYPE_LONGLONG)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("CUSTOM_INTEGER"), Some("INTEGER")),
            Some(MYSQL_TYPE_LONGLONG)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("VARCHAR(32)"), Some("TEXT")),
            Some(MYSQL_TYPE_VAR_STRING)
        );
        assert_eq!(
            mysql_type_for_declared_or_inferred(Some("CUSTOM_INTEGER"), None),
            None
        );
    }

    #[test]
    fn smallint_metadata_uses_mysql_short_type() {
        assert_eq!(mysql_type_for_name("SMALLINT"), Some(MYSQL_TYPE_SHORT));
    }

    #[test]
    fn mediumint_metadata_uses_mysql_int24_type_and_length() {
        assert_eq!(mysql_type_for_name("MEDIUMINT"), Some(MYSQL_TYPE_INT24));
        assert_eq!(
            column_definition("value".to_owned(), MYSQL_TYPE_INT24).column_length,
            9
        );
    }

    #[test]
    fn prepared_integer_name_mapping_distinguishes_declared_and_inferred_integer() {
        assert_eq!(mysql_type_for_name("TINYINT"), Some(MYSQL_TYPE_TINY));
        assert_eq!(mysql_type_for_name("INT"), Some(MYSQL_TYPE_LONG));
        assert_eq!(mysql_type_for_name("INTEGER"), Some(MYSQL_TYPE_LONGLONG));
        assert_eq!(mysql_type_for_name("BIGINT"), Some(MYSQL_TYPE_LONGLONG));
        let adapter = adapter();
        adapter
            .connection
            .execute("CREATE TABLE integer_sources (integer_value INTEGER)")
            .unwrap();
        let metadata = adapter
            .connection
            .prepare_checked_statement(
                "SELECT integer_value, 1 AS literal_value FROM integer_sources",
            )
            .unwrap();
        let type_metadata = adapter
            .connection
            .prepared_statement_result_column_type_metadata(metadata.statement_id)
            .unwrap();
        assert_eq!(
            mysql_type_for_prepared_column(&metadata.result_columns[0], &type_metadata[0]),
            Some(MYSQL_TYPE_LONG)
        );
        assert_eq!(
            mysql_type_for_prepared_column(&metadata.result_columns[1], &type_metadata[1]),
            Some(MYSQL_TYPE_LONGLONG)
        );
    }

    #[test]
    fn bigint_metadata_uses_mysql_integer_type() {
        assert_eq!(mysql_type_for_name("BIGINT"), Some(MYSQL_TYPE_LONGLONG));
    }

    #[test]
    fn unsupported_query_and_init_db_are_typed_denials() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.execute_query("INSERT INTO users VALUES (1)"),
            Err(FrontendErrorKind::Unsupported)
        );
        assert_eq!(
            adapter.execute_init_db("users"),
            Err(FrontendErrorKind::Unsupported)
        );
        assert_eq!(
            adapter.execute_query("SELECT ?"),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_adapter_selects_with_init_db_and_requires_a_selection_for_query() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([7; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        assert_eq!(
            adapter.execute_query("SELECT id FROM records"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        assert_eq!(
            adapter.execute_init_db("REPORTS"),
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        );

        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT id, label FROM records")
            .unwrap()
        else {
            panic!("SELECT must produce a result set");
        };
        assert_eq!(
            result.rows,
            vec![vec![Some(b"7".to_vec()), Some(b"kept".to_vec())]]
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_adapter_serves_bootstrap_without_database_or_query_authorization() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .with_bootstrap_settings(8192, Duration::from_millis(500))
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([30; 32]),
            ))
            .unwrap();

        adapter.authorize_connection().unwrap();
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT @@max_allowed_packet,@@wait_timeout")
            .unwrap()
        else {
            panic!("driver bootstrap query must produce a result set");
        };
        assert_eq!(
            result.rows,
            vec![vec![Some(b"8192".to_vec()), Some(b"1".to_vec())]]
        );
        assert_eq!(result.columns.len(), 2);
        assert!(result
            .columns
            .iter()
            .all(|column| column.column_type == MYSQL_TYPE_LONGLONG));
        assert_eq!(
            authorizer.actions(),
            vec![RecordedDatabaseAction::Connect(None)]
        );

        assert_eq!(
            adapter.execute_query("SELECT 1"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        assert_eq!(
            authorizer.actions(),
            vec![RecordedDatabaseAction::Connect(None)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_unknown_system_variables_remain_unsupported_after_selection() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([31; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_query("SELECT @@socket,@@wait_timeout"),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorization_hides_existing_and_missing_databases_before_catalog_lookup() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Unavailable),
        ]));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([8; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_init_db("reports"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            adapter.execute_init_db("missing"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Connect(Some("missing".to_owned())),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_init_db_keeps_the_previous_database_selected() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([9; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_init_db("missing"),
            Err(FrontendErrorKind::UnknownDatabase)
        );
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn denied_database_switch_keeps_the_previous_database_selected() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Ok(()),
        ]));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([13; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_init_db("archive"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Connect(Some("archive".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn every_query_is_reauthorized_after_database_selection() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
        ]));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([10; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        assert!(matches!(
            adapter.execute_query("SELECT 1"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert_eq!(
            adapter.execute_query("SELECT 1"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn admin_queries_authorize_canonical_names_before_typed_execution() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([14; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("CREATE DATABASE Archive;"),
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        );
        assert_eq!(
            adapter.execute_query("USE ARCHIVE"),
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        );
        assert_eq!(
            adapter.execute_query("SHOW DATABASES"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![database_list_column()],
                rows: vec![
                    vec![Some(b"archive".to_vec())],
                    vec![Some(b"reports".to_vec())],
                ],
                warnings: 0,
                status_flags: 0x0002,
            }))
        );
        assert_eq!(
            adapter.execute_query("DROP DATABASE ARCHIVE"),
            Err(FrontendErrorKind::DatabaseBusy)
        );
        assert_eq!(
            adapter.execute_query("DROP DATABASE REPORTS"),
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        );
        assert_eq!(catalog.list().unwrap(), vec![String::from("archive")]);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Create(String::from("archive")),
                RecordedDatabaseAction::Connect(Some(String::from("archive"))),
                RecordedDatabaseAction::List,
                RecordedDatabaseAction::Drop(String::from("archive")),
                RecordedDatabaseAction::Drop(String::from("reports")),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn admin_authorization_hides_existence_and_preserves_catalog_state() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Unavailable),
            Err(AuthorizationError::Denied),
        ]));
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([15; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("CREATE DATABASE REPORTS"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            adapter.execute_query("DROP DATABASE MISSING"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            adapter.execute_query("SHOW DATABASES"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(catalog.list().unwrap(), vec![String::from("reports")]);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Create(String::from("reports")),
                RecordedDatabaseAction::Drop(String::from("missing")),
                RecordedDatabaseAction::List,
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_admin_catalog_errors_keep_their_typed_categories() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([21; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        adapter.execute_query("CREATE DATABASE Archive").unwrap();
        assert_eq!(
            adapter.execute_query("CREATE DATABASE ARCHIVE"),
            Err(FrontendErrorKind::DuplicateDatabase)
        );
        assert_eq!(
            adapter.execute_query("DROP DATABASE MISSING"),
            Err(FrontendErrorKind::UnknownDatabase)
        );
        assert_eq!(
            adapter.execute_query("USE MISSING"),
            Err(FrontendErrorKind::UnknownDatabase)
        );
        assert_eq!(
            catalog.list().unwrap(),
            vec![String::from("archive"), String::from("reports")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn sql_use_denial_keeps_the_previous_database_selected() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Ok(()),
        ]));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([16; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        adapter.execute_query("USE REPORTS").unwrap();
        assert_eq!(
            adapter.execute_query("USE MISSING"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some(String::from("reports"))),
                RecordedDatabaseAction::Connect(Some(String::from("missing"))),
                RecordedDatabaseAction::Query(String::from("reports")),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_drop_does_not_reveal_that_the_selected_database_is_busy() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Ok(()),
        ]));
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([22; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE REPORTS").unwrap();

        assert_eq!(
            adapter.execute_query("DROP DATABASE REPORTS"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert_eq!(catalog.list().unwrap(), vec![String::from("reports")]);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some(String::from("reports"))),
                RecordedDatabaseAction::Drop(String::from("reports")),
                RecordedDatabaseAction::Query(String::from("reports")),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn malformed_admin_is_syntax_but_other_admin_sql_is_unsupported() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([17; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("CREATE DATABASE"),
            Err(FrontendErrorKind::Syntax)
        );
        assert_eq!(
            adapter.execute_query("SHOW DATABASES trailing"),
            Err(FrontendErrorKind::Syntax)
        );
        assert_eq!(
            adapter.execute_query("CREATE TABLE records (id INT)"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        adapter.execute_query("USE REPORTS").unwrap();
        assert_eq!(
            adapter.execute_query("SHOW COLUMNS"),
            Err(FrontendErrorKind::Syntax)
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_requires_selection_and_reauthorizes_the_selected_database() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([35; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("SHOW COLUMNS FROM records"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        assert_eq!(
            authorizer.actions(),
            vec![RecordedDatabaseAction::Connect(None)]
        );

        adapter.execute_query("USE REPORTS").unwrap();
        assert_eq!(
            adapter.execute_query("SHOW COLUMNS FROM RECORDS;"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: show_columns_columns(),
                rows: vec![
                    vec![
                        Some(b"id".to_vec()),
                        Some(b"int".to_vec()),
                        Some(b"YES".to_vec()),
                        Some(Vec::new()),
                        None,
                        Some(Vec::new()),
                    ],
                    vec![
                        Some(b"label".to_vec()),
                        Some(b"text".to_vec()),
                        Some(b"YES".to_vec()),
                        Some(Vec::new()),
                        None,
                        Some(Vec::new()),
                    ],
                ],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );

        assert_eq!(
            adapter.execute_query("DESCRIBE records"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: show_columns_columns(),
                rows: vec![
                    vec![
                        Some(b"id".to_vec()),
                        Some(b"int".to_vec()),
                        Some(b"YES".to_vec()),
                        Some(Vec::new()),
                        None,
                        Some(Vec::new()),
                    ],
                    vec![
                        Some(b"label".to_vec()),
                        Some(b"text".to_vec()),
                        Some(b"YES".to_vec()),
                        Some(Vec::new()),
                        None,
                        Some(Vec::new()),
                    ],
                ],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_requires_query_or_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
            [Err(AuthorizationError::Denied)],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([36; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();

        assert_eq!(
            adapter.execute_query("SHOW COLUMNS FROM records"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_and_describe_fall_back_to_granted_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [
                Ok(()),
                Ok(()),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
            ],
            [Ok(()), Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([46; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for sql in ["SHOW COLUMNS FROM RECORDS", "DESCRIBE records"] {
            assert!(matches!(
                adapter.execute_query(sql),
                Ok(CommandExecutionResult::ResultSet(_))
            ));
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_and_describe_direct_view_preserve_source_nullability() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [
                Ok(()),
                Ok(()),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
            ],
            [Ok(()), Ok(())],
        ));
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("reports").unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE TABLE strict_records (id INT NOT NULL, label TEXT)")
            .unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE VIEW strict_records_view AS SELECT id, label FROM strict_records")
            .unwrap();
        drop(seed);

        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([48; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        let expected = |rows| {
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: show_columns_columns(),
                rows,
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        };
        for sql in [
            "SHOW COLUMNS FROM strict_records_view",
            "DESCRIBE strict_records_view",
        ] {
            assert_eq!(
                adapter.execute_query(sql),
                expected(vec![
                    vec![
                        Some(b"id".to_vec()),
                        Some(b"int".to_vec()),
                        Some(b"NO".to_vec()),
                        Some(Vec::new()),
                        None,
                        Some(Vec::new()),
                    ],
                    vec![
                        Some(b"label".to_vec()),
                        Some(b"text".to_vec()),
                        Some(b"YES".to_vec()),
                        Some(Vec::new()),
                        None,
                        Some(Vec::new()),
                    ],
                ])
            );
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "strict_records_view".to_owned(),
                },
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "strict_records_view".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_show_columns_authorization_does_not_try_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
            [Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([47; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_query("SHOW COLUMNS FROM records"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_encodes_typed_default_values() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([37; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();
        adapter
            .execute_query(
                "CREATE TABLE metadata (id INT NOT NULL UNIQUE DEFAULT 1, name TEXT DEFAULT 'guest', payload BLOB, tiny TINYINT, small SMALLINT, maybe INT DEFAULT NULL)",
            )
            .unwrap();
        let columns = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("metadata").unwrap())
            .unwrap();
        let result =
            show_columns_result_to_execution_result(columns, SERVER_STATUS_AUTOCOMMIT).unwrap();

        let CommandExecutionResult::ResultSet(result) = result else {
            panic!("SHOW COLUMNS must produce a result set");
        };
        assert_eq!(result.columns, show_columns_columns());
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Some(b"id".to_vec()),
                    Some(b"int".to_vec()),
                    Some(b"NO".to_vec()),
                    Some(b"UNI".to_vec()),
                    Some(b"1".to_vec()),
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"name".to_vec()),
                    Some(b"text".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    Some(b"guest".to_vec()),
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"payload".to_vec()),
                    Some(b"blob".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"tiny".to_vec()),
                    Some(b"tinyint".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"small".to_vec()),
                    Some(b"smallint".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"maybe".to_vec()),
                    Some(b"int".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
            ]
        );
        assert_eq!(
            show_column_default_value(Some(&MySqlColumnDefault::Boolean(true))),
            Ok(Some(b"1".to_vec()))
        );
        assert_eq!(
            show_column_default_value(Some(&MySqlColumnDefault::Boolean(false))),
            Ok(Some(b"0".to_vec()))
        );
        assert_eq!(
            show_column_default_value(Some(&MySqlColumnDefault::Integer {
                text: "+42".to_owned(),
                value: 42,
            })),
            Ok(Some(b"42".to_vec()))
        );
        assert_eq!(
            show_column_default_value(Some(&MySqlColumnDefault::Text("it's".to_owned()))),
            Ok(Some(b"it's".to_vec()))
        );
        assert_eq!(
            show_column_default_value(Some(&MySqlColumnDefault::Null)),
            Ok(None)
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_reports_mediumint_as_lowercase_type_name() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([41; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();
        adapter
            .execute_query("CREATE TABLE medium_columns (value MEDIUMINT NULL)")
            .unwrap();

        assert_eq!(
            adapter.execute_query("SHOW COLUMNS FROM medium_columns"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: show_columns_columns(),
                rows: vec![vec![
                    Some(b"value".to_vec()),
                    Some(b"mediumint".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ]],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_encodes_primary_and_auto_increment_metadata() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([40; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();
        adapter
            .execute_query(
                "CREATE TABLE key_metadata (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
            )
            .unwrap();

        assert_eq!(
            adapter.execute_query("SHOW COLUMNS FROM key_metadata"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: show_columns_columns(),
                rows: vec![
                    vec![
                        Some(b"id".to_vec()),
                        Some(b"int".to_vec()),
                        Some(b"NO".to_vec()),
                        Some(b"PRI".to_vec()),
                        None,
                        Some(b"auto_increment".to_vec()),
                    ],
                    vec![
                        Some(b"label".to_vec()),
                        Some(b"text".to_vec()),
                        Some(b"YES".to_vec()),
                        Some(Vec::new()),
                        None,
                        Some(Vec::new()),
                    ],
                ],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
        assert_eq!(show_column_extra(""), Ok(b"".as_slice()));
        assert_eq!(
            show_column_extra("AUTO_INCREMENT"),
            Ok(b"auto_increment".as_slice())
        );
        assert_eq!(
            show_column_extra("unexpected"),
            Err(FrontendErrorKind::Internal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_maps_metadata_failures_to_safe_frontend_categories() {
        assert_eq!(
            column_metadata_error_kind(MySqlColumnMetadataError::TableNotFound),
            FrontendErrorKind::MissingObject
        );
        assert_eq!(
            column_metadata_error_kind(MySqlColumnMetadataError::UnsupportedDefinition),
            FrontendErrorKind::Unsupported
        );
        assert_eq!(
            column_metadata_error_kind(MySqlColumnMetadataError::CorruptDefinition),
            FrontendErrorKind::Internal
        );
        assert_eq!(
            column_metadata_error_kind(MySqlColumnMetadataError::Engine(LimboError::TooBig)),
            FrontendErrorKind::Internal
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_has_bounded_protocol_result() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([38; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();
        let codec = PacketCodec::new(4096).unwrap();
        let mut payload = vec![COM_QUERY];
        payload.extend_from_slice(b"SHOW COLUMNS FROM records");
        let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
        let mut connection = ready_connection();

        let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
        assert_eq!(
            frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
        );
        assert_eq!(
            crate::ColumnCountPacket::decode(codec, &frames[0])
                .unwrap()
                .column_count,
            6
        );
        let definitions = (1..=6)
            .map(|index| crate::ColumnDefinitionPacket::decode(codec, &frames[index]).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            definitions
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["Field", "Type", "Null", "Key", "Default", "Extra"]
        );
        assert!(definitions.iter().all(|column| {
            column.column_type == MYSQL_TYPE_VAR_STRING
                && column.character_set == u16::from(DEFAULT_UTF8MB4_COLLATION)
        }));

        for index in [7, 10] {
            assert!(matches!(
                crate::ResultTerminatorPacket::decode(
                    codec,
                    &frames[index],
                    REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
                )
                .unwrap(),
                crate::ResultTerminatorPacket::Eof(_)
            ));
        }
        let first_row = crate::TextRowPacket::decode(codec, &frames[8], 6).unwrap();
        assert_eq!(first_row.values[0], TextRowValue::Bytes(b"id"));
        assert_eq!(first_row.values[1], TextRowValue::Bytes(b"int"));
        assert_eq!(first_row.values[2], TextRowValue::Bytes(b"YES"));
        assert_eq!(first_row.values[3], TextRowValue::Bytes(b""));
        assert_eq!(first_row.values[4], TextRowValue::Null);
        assert_eq!(first_row.values[5], TextRowValue::Bytes(b""));
        let second_row = crate::TextRowPacket::decode(codec, &frames[9], 6).unwrap();
        assert_eq!(second_row.values[0], TextRowValue::Bytes(b"label"));
        assert_eq!(second_row.values[1], TextRowValue::Bytes(b"text"));
        assert_eq!(second_row.values[2], TextRowValue::Bytes(b"YES"));
        assert_eq!(second_row.values[3], TextRowValue::Bytes(b""));
        assert_eq!(second_row.values[4], TextRowValue::Null);
        assert_eq!(second_row.values[5], TextRowValue::Bytes(b""));
    }

    #[cfg(unix)]
    #[test]
    fn show_columns_rejects_unencodable_results_before_dispatch() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([39; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();

        adapter
            .execute_query("CREATE TABLE bounded (value TEXT)")
            .unwrap();
        let bounded = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("bounded").unwrap())
            .unwrap();
        assert_eq!(
            show_columns_result_to_execution_result(
                vec![bounded[0].clone(); MAX_DISPATCH_RESULT_ROWS + 1],
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );

        let oversized_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH);
        adapter
            .execute_query(&format!(
                "CREATE TABLE oversized_default (value TEXT DEFAULT '{oversized_default}')"
            ))
            .unwrap();
        let oversized_default = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("oversized_default").unwrap())
            .unwrap();
        assert_eq!(
            show_columns_result_to_execution_result(oversized_default, SERVER_STATUS_AUTOCOMMIT,),
            Err(FrontendErrorKind::Internal)
        );

        let packet_bound_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH - 19);
        adapter
            .execute_query(&format!(
                "CREATE TABLE packet_bound (value TEXT DEFAULT '{packet_bound_default}')"
            ))
            .unwrap();
        let packet_bound = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("packet_bound").unwrap())
            .unwrap();
        assert_eq!(
            show_columns_result_to_execution_result(packet_bound, SERVER_STATUS_AUTOCOMMIT),
            Err(FrontendErrorKind::Internal)
        );

        let long_name = "x".repeat(2_000);
        adapter
            .execute_query(&format!("CREATE TABLE retained (`{long_name}` TEXT)"))
            .unwrap();
        let retained = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("retained").unwrap())
            .unwrap();
        assert_eq!(
            show_columns_result_to_execution_result(
                vec![retained[0].clone(); MAX_DISPATCH_RESULT_ROWS],
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_full_tables_filters_grants_and_drop_view_requires_query_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [
                Ok(()),
                Ok(()),
                Err(AuthorizationError::Denied),
                Err(AuthorizationError::Denied),
            ],
            [Err(AuthorizationError::Denied), Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([82; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        adapter
            .session
            .connection()
            .unwrap()
            .execute("CREATE VIEW alpha AS SELECT id FROM records")
            .unwrap();
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SHOW FULL TABLES").unwrap()
        else {
            panic!("SHOW must return rows");
        };
        assert_eq!(
            result.rows,
            vec![vec![
                Some(b"records".to_vec()),
                Some(b"BASE TABLE".to_vec())
            ]]
        );
        assert_eq!(
            adapter.execute_query("DROP VIEW alpha"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            adapter
                .session
                .connection()
                .unwrap()
                .list_tables()
                .unwrap()
                .len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn drop_table_requires_query_permission_without_table_select_fallback() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [
                Ok(()),
                Ok(()),
                Err(AuthorizationError::Denied),
            ],
            [Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([83; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_query("DROP TABLE records"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
        assert_eq!(
            adapter
                .session
                .connection()
                .unwrap()
                .list_tables()
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_full_tables_has_typed_bounded_metadata_and_requires_selection() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([81; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        assert_eq!(
            adapter.execute_query("SHOW FULL TABLES"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        adapter.execute_query("SET sql_notes = 0").unwrap();
        adapter.execute_query("USE reports").unwrap();
        let CommandExecutionResult::ResultSet(notes) =
            adapter.execute_query("SELECT @@sql_notes").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(notes.rows, vec![vec![Some(b"0".to_vec())]]);
        adapter
            .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
            .unwrap();
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SHOW FULL TABLES").unwrap()
        else {
            panic!("SHOW must return rows");
        };
        assert_eq!(
            result.rows,
            vec![
                vec![Some(b"records".to_vec()), Some(b"BASE TABLE".to_vec())],
                vec![Some(b"records_view".to_vec()), Some(b"VIEW".to_vec())]
            ]
        );
        assert_eq!(result.columns[0].name, "Tables_in_reports");
        assert_eq!(result.columns[1].name, "Table_type");
        assert_eq!(result.columns[1].column_type, MYSQL_TYPE_STRING);
        assert_eq!(result.columns[1].column_length, 44);
        assert_eq!(
            result.columns[1].flags,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG
        );
        assert_eq!(result.columns[1].catalog, "def");
        assert_eq!(result.columns[1].table, "TABLES");
        assert_eq!(result.columns[1].original_table, "tables");
        let tables = adapter.session.connection().unwrap().list_tables().unwrap();
        assert_eq!(
            show_full_tables_result_to_execution_result(
                "reports",
                vec![tables[0].clone(); MAX_DISPATCH_RESULT_ROWS + 1],
                SERVER_STATUS_AUTOCOMMIT
            ),
            Err(FrontendErrorKind::Internal)
        );
        adapter.execute_reset_connection().unwrap();
        let CommandExecutionResult::ResultSet(notes) =
            adapter.execute_query("SELECT @@sql_notes").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(notes.rows, vec![vec![Some(b"1".to_vec())]]);
    }

    #[cfg(unix)]
    #[test]
    fn show_tables_requires_a_selection_and_reauthorizes_the_selected_database() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([32; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("SHOW TABLES"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        assert_eq!(
            authorizer.actions(),
            vec![RecordedDatabaseAction::Connect(None)]
        );

        adapter.execute_query("USE REPORTS").unwrap();
        assert_eq!(
            adapter.execute_query("SHOW TABLES;"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![show_tables_column("reports")],
                rows: vec![vec![Some(b"records".to_vec())]],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_tables_requires_selection_and_returns_sorted_user_objects() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([41; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        let query = "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME";
        assert_eq!(
            adapter.execute_query(query),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        adapter.execute_init_db("REPORTS").unwrap();
        let connection = adapter.session.connection().unwrap();
        connection.execute("CREATE TABLE zeta (id INT)").unwrap();
        connection
            .execute("CREATE VIEW alpha AS SELECT id FROM records")
            .unwrap();

        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap()
        else {
            panic!("information_schema.TABLES must return a result set");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.original_name.as_str(),
                    column.table.as_str(),
                    column.original_table.as_str(),
                    column.schema.as_str(),
                    column.catalog.as_str(),
                    column.column_type,
                    column.character_set,
                    column.column_length,
                    column.flags,
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "TABLE_SCHEMA",
                    "TABLE_SCHEMA",
                    "TABLES",
                    "schemata",
                    "information_schema",
                    "def",
                    MYSQL_TYPE_VAR_STRING,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    256,
                    MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                ),
                (
                    "TABLE_NAME",
                    "TABLE_NAME",
                    "TABLES",
                    "tables",
                    "information_schema",
                    "def",
                    MYSQL_TYPE_VAR_STRING,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    256,
                    MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                ),
                (
                    "TABLE_TYPE",
                    "TABLE_TYPE",
                    "TABLES",
                    "tables",
                    "information_schema",
                    "def",
                    MYSQL_TYPE_STRING,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    44,
                    MYSQL_NOT_NULL_FLAG
                        | MYSQL_BINARY_FLAG
                        | MYSQL_ENUM_FLAG
                        | MYSQL_NO_DEFAULT_VALUE_FLAG,
                ),
            ]
        );
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Some(b"reports".to_vec()),
                    Some(b"alpha".to_vec()),
                    Some(b"VIEW".to_vec()),
                ],
                vec![
                    Some(b"reports".to_vec()),
                    Some(b"records".to_vec()),
                    Some(b"BASE TABLE".to_vec()),
                ],
                vec![
                    Some(b"reports".to_vec()),
                    Some(b"zeta".to_vec()),
                    Some(b"BASE TABLE".to_vec()),
                ],
            ]
        );
        assert_eq!(result.warnings, 0);
        assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_index_returns_the_fifteen_columns_mysql_returns() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([46; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        assert_eq!(
            adapter.execute_query("SHOW INDEX FROM records"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        adapter.execute_init_db("reports").unwrap();

        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SHOW INDEX FROM records").unwrap()
        else {
            panic!("SHOW INDEX must return a result set");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            [
                "Table",
                "Non_unique",
                "Key_name",
                "Seq_in_index",
                "Column_name",
                "Collation",
                "Cardinality",
                "Sub_part",
                "Packed",
                "Null",
                "Index_type",
                "Comment",
                "Index_comment",
                "Visible",
                "Expression",
            ]
        );
        for row in &result.rows {
            assert_eq!(row.len(), 15);
            assert_eq!(row[0], Some(b"records".to_vec()));
            assert_eq!(row[5], Some(b"A".to_vec()));
            // Cardinality is a statistic Turso does not gather, and MySQL sends
            // NULL when it has none either.
            assert_eq!(row[6], None);
            assert_eq!(row[10], Some(b"BTREE".to_vec()));
            assert_eq!(row[13], Some(b"YES".to_vec()));
            assert_eq!(row[14], None);
        }

        // Every spelling reaches the same place, and the other catalog
        // commands still answer for themselves.
        for sql in ["SHOW KEYS FROM records", "SHOW INDEXES IN records"] {
            assert_eq!(
                adapter.execute_query(sql).unwrap(),
                adapter.execute_query("SHOW INDEX FROM records").unwrap(),
                "{sql}"
            );
        }
        assert_eq!(
            adapter.execute_query("SHOW INDEX FROM missing"),
            Err(FrontendErrorKind::MissingObject)
        );
        assert_eq!(
            adapter.execute_query("SHOW INDEX FROM archive.records"),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_create_table_needs_a_selection_and_returns_the_mysql_ddl() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([43; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("SHOW CREATE TABLE records"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        adapter.execute_init_db("reports").unwrap();

        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SHOW CREATE TABLE records").unwrap()
        else {
            panic!("SHOW CREATE TABLE must return a result set");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.column_type,
                    column.character_set,
                    column.column_length,
                    column.decimals,
                    column.flags,
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "Table",
                    MYSQL_TYPE_VAR_STRING,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    256,
                    31,
                    MYSQL_NOT_NULL_FLAG,
                ),
                (
                    "Create Table",
                    MYSQL_TYPE_VAR_STRING,
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    4096,
                    31,
                    MYSQL_NOT_NULL_FLAG,
                ),
            ]
        );
        let [row] = result.rows.as_slice() else {
            panic!("SHOW CREATE TABLE must return exactly one row");
        };
        assert_eq!(row[0], Some(b"records".to_vec()));
        assert_eq!(
            String::from_utf8(row[1].clone().unwrap()).unwrap(),
            concat!(
                "CREATE TABLE `records` (\n",
                "  `id` int DEFAULT NULL,\n",
                "  `label` text\n",
                ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            )
        );
        assert_eq!(
            adapter.execute_query("SHOW CREATE TABLE missing"),
            Err(FrontendErrorKind::MissingObject)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_qualifier_naming_the_selected_database_is_taken_and_any_other_is_refused() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([45; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        // The qualifier clients write right after USE is redundant, and MySQL
        // answers it exactly as it answers the unqualified form.
        let qualified = adapter
            .execute_query("SHOW CREATE TABLE reports.records")
            .unwrap();
        let plain = adapter.execute_query("SHOW CREATE TABLE records").unwrap();
        assert_eq!(qualified, plain);
        assert_eq!(
            adapter.execute_query("SHOW CREATE TABLE REPORTS.records"),
            Ok(plain)
        );

        for sql in [
            "SHOW CREATE TABLE archive.records",
            "SHOW COLUMNS FROM archive.records",
            "DESCRIBE archive.records",
        ] {
            assert_eq!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported),
                "{sql}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn show_create_table_authorizes_before_catalog_lookup() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
            [Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([44; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_query("SHOW CREATE TABLE records"),
            Err(FrontendErrorKind::AccessDenied)
        );
        // The catalog was never read: the run stops at the denied Query.
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_tables_authorizes_before_catalog_lookup() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
            [Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([42; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_query(
                "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME"
            ),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_tables_filters_rows_by_granted_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
            [Err(AuthorizationError::Denied), Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([48; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        adapter
            .session
            .connection()
            .unwrap()
            .execute("CREATE TABLE alpha (id INT)")
            .unwrap();

        let query = "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME";
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap()
        else {
            panic!("information_schema.TABLES must return a result set");
        };
        assert_eq!(
            result.rows,
            vec![vec![
                Some(b"reports".to_vec()),
                Some(b"records".to_vec()),
                Some(b"BASE TABLE".to_vec()),
            ]]
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "alpha".to_owned(),
                },
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_tables_rejects_malformed_queries_without_falling_through() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([43; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for query in [
            "SELECT * FROM information_schema.TABLES",
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_SCHEMA",
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME DESC",
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME; SELECT 1",
        ] {
            assert_eq!(
                adapter.execute_query(query),
                Err(FrontendErrorKind::Syntax),
                "malformed information_schema.TABLES query must not execute as a normal SELECT: {query}"
            );
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_returns_exact_metadata_and_rows() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        catalog.create("metadata").unwrap();
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("metadata").unwrap();
        seed.connection()
            .unwrap()
            .execute_schema_ddl(
                "CREATE TABLE records (id INT NOT NULL, label TEXT, value MEDIUMINT NULL)",
            )
            .unwrap();
        drop(seed);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([50; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("metadata").unwrap();

        let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap()
        else {
            panic!("information_schema.COLUMNS must return a result set");
        };
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.catalog.as_str(),
                    column.schema.as_str(),
                    column.table.as_str(),
                    column.original_table.as_str(),
                    column.name.as_str(),
                    column.original_name.as_str(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "def",
                    "information_schema",
                    "COLUMNS",
                    "",
                    "COLUMN_NAME",
                    "COLUMN_NAME",
                ),
                (
                    "def",
                    "information_schema",
                    "COLUMNS",
                    "columns",
                    "ORDINAL_POSITION",
                    "ORDINAL_POSITION",
                ),
                (
                    "def",
                    "information_schema",
                    "COLUMNS",
                    "columns",
                    "COLUMN_DEFAULT",
                    "COLUMN_DEFAULT",
                ),
                (
                    "def",
                    "information_schema",
                    "COLUMNS",
                    "",
                    "IS_NULLABLE",
                    "IS_NULLABLE",
                ),
                (
                    "def",
                    "information_schema",
                    "COLUMNS",
                    "columns",
                    "COLUMN_TYPE",
                    "COLUMN_TYPE",
                ),
                (
                    "def",
                    "information_schema",
                    "COLUMNS",
                    "columns",
                    "COLUMN_KEY",
                    "COLUMN_KEY",
                ),
                ("def", "information_schema", "COLUMNS", "", "EXTRA", "EXTRA",),
            ]
        );
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.character_set,
                    column.column_length,
                    column.column_type,
                    column.flags,
                    column.decimals,
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "COLUMN_NAME",
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    256,
                    MYSQL_TYPE_VAR_STRING,
                    0,
                    0,
                ),
                (
                    "ORDINAL_POSITION",
                    MYSQL_BINARY_COLLATION,
                    10,
                    MYSQL_TYPE_LONG,
                    MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                    0,
                ),
                (
                    "COLUMN_DEFAULT",
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    262_140,
                    MYSQL_TYPE_BLOB,
                    MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
                    0,
                ),
                (
                    "IS_NULLABLE",
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    12,
                    MYSQL_TYPE_VAR_STRING,
                    MYSQL_NOT_NULL_FLAG,
                    0,
                ),
                (
                    "COLUMN_TYPE",
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    67_108_860,
                    MYSQL_TYPE_BLOB,
                    MYSQL_NOT_NULL_FLAG
                        | MYSQL_BLOB_FLAG
                        | MYSQL_BINARY_FLAG
                        | MYSQL_NO_DEFAULT_VALUE_FLAG,
                    0,
                ),
                (
                    "COLUMN_KEY",
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    12,
                    MYSQL_TYPE_STRING,
                    MYSQL_NOT_NULL_FLAG
                        | MYSQL_BINARY_FLAG
                        | MYSQL_ENUM_FLAG
                        | MYSQL_NO_DEFAULT_VALUE_FLAG,
                    0,
                ),
                (
                    "EXTRA",
                    u16::from(DEFAULT_UTF8MB4_COLLATION),
                    1024,
                    MYSQL_TYPE_VAR_STRING,
                    0,
                    0,
                ),
            ]
        );
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Some(b"id".to_vec()),
                    Some(b"1".to_vec()),
                    None,
                    Some(b"NO".to_vec()),
                    Some(b"int".to_vec()),
                    Some(Vec::new()),
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"label".to_vec()),
                    Some(b"2".to_vec()),
                    None,
                    Some(b"YES".to_vec()),
                    Some(b"text".to_vec()),
                    Some(Vec::new()),
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"value".to_vec()),
                    Some(b"3".to_vec()),
                    None,
                    Some(b"YES".to_vec()),
                    Some(b"mediumint".to_vec()),
                    Some(Vec::new()),
                    Some(Vec::new()),
                ],
            ]
        );
        let codec = PacketCodec::new(4096).unwrap();
        for (index, column) in result.columns.iter().enumerate() {
            let frame = column.encode(codec, (index + 1) as u8).unwrap();
            let decoded = crate::ColumnDefinitionPacket::decode(codec, &frame).unwrap();
            assert_eq!(
                (
                    decoded.sequence_id,
                    decoded.catalog.as_str(),
                    decoded.schema.as_str(),
                    decoded.table.as_str(),
                    decoded.original_table.as_str(),
                    decoded.name.as_str(),
                    decoded.original_name.as_str(),
                    decoded.character_set,
                    decoded.column_length,
                    decoded.column_type,
                    decoded.flags,
                    decoded.decimals,
                ),
                (
                    (index + 1) as u8,
                    column.catalog.as_str(),
                    column.schema.as_str(),
                    column.table.as_str(),
                    column.original_table.as_str(),
                    column.name.as_str(),
                    column.original_name.as_str(),
                    column.character_set,
                    column.column_length,
                    column.column_type,
                    column.flags,
                    column.decimals,
                )
            );
        }
        assert_eq!(result.warnings, 0);
        assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("metadata".to_owned())),
                RecordedDatabaseAction::Query("metadata".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_returns_the_requested_table_or_view() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("reports").unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE TABLE other (id BIGINT NOT NULL, note TEXT)")
            .unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE VIEW records_view AS SELECT id FROM records")
            .unwrap();
        drop(seed);

        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([58; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for (table, columns) in [
            ("other", ["id", "note"].as_slice()),
            ("records_view", ["id"].as_slice()),
            ("missing", &[] as &[&str]),
        ] {
            let query = format!(
                "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
            );
            let CommandExecutionResult::ResultSet(result) = adapter.execute_query(&query).unwrap()
            else {
                panic!("information_schema.COLUMNS must return a result set");
            };
            assert_eq!(result.rows.len(), columns.len());
            for (row, column) in result.rows.iter().zip(columns) {
                assert_eq!(row[0], Some(column.as_bytes().to_vec()));
            }
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_binds_lookup_to_the_selected_database() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        catalog.create("archive").unwrap();
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("archive").unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE TABLE records (archived_id INT NOT NULL)")
            .unwrap();
        drop(seed);

        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([60; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        for (database, columns) in [
            ("reports", ["id", "label"].as_slice()),
            ("archive", ["archived_id"].as_slice()),
        ] {
            adapter.execute_init_db(database).unwrap();
            let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
            let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap()
            else {
                panic!("information_schema.COLUMNS must return a result set");
            };
            assert_eq!(result.rows.len(), columns.len());
            for (row, column) in result.rows.iter().zip(columns) {
                assert_eq!(row[0], Some(column.as_bytes().to_vec()));
            }
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Connect(Some("archive".to_owned())),
                RecordedDatabaseAction::Query("archive".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_uses_granted_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
            [Ok(())],
        ));
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("reports").unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE TABLE other (id BIGINT NOT NULL, note TEXT)")
            .unwrap();
        drop(seed);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([51; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'other' ORDER BY ORDINAL_POSITION";
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap()
        else {
            panic!("information_schema.COLUMNS must return a result set");
        };
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Some(b"id".to_vec()));
        assert_eq!(result.rows[1][0], Some(b"note".to_vec()));
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "other".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_denied_table_returns_empty_result() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
            [Err(AuthorizationError::Denied)],
        ));
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        let mut seed = catalog.new_session(binary_context());
        seed.select_database("reports").unwrap();
        seed.connection()
            .unwrap()
            .execute("CREATE TABLE other (id BIGINT NOT NULL, note TEXT)")
            .unwrap();
        drop(seed);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([52; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'other' ORDER BY ORDINAL_POSITION";
        assert_eq!(
            adapter.execute_query(query),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: information_schema_columns_columns(),
                rows: Vec::new(),
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "other".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_unavailable_authorization_precedes_lookup() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
            [Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([53; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
        assert_eq!(
            adapter.execute_query(query),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_missing_records_returns_empty_rows() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        catalog.create("archive").unwrap();
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([54; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("archive").unwrap();

        let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
        assert_eq!(
            adapter.execute_query(query),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: information_schema_columns_columns(),
                rows: Vec::new(),
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_rejects_unencodable_results_before_dispatch() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([57; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        adapter
            .execute_query("CREATE TABLE bounded (value TEXT)")
            .unwrap();
        let bounded = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("bounded").unwrap())
            .unwrap();
        assert_eq!(
            information_schema_columns_result_to_execution_result(
                vec![bounded[0].clone(); MAX_DISPATCH_RESULT_ROWS + 1],
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );

        let oversized_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH + 1);
        adapter
            .execute_query(&format!(
                "CREATE TABLE oversized_default (value TEXT DEFAULT '{oversized_default}')"
            ))
            .unwrap();
        let oversized_default = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("oversized_default").unwrap())
            .unwrap();
        assert_eq!(
            information_schema_columns_result_to_execution_result(
                oversized_default,
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );

        let packet_bound_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH - 19);
        adapter
            .execute_query(&format!(
                "CREATE TABLE packet_bound (value TEXT DEFAULT '{packet_bound_default}')"
            ))
            .unwrap();
        let packet_bound = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("packet_bound").unwrap())
            .unwrap();
        assert_eq!(
            information_schema_columns_result_to_execution_result(
                packet_bound,
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );

        let long_name = "x".repeat(2_000);
        adapter
            .execute_query(&format!("CREATE TABLE retained (`{long_name}` TEXT)"))
            .unwrap();
        let retained = adapter
            .session
            .connection()
            .unwrap()
            .list_columns(&MySqlTableName::parse("retained").unwrap())
            .unwrap();
        assert_eq!(
            information_schema_columns_result_to_execution_result(
                vec![retained[0].clone(); MAX_DISPATCH_RESULT_ROWS],
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_rejects_malformed_queries_without_fallthrough() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([55; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for query in [
            "SELECT * FROM information_schema.COLUMNS",
            "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records'",
            "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY COLUMN_NAME",
            "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION DESC",
            "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION; SELECT 1",
        ] {
            assert_eq!(
                adapter.execute_query(query),
                Err(FrontendErrorKind::Syntax),
                "malformed information_schema.COLUMNS query must fail closed: {query}"
            );
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_keeps_prepare_fail_closed() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([56; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
        assert!(matches!(
            adapter.execute_stmt_prepare(query),
            Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
        ));
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn internal_catalog_selects_fail_closed_without_table_grant_fallback() {
        let mut decisions = vec![Ok(()), Ok(())];
        decisions.extend(std::iter::repeat_with(|| Err(AuthorizationError::Denied)).take(6));
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            decisions,
            vec![Ok(()); 6],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([44; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for query in [
            "SELECT name FROM sqlite_schema",
            "SELECT name FROM sqlite_master",
            "SELECT name FROM sqlite_sequence",
            "SELECT name FROM __turso_internal_types",
            "SELECT name FROM `SQLite_Schema`",
            "/* hidden */ SELECT name FROM sqlite_schema",
        ] {
            assert_eq!(
                adapter.execute_query(query),
                Err(FrontendErrorKind::AccessDenied),
                "internal catalog query must be rejected before authorization fallback: {query}"
            );
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_columns_hides_internal_tables() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([59; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();

        for table in ["sqlite_schema", "sqlite_master", "__turso_internal_types"] {
            let query = format!(
                "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
            );
            let CommandExecutionResult::ResultSet(result) = adapter.execute_query(&query).unwrap()
            else {
                panic!("information_schema.COLUMNS must return a result set");
            };
            assert!(result.rows.is_empty(), "internal table leaked: {table}");
        }
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_tables_rejects_results_over_dispatch_bounds() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([45; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        let tables = adapter.session.connection().unwrap().list_tables().unwrap();

        assert_eq!(
            information_schema_tables_result_to_execution_result(
                &"x".repeat(MAX_TEXT_ROW_VALUE_LENGTH + 1),
                tables.clone(),
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );
        assert_eq!(
            information_schema_tables_result_to_execution_result(
                &"x".repeat(MAX_TEXT_ROW_VALUE_LENGTH - 19),
                tables.clone(),
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );

        assert_eq!(
            information_schema_tables_result_to_execution_result(
                "reports",
                tables
                    .iter()
                    .cloned()
                    .cycle()
                    .take(MAX_DISPATCH_RESULT_ROWS + 1),
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_tables_requires_query_or_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
            [Err(AuthorizationError::Denied)],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([34; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();

        assert_eq!(
            adapter.execute_query("SHOW TABLES"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![show_tables_column("reports")],
                rows: Vec::new(),
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            }))
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_tables_filters_rows_by_granted_table_permission() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
            [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
            [Err(AuthorizationError::Denied), Ok(())],
        ));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([49; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("reports").unwrap();
        adapter
            .session
            .connection()
            .unwrap()
            .execute("CREATE TABLE alpha (id INT)")
            .unwrap();

        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SHOW TABLES").unwrap()
        else {
            panic!("SHOW TABLES must return a result set");
        };
        assert_eq!(result.rows, vec![vec![Some(b"records".to_vec())]]);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "alpha".to_owned(),
                },
                RecordedDatabaseAction::TableSelect {
                    database: "reports".to_owned(),
                    table: "records".to_owned(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_databases_has_bounded_protocol_result() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([18; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("CREATE DATABASE Archive").unwrap();
        let codec = PacketCodec::new(4096).unwrap();
        let mut payload = vec![COM_QUERY];
        payload.extend_from_slice(b"SHOW DATABASES");
        let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
        let mut connection = ready_connection();

        let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
        assert_eq!(
            frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            crate::ColumnCountPacket::decode(codec, &frames[0])
                .unwrap()
                .column_count,
            1
        );
        let column = crate::ColumnDefinitionPacket::decode(codec, &frames[1]).unwrap();
        assert_eq!(column.name, "Database");
        assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
        assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
        assert_eq!(column.column_length, 64);
        let first_row = crate::TextRowPacket::decode(codec, &frames[3], 1).unwrap();
        assert!(matches!(first_row.values[0], TextRowValue::Bytes(value) if value == b"archive"));
        let second_row = crate::TextRowPacket::decode(codec, &frames[4], 1).unwrap();
        assert!(matches!(second_row.values[0], TextRowValue::Bytes(value) if value == b"reports"));
        assert!(matches!(
            crate::ResultTerminatorPacket::decode(
                codec,
                &frames[2],
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )
            .unwrap(),
            crate::ResultTerminatorPacket::Eof(_)
        ));
        assert!(matches!(
            crate::ResultTerminatorPacket::decode(
                codec,
                &frames[5],
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )
            .unwrap(),
            crate::ResultTerminatorPacket::Eof(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn show_tables_has_bounded_protocol_result() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([33; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_query("USE reports").unwrap();
        let codec = PacketCodec::new(4096).unwrap();
        let mut payload = vec![COM_QUERY];
        payload.extend_from_slice(b"SHOW TABLES");
        let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
        let mut connection = ready_connection();

        let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
        assert_eq!(
            frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        assert_eq!(
            crate::ColumnCountPacket::decode(codec, &frames[0])
                .unwrap()
                .column_count,
            1
        );
        let column = crate::ColumnDefinitionPacket::decode(codec, &frames[1]).unwrap();
        assert_eq!(column.name, "Tables_in_reports");
        assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
        assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
        assert_eq!(column.column_length, 256);
        let row = crate::TextRowPacket::decode(codec, &frames[3], 1).unwrap();
        assert!(matches!(row.values[0], TextRowValue::Bytes(value) if value == b"records"));
        assert!(matches!(
            crate::ResultTerminatorPacket::decode(
                codec,
                &frames[2],
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )
            .unwrap(),
            crate::ResultTerminatorPacket::Eof(_)
        ));
        assert!(matches!(
            crate::ResultTerminatorPacket::decode(
                codec,
                &frames[4],
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )
            .unwrap(),
            crate::ResultTerminatorPacket::Eof(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn show_databases_returns_an_empty_result_without_a_selection() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        catalog.drop_database("reports").unwrap();
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([19; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("SHOW DATABASES"),
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![database_list_column()],
                rows: Vec::new(),
                warnings: 0,
                status_flags: 0x0002,
            }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_schemata_lists_databases_without_a_selection() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
        catalog.create("Archive").unwrap();
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([60; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA")
            .unwrap()
        else {
            panic!("information_schema.SCHEMATA must return a result set");
        };
        assert_eq!(result.columns, vec![information_schema_schemata_column()]);
        assert_eq!(
            result.rows,
            vec![
                vec![Some(b"archive".to_vec())],
                vec![Some(b"reports".to_vec())],
            ]
        );
        assert_eq!(result.warnings, 0);
        assert_eq!(result.status_flags, 0x0002);
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::List
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_schemata_reuses_list_authorization_and_bounds() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Err(AuthorizationError::Denied),
        ]));
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([61; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        assert_eq!(
            adapter.execute_query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA"),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::List
            ]
        );
        assert_eq!(
            information_schema_schemata_result_to_execution_result(vec![
                String::new();
                MAX_DISPATCH_RESULT_ROWS
                    + 1
            ]),
            Err(FrontendErrorKind::Internal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn information_schema_schemata_rejects_malformed_queries_without_fallthrough() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([62; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();

        for query in [
            "SELECT * FROM information_schema.SCHEMATA",
            "SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME FROM information_schema.SCHEMATA",
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'reports'",
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA LIMIT 1",
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA; SELECT 1",
        ] {
            assert_eq!(
                adapter.execute_query(query),
                Err(FrontendErrorKind::Syntax),
                "malformed information_schema.SCHEMATA query must fail closed: {query}"
            );
        }
        assert_eq!(
            authorizer.actions(),
            vec![RecordedDatabaseAction::Connect(None)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_databases_rejects_more_rows_than_the_dispatcher_can_encode() {
        assert_eq!(
            admin_result_to_execution_result(MySqlAdminCommandResult::Listed {
                databases: vec![String::new(); MAX_DISPATCH_RESULT_ROWS + 1],
            }),
            Err(FrontendErrorKind::Internal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_tables_rejects_unencodable_results_before_dispatch() {
        assert_eq!(
            show_tables_result_to_execution_result(
                "reports",
                vec![String::new(); MAX_DISPATCH_RESULT_ROWS + 1],
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );
        assert_eq!(
            show_tables_result_to_execution_result(
                "reports",
                vec!["x".repeat(MAX_TEXT_ROW_VALUE_LENGTH + 1)],
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );
        assert_eq!(
            show_tables_result_to_execution_result(
                "reports",
                vec![
                    "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH);
                    (MAX_FRONTEND_ADAPTER_RESULT_BYTES / MAX_TEXT_ROW_VALUE_LENGTH) + 1
                ],
                SERVER_STATUS_AUTOCOMMIT,
            ),
            Err(FrontendErrorKind::Internal)
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_and_unavailable_admin_actions_are_fixed_access_denied_packets() {
        let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Unavailable),
        ]));
        let (_directory, catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([20; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        let codec = PacketCodec::new(4096).unwrap();
        let mut connection = ready_connection();

        let mut error_frames = Vec::new();
        for sql in ["CREATE DATABASE REPORTS", "DROP DATABASE MISSING"] {
            let mut payload = vec![COM_QUERY];
            payload.extend_from_slice(sql.as_bytes());
            let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
            let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
            assert_eq!(frames.len(), 1);
            let error = crate::ErrPacket::decode(
                codec,
                &frames[0],
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )
            .unwrap();
            assert_eq!(error.sequence_id, 1);
            assert_eq!(error.error_code, 1045);
            assert_eq!(error.sql_state, Some(*b"28000"));
            assert_eq!(error.message, b"access denied");
            assert_eq!(connection.state(), ConnectionState::Ready);
            error_frames.push(frames[0].clone());
        }
        assert_eq!(error_frames[0], error_frames[1]);
        assert_eq!(catalog.list().unwrap(), vec![String::from("reports")]);
    }

    #[cfg(unix)]
    #[test]
    fn factory_passes_the_authenticated_canonical_account_id_to_the_policy() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let expected = AccountId::from_bytes([0xa5; 32]);
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                expected.clone(),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        assert_eq!(
            authorizer.account_ids.lock().unwrap().as_slice(),
            &[expected]
        );
    }

    #[test]
    fn malformed_select_is_a_syntax_category() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.execute_query("SELECT FROM"),
            Err(FrontendErrorKind::Syntax)
        );
    }

    #[test]
    fn core_prepare_errors_are_not_guessed_to_be_syntax_errors() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.execute_query("SELECT id FROM missing_table"),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[test]
    fn result_collection_stops_at_the_dispatcher_row_limit() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.execute_query("SELECT id FROM many_rows"),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    #[test]
    fn aggregate_row_payload_is_rejected_before_values_are_copied() {
        let mut adapter = adapter();
        assert_eq!(
            adapter.execute_query("SELECT left_value, right_value FROM wide_values"),
            Err(FrontendErrorKind::Unsupported)
        );
    }

    fn ready_connection() -> ClassicConnection {
        let codec = PacketCodec::new(4096).unwrap();
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ClassicConnection::with_test_nonce(
            InitialHandshakeSettings {
                capability_flags: capabilities,
                ..InitialHandshakeSettings::default()
            },
            codec,
            TransportSecurity::Secure,
            [0xa5; crate::AUTH_PLUGIN_DATA_LENGTH],
        )
        .unwrap();
        connection.send_initial_handshake().unwrap();
        let response = ClientHandshakeResponseConfig::new(
            capabilities,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(codec, 1)
        .unwrap();
        connection
            .receive_client_handshake_frame(&response)
            .unwrap();
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        connection.send_authentication_ok().unwrap();
        assert_eq!(connection.state(), ConnectionState::Ready);
        connection
    }

    #[test]
    fn adapter_runs_through_dispatcher_with_protocol_sequences() {
        let mut connection = ready_connection();
        let mut adapter = adapter();
        let codec = PacketCodec::new(4096).unwrap();
        let mut command_payload = vec![COM_QUERY];
        command_payload.extend_from_slice(b"SELECT id, payload FROM result_values");
        let command = codec.encode(COMMAND_SEQUENCE_ID, &command_payload).unwrap();

        let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
        assert_eq!(
            frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(
            crate::ColumnCountPacket::decode(codec, &frames[0])
                .unwrap()
                .column_count,
            2
        );
        let id_definition = crate::ColumnDefinitionPacket::decode(codec, &frames[1]).unwrap();
        assert_eq!(id_definition.column_type, MYSQL_TYPE_LONG);
        assert_eq!(id_definition.character_set, MYSQL_BINARY_COLLATION);
        let payload_definition = crate::ColumnDefinitionPacket::decode(codec, &frames[2]).unwrap();
        assert_eq!(payload_definition.column_type, MYSQL_TYPE_BLOB);
        assert_eq!(payload_definition.character_set, MYSQL_BINARY_COLLATION);
        let first_row = crate::TextRowPacket::decode(codec, &frames[4], 2).unwrap();
        assert!(matches!(first_row.values[0], TextRowValue::Bytes(value) if value == b"1"));
        assert!(matches!(first_row.values[1], TextRowValue::Bytes(value) if value == [0, 0xff]));
        let second_row = crate::TextRowPacket::decode(codec, &frames[5], 2).unwrap();
        assert!(matches!(second_row.values[0], TextRowValue::Bytes(value) if value == b"2"));
        assert!(matches!(second_row.values[1], TextRowValue::Null));
    }

    #[cfg(unix)]
    #[test]
    fn catalog_adapter_runs_init_db_through_the_dispatcher() {
        let mut connection = ready_connection();
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer);
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([11; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        let codec = PacketCodec::new(4096).unwrap();
        let mut command_payload = vec![COM_INIT_DB];
        command_payload.extend_from_slice(b"REPORTS");
        let command = codec.encode(COMMAND_SEQUENCE_ID, &command_payload).unwrap();

        let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            crate::OkPacket::decode(codec, &frames[0])
                .unwrap()
                .sequence_id,
            1
        );
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[cfg(unix)]
    #[test]
    fn catalog_adapter_selects_the_handshake_database_before_authentication_ok() {
        let authorizer = Arc::new(RecordingAuthorizer::default());
        let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
        let mut adapter = factory
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes([12; 32]),
            ))
            .unwrap();
        adapter.authorize_connection().unwrap();
        let codec = PacketCodec::new(4096).unwrap();
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_CONNECT_WITH_DB;
        let mut connection = ClassicConnection::with_test_nonce(
            InitialHandshakeSettings {
                capability_flags: capabilities,
                ..InitialHandshakeSettings::default()
            },
            codec,
            TransportSecurity::Secure,
            [0xa5; crate::AUTH_PLUGIN_DATA_LENGTH],
        )
        .unwrap();
        connection.send_initial_handshake().unwrap();
        let response = ClientHandshakeResponseConfig::new(
            capabilities,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0; 32],
            Some("REPORTS"),
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(codec, 1)
        .unwrap();
        connection
            .receive_client_handshake_frame(&response)
            .unwrap();
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();

        let AuthenticationResponse::Ok(frame) = connection
            .send_authentication_ok_with_selector(&mut adapter)
            .unwrap()
        else {
            panic!("known initial database must produce authentication OK");
        };
        assert_eq!(
            crate::AuthOkPacket::decode(codec, &frame)
                .unwrap()
                .sequence_id,
            3
        );
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
        assert_eq!(
            authorizer.actions(),
            vec![
                RecordedDatabaseAction::Connect(None),
                RecordedDatabaseAction::Connect(Some("reports".to_owned())),
                RecordedDatabaseAction::Query("reports".to_owned()),
            ]
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }
}
