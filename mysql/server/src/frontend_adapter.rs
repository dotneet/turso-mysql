//! Adapter from the bounded MySQL frontend to the transport-neutral server.
//!
//! The dependency points from the protocol crate to the frontend crate.  The
//! frontend does not depend on this crate, so this keeps the execution boundary
//! one-way while allowing a server owner to opt into the checked SELECT slice.

#[cfg(unix)]
mod catalog_results;

#[cfg(unix)]
use catalog_results::{
    admin_result_to_execution_result, information_schema_columns_result_to_execution_result,
    information_schema_schemata_result_to_execution_result,
    information_schema_tables_result_to_execution_result, reject_other_database_qualifier,
    show_columns_result_to_execution_result, show_create_table_error_kind,
    show_create_table_result_to_execution_result, show_full_tables_result_to_execution_result,
    show_index_result_to_execution_result, show_tables_result_to_execution_result,
};

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
    parse_optional_show_errors, parse_optional_show_warnings, parse_select,
    MySqlDriverBootstrapQuery, SessionSqlMode,
};
#[cfg(unix)]
use turso_mysql_parser::{
    parse_optional_describe, parse_optional_information_schema_columns,
    parse_optional_information_schema_schemata, parse_optional_information_schema_tables,
    parse_optional_show_columns, parse_optional_show_create_table, parse_optional_show_full_tables,
    parse_optional_create_table_with_keys, parse_optional_show_index, parse_optional_show_tables,
    ArithmeticOperand, ArithmeticOperator, ArithmeticShape, ColumnAggregateKind, MySqlDatabaseName,
    MySqlSelectSource, MySqlShowCommand, MySqlTableName,
    ScalarFunction,
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
    /// What the last statement warned about, which `SHOW WARNINGS` reports.
    raised_warnings: Vec<MySqlWarning>,
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
    source_tables: Vec<MySqlSelectSource>,
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
            raised_warnings: Vec::new(),
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
            self.connection.parser_mode(),
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
        if let Some(command) = parse_optional_show_warnings(sql, self.connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            // MySQL keeps the warnings until the next statement that can raise
            // one, so reading them does not clear them.
            return Ok(show_warnings_result(
                &self.raised_warnings,
                status_flags,
                command.offset(),
                command.row_count(),
            ));
        }
        if let Some(command) = parse_optional_show_errors(sql, self.connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(show_errors_result(
                &self.raised_warnings,
                status_flags,
                command.offset(),
                command.row_count(),
            ));
        }
        self.raised_warnings.clear();
        execute_checked_query(
            &self.connection,
            sql,
            None,
            &[],
            CheckedQueryOptions {
                query_timeout: None,
                affected_rows_mode: MySqlAffectedRowsMode::Changed,
                sql_notes: self.session_variables.sql_notes(),
                raised: &mut self.raised_warnings,
            },
        )
    }

    fn execute_reset_connection(&mut self) -> Result<(), FrontendErrorKind> {
        self.connection
            .reset_connection()
            .map_err(frontend_query_error)?;
        self.prepared_types.clear();
        self.session_variables = crate::session_variables::MySqlSessionVariables::default();
        self.raised_warnings.clear();
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
            raised_warnings: Vec::new(),
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
    /// What the last statement warned about, which `SHOW WARNINGS` reports.
    raised_warnings: Vec<MySqlWarning>,
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
    ) -> Result<Vec<MySqlSelectSource>, FrontendErrorKind> {
        let source_tables = parsed_source_tables(sql);
        match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::Query { database })
        {
            Ok(()) => {
                if source_tables
                    .iter()
                    .any(|source| is_internal_catalog_table(source.table().as_str()))
                {
                    return Err(FrontendErrorKind::Unsupported);
                }
                Ok(source_tables)
            }
            Err(AuthorizationError::Denied) => {
                // A join reads every table it names, so a grant on one of them
                // is not a grant on the statement.
                if source_tables.is_empty() {
                    return Err(FrontendErrorKind::AccessDenied);
                }
                for source in &source_tables {
                    let table = source.table().as_str();
                    if is_internal_catalog_table(table) {
                        return Err(FrontendErrorKind::AccessDenied);
                    }
                    self.authorize_table_select(database, table)?;
                }
                Ok(source_tables)
            }
            Err(error) => Err(authorization_frontend_error(error)),
        }
    }

    fn authorize_prepared_query(
        &self,
        database: &str,
        source_tables: &[MySqlSelectSource],
    ) -> Result<(), FrontendErrorKind> {
        match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::Query { database })
        {
            Ok(()) => Ok(()),
            Err(AuthorizationError::Denied) => {
                if source_tables.is_empty() {
                    return Err(FrontendErrorKind::AccessDenied);
                }
                for source in source_tables {
                    self.authorize_table_select(database, source.table().as_str())?;
                }
                Ok(())
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
            self.session.session_sql_mode(),
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
        if let Some(command) = parse_optional_show_warnings(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(show_warnings_result(
                &self.raised_warnings,
                status_flags,
                command.offset(),
                command.row_count(),
            ));
        }
        if let Some(command) = parse_optional_show_errors(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(show_errors_result(
                &self.raised_warnings,
                status_flags,
                command.offset(),
                command.row_count(),
            ));
        }
        let source_tables = self.authorize_query_text(&selected_database, sql)?;
        self.raised_warnings.clear();
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
            &source_tables,
            CheckedQueryOptions {
                query_timeout: self.query_timeout,
                affected_rows_mode,
                sql_notes: self.session_variables.sql_notes(),
                raised: &mut self.raised_warnings,
            },
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
        self.raised_warnings.clear();
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
        let source_tables = self.authorize_query_text(&selected_database, sql)?;
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
            &source_tables,
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
                source_tables,
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
        let (database, source_tables) = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .map(|statement| (statement.database.clone(), statement.source_tables.clone()))
            .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
        self.authorize_prepared_query(&database, &source_tables)?;
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
    parse_select(sql, SessionSqlMode::default()).is_ok_and(|translated| {
        translated
            .source_tables()
            .iter()
            .any(|source| is_internal_catalog_table(source.table().as_str()))
    })
}

#[cfg(unix)]
fn parsed_source_tables(sql: &str) -> Vec<MySqlSelectSource> {
    parse_select(sql, SessionSqlMode::default())
        .map(|translated| translated.source_tables().to_vec())
        .unwrap_or_default()
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

/// How one checked statement is run, and where what it warns about is kept.
struct CheckedQueryOptions<'a> {
    query_timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
    sql_notes: bool,
    /// Every warning the statement raises, so a later `SHOW WARNINGS` can
    /// report it.
    raised: &'a mut Vec<MySqlWarning>,
}

fn execute_checked_query(
    connection: &MySqlConnection,
    sql: &str,
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
    options: CheckedQueryOptions<'_>,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let CheckedQueryOptions {
        query_timeout,
        affected_rows_mode,
        sql_notes,
        raised,
    } = options;
    let sql = strip_leading_sql_comments(sql);
    if let Some(command) = parse_optional_drop_table(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Syntax)?
    {
        let result = connection.drop_table(&command).map_err(|error| match error {
            MySqlDropTableError::MissingTable => FrontendErrorKind::UnknownTable,
            MySqlDropTableError::Engine(error) => frontend_error_kind(error),
        })?;
        let noted = !result.dropped && sql_notes;
        if noted {
            raised.push(MySqlWarning::unknown_table(
                selected_database,
                command.table().as_str(),
            ));
        }
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            warnings: u16::from(noted),
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
            source_tables,
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
    let result = prepared_statement_result(connection, metadata, &type_metadata, None, &[]);
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
        &[],
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
        &statement.source_tables,
    )
}

fn execute_prepared_values(
    connection: &MySqlConnection,
    statement_id: u32,
    values: Vec<MySqlPreparedValue>,
    timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (selected_database, source_tables);
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
        source_tables,
    )?;
    let columns = metadata
        .result_columns
        .into_iter()
        .enumerate()
        .zip(&column_types)
        .map(|((index, column), column_type)| {
            if let Some(metadata) = type_metadata[index].static_metadata() {
                if let Some(definition) = static_column_definition(column.name.clone(), metadata) {
                    return Ok(definition);
                }
                #[cfg(unix)]
                return aggregate_column_definition(
                    source_metadata.as_ref(),
                    column.name,
                    metadata,
                );
                #[cfg(not(unix))]
                return Err(FrontendErrorKind::Unsupported);
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
                .zip(&columns)
                .map(|(value, column)| {
                    binary_result_value(value, column.column_type, column.decimals)
                })
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
    // A DECIMAL crosses as text, and MySQL writes it at the scale the column
    // declared, so the binary protocol needs that scale as much as the text one.
    decimals: u8,
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
        MySqlPreparedValue::Real(value)
            if column_type == MYSQL_TYPE_DOUBLE || column_type == MYSQL_TYPE_FLOAT =>
        {
            Ok(BinaryResultValue::Real(value))
        }
        // A DECIMAL crosses as text whatever the engine holds it as, because
        // that is what MySQL sends for a NEWDECIMAL.
        MySqlPreparedValue::Real(value) if column_type == MYSQL_TYPE_NEWDECIMAL => Ok(
            BinaryResultValue::Text(format!("{:.*}", usize::from(decimals), value)),
        ),
        MySqlPreparedValue::Integer(value) if column_type == MYSQL_TYPE_NEWDECIMAL => Ok(
            BinaryResultValue::Text(format!("{:.*}", usize::from(decimals), value as f64)),
        ),
        // A CHAR and a DECIMAL both cross as length-encoded text, which is
        // what MySQL sends for them.
        MySqlPreparedValue::Text(value)
            if matches!(
                column_type,
                MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING | MYSQL_TYPE_NEWDECIMAL
            ) =>
        {
            Ok(BinaryResultValue::Text(value))
        }
        MySqlPreparedValue::Text(value)
            if matches!(column_type, MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP) =>
        {
            binary_result_datetime(&value)
        }
        MySqlPreparedValue::Blob(value) if column_type == MYSQL_TYPE_BLOB => {
            Ok(BinaryResultValue::Blob(value))
        }
        // A TEXT column reports BLOB, so its value crosses as the same
        // length-encoded bytes.
        MySqlPreparedValue::Text(value) if column_type == MYSQL_TYPE_BLOB => {
            Ok(BinaryResultValue::Blob(value.into_bytes()))
        }
        // The engine answers an integer result as a float only when it left
        // the range an integer can hold, which MySQL answers 1690 for.
        MySqlPreparedValue::Real(_)
            if matches!(
                column_type,
                MYSQL_TYPE_TINY
                    | MYSQL_TYPE_SHORT
                    | MYSQL_TYPE_INT24
                    | MYSQL_TYPE_LONG
                    | MYSQL_TYPE_LONGLONG
            ) =>
        {
            Err(FrontendErrorKind::NumericOverflow)
        }
        _ => Err(FrontendErrorKind::Internal),
    }
}

/// Reads the whole-second form this server stores a DATETIME in.
///
/// The text is the one this frontend wrote, `YYYY-MM-DD HH:MM:SS`, so anything
/// else means the row and the column disagree about the type.
fn binary_result_datetime(value: &str) -> Result<BinaryResultValue, FrontendErrorKind> {
    let (date, time) = value.split_once(' ').ok_or(FrontendErrorKind::Internal)?;
    let [year, month, day] = <[&str; 3]>::try_from(date.split('-').collect::<Vec<_>>())
        .map_err(|_| FrontendErrorKind::Internal)?;
    let [hour, minute, second] = <[&str; 3]>::try_from(time.split(':').collect::<Vec<_>>())
        .map_err(|_| FrontendErrorKind::Internal)?;
    let field = |text: &str| text.parse::<u8>().map_err(|_| FrontendErrorKind::Internal);
    Ok(BinaryResultValue::DateTime {
        year: year.parse().map_err(|_| FrontendErrorKind::Internal)?,
        month: field(month)?,
        day: field(day)?,
        hour: field(hour)?,
        minute: field(minute)?,
        second: field(second)?,
    })
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
    source_tables: &[MySqlSelectSource],
) -> Result<PreparedStatementResult, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (selected_database, source_tables);
    if metadata.result_columns.len() != type_metadata.len() {
        return Err(FrontendErrorKind::Internal);
    }
    let parameters = (0..metadata.parameter_count)
        .map(|index| column_definition(format!("?{}", index + 1), MYSQL_TYPE_NULL))
        .collect();
    #[cfg(unix)]
    let source_metadata = prepared_table_result_metadata(
        connection,
        type_metadata,
        selected_database,
        source_tables,
    )?;
    let columns = metadata
        .result_columns
        .into_iter()
        .zip(type_metadata)
        .map(|(column, type_metadata)| {
            if let Some(metadata) = type_metadata.static_metadata() {
                if let Some(definition) = static_column_definition(column.name.clone(), metadata) {
                    return Ok(definition);
                }
                #[cfg(unix)]
                return aggregate_column_definition(
                    source_metadata.as_ref(),
                    column.name,
                    metadata,
                );
                #[cfg(not(unix))]
                return Err(FrontendErrorKind::Unsupported);
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
    source_tables: &[MySqlSelectSource],
    query_timeout: Option<Duration>,
) -> Result<TextResultSet, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (selected_database, source_tables);
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

    #[cfg(unix)]
    let source_metadata = table_result_metadata(
        connection,
        &statement,
        selected_database,
        source_tables,
        static_result_metadata
            .iter()
            .flatten()
            .any(needs_source_columns),
    )?;

    let columns = (0..column_count)
        .map(|index| {
            let name = statement.get_column_name(index).into_owned();
            match (static_result_metadata.len() == column_count)
                .then(|| static_result_metadata[index].as_ref())
                .flatten()
            {
                Some(metadata) => {
                    if let Some(definition) = static_column_definition(name.clone(), metadata) {
                        return Ok(definition);
                    }
                    #[cfg(unix)]
                    return aggregate_column_definition(source_metadata.as_ref(), name, metadata);
                    #[cfg(not(unix))]
                    return Err(FrontendErrorKind::Unsupported);
                }
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
    let rendering = columns
        .iter()
        .map(TextValueRendering::for_column)
        .collect::<Vec<_>>();

    let mut rows = Vec::new();
    let mut retained_bytes = 0usize;
    let mut overflowed = false;
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
            // MySQL answers 1690 when an integer result leaves BIGINT's range;
            // the engine turns the same sum into a float, which is how this
            // sees it.
            overflowed |= row.get_values().enumerate().any(|(index, value)| {
                rendering[index] == TextValueRendering::Integer
                    && matches!(value, Value::Numeric(Numeric::Float(_)))
            });
            let values = row
                .get_values()
                .enumerate()
                .map(|(index, value)| value_to_text_ref(value, rendering[index]))
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

    if overflowed {
        return Err(FrontendErrorKind::NumericOverflow);
    }
    Ok(TextResultSet {
        columns,
        rows,
        warnings: 0,
        status_flags: 0x0002,
    })
}

/// One table a result column can have come from.
#[cfg(unix)]
struct SourceTableColumns {
    source_table: String,
    table_reference: String,
    columns: Vec<MySqlColumnMetadata>,
    /// The columns a `WITH` name projects, in order, when this reference is a
    /// CTE rather than the table itself. A result column's ordinal counts
    /// through these, not through the table's own columns.
    projected_columns: Vec<String>,
    /// An outer join can leave this table's row missing, which is what takes
    /// the `NOT NULL` flag off its columns.
    outer: bool,
}

#[cfg(unix)]
struct TableResultMetadata {
    database: String,
    tables: Vec<SourceTableColumns>,
    /// A `UNION` reads more than one branch, and its result columns belong to
    /// none of the tables any single branch names.
    union: bool,
}

#[cfg(unix)]
impl SourceTableColumns {
    /// Turns a result column's ordinal into the table column it names.
    ///
    /// A CTE can project its table's columns in any order, so the ordinal
    /// counts through what the CTE projected and the name it lands on is
    /// looked up in the table.
    fn column_ordinal(&self, ordinal: usize) -> Result<usize, FrontendErrorKind> {
        if self.projected_columns.is_empty() {
            return Ok(ordinal);
        }
        let name = self
            .projected_columns
            .get(ordinal)
            .ok_or(FrontendErrorKind::Internal)?;
        self.columns
            .iter()
            .position(|column| column.name().eq_ignore_ascii_case(name))
            .ok_or(FrontendErrorKind::UnknownColumn)
    }
}

#[cfg(unix)]
impl TableResultMetadata {
    /// Returns the table the engine reports a result column against.
    fn table_for(&self, table_reference: &str) -> Option<&SourceTableColumns> {
        // The engine reports the canonical spelling of a name the client may
        // have written in any case, which is how `FROM `RECORDS`` reaches here
        // as `records`.
        self.tables
            .iter()
            .find(|table| table.table_reference.eq_ignore_ascii_case(table_reference))
    }

    /// Finds one column by name across every table this statement reads.
    ///
    /// A join whose tables both carry the name is refused rather than answered
    /// from whichever came first; the parser already requires a qualified name
    /// in a joined projection, so this only sees the aggregate and arithmetic
    /// surfaces, which name a column and no table.
    fn column_named(&self, name: &str) -> Result<(&SourceTableColumns, usize), FrontendErrorKind> {
        let mut found = None;
        for table in &self.tables {
            let Some(ordinal) = table
                .columns
                .iter()
                .position(|column| column.name().eq_ignore_ascii_case(name))
            else {
                continue;
            };
            if found.is_some() {
                return Err(FrontendErrorKind::Unsupported);
            }
            found = Some((table, ordinal));
        }
        found.ok_or(FrontendErrorKind::UnknownColumn)
    }
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
        let table = self
            .table_for(&table_reference)
            .ok_or(FrontendErrorKind::Unsupported)?;
        let ordinal = table.column_ordinal(ordinal)?;
        let source = table
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
        if source.type_name() == "FLOAT" {
            // Measured on MySQL 8.4.11: a FLOAT column reports 12 where a
            // DOUBLE reports 22, both with the not-fixed decimals value.
            definition.column_length = 12;
            definition.decimals = NOT_FIXED_DECIMALS;
        }
        if source.type_name() == "BOOLEAN" {
            // Measured on MySQL 8.4.11: a BOOLEAN column reports 1, the display
            // width in `tinyint(1)`, where a plain TINYINT reports 4.
            definition.column_length = 1;
        }
        if matches!(source.type_name(), "TEXT" | "BLOB") {
            // Measured on MySQL 8.4.11: a TEXT column reports 262140, the four
            // bytes utf8mb4 needs for each of 65,535 characters, and carries
            // the text collation; a BLOB reports 65535 and the binary one.
            let text = source.type_name() == "TEXT";
            definition.column_length = if text { 262_140 } else { 65_535 };
            definition.character_set = if text {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            } else {
                MYSQL_BINARY_COLLATION
            };
        }
        if let Some(length) = source.character_length() {
            // Measured on MySQL 8.4.11: a `VARCHAR(4)` and a `CHAR(4)` both
            // report 16. The declared count is characters, and the reported
            // length reserves the four bytes utf8mb4 needs for one.
            definition.column_length = length.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        }
        definition.schema.clone_from(&self.database);
        definition.table = table_reference;
        definition.original_table.clone_from(&table.source_table);
        source.name().clone_into(&mut definition.original_name);
        set_column_flags(&mut definition, mysql_table_column_flags(source));
        if self.union {
            // Measured on MySQL 8.4.11: a UNION's result column names no table
            // and carries none of the column's key facts. Its NOT NULL is
            // dropped here as well, which MySQL keeps when both branches are
            // NOT NULL — the engine reports only the first branch's column, so
            // this cannot tell, and a column a client believes may be NULL is
            // never wrong.
            definition.schema.clear();
            definition.table.clear();
            definition.original_table.clear();
            definition.original_name.clear();
            set_column_flags(&mut definition, 0);
        }
        if table.outer {
            // Measured on MySQL 8.4.11: a NOT NULL column on the outer side of
            // a LEFT JOIN reports no NOT_NULL flag, because a row with no match
            // answers NULL for it. Its key flags stay.
            definition.flags &= !MYSQL_NOT_NULL_FLAG;
        }
        if matches!(source.type_name(), "DATETIME" | "TIMESTAMP") {
            // Measured: a temporal column carries the binary flag, because it
            // has no collation of its own.
            definition.flags |= MYSQL_BINARY_FLAG;
        }
        Ok(definition)
    }

    /// Builds the result column an aggregate over `column_name` reports.
    ///
    /// The answer belongs to no table, so the column's own table, key and
    /// auto-increment facts are dropped and the result is nullable whatever the
    /// column is — measured on MySQL 8.4.11, an empty table gives NULL. What
    /// each aggregate does with the type is its own rule below.
    fn aggregate_column_definition(
        &self,
        name: String,
        column_name: &str,
        kind: ColumnAggregateKind,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        let (table, ordinal) = self.column_named(column_name)?;
        let source = &table.columns[ordinal];
        let mut definition = self.column_definition_for_reference(
            Some((table.table_reference.clone(), ordinal)),
            name,
            None,
        )?;
        if kind != ColumnAggregateKind::MinMax {
            apply_summing_aggregate_metadata(&mut definition, source, kind)?;
        }
        definition.schema.clear();
        definition.table.clear();
        definition.original_table.clear();
        definition.original_name.clear();
        let aggregate_flags = if matches!(
            definition.column_type,
            MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP
        ) {
            // Measured: a MIN over a text or temporal column reports no flags
            // at all, losing even the BINARY a temporal column carries.
            0
        } else {
            // Measured: a numeric aggregate answers with the binary collation
            // where the plain column does not.
            MYSQL_BINARY_FLAG
        };
        set_column_flags(&mut definition, aggregate_flags);
        Ok(definition)
    }

    /// Builds the result column an integer arithmetic expression reports.
    ///
    /// Measured on MySQL 8.4.11. `+` and `-` give a precision of
    /// `max(left, right) + 1` and `*` gives `left + right`, and the reported
    /// length is that precision plus one for the sign: `1+1` is 3, `i + 1` and
    /// `i * 2` are 12 over an `INT`, `i - b` is 21 against a `BIGINT`, and
    /// `i * 1000000` is 18. A literal's precision is its digit count. `/` is
    /// decimal division: precision is the left operand's plus four, scale is
    /// four, and the length adds one for the sign and one for the point, so
    /// `3/2` is 7 and `i / 2` is 16. Every result carries the binary collation,
    /// and one is NOT NULL only when no operand can be null — a division never
    /// is, because dividing by zero answers NULL.
    fn arithmetic_column_definition(
        source_metadata: Option<&Self>,
        name: String,
        shape: &ArithmeticShape,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        let left = Self::arithmetic_operand_shape(source_metadata, &shape.left)?;
        let right = Self::arithmetic_operand_shape(source_metadata, &shape.right)?;
        let (column_type, precision, scale, not_null) = match shape.operator {
            ArithmeticOperator::Add | ArithmeticOperator::Subtract => (
                MYSQL_TYPE_LONGLONG,
                left.precision.max(right.precision) + 1,
                0,
                left.not_null && right.not_null,
            ),
            ArithmeticOperator::Multiply => (
                MYSQL_TYPE_LONGLONG,
                left.precision + right.precision,
                0,
                left.not_null && right.not_null,
            ),
            ArithmeticOperator::Divide => (MYSQL_TYPE_NEWDECIMAL, left.precision + 4, 4, false),
        };
        if precision > MYSQL_MAX_DECIMAL_PRECISION {
            return Err(FrontendErrorKind::Unsupported);
        }
        let mut definition = column_definition(name, column_type);
        definition.column_length = precision + 1 + u32::from(scale > 0);
        definition.decimals = scale as u8;
        set_column_flags(
            &mut definition,
            MYSQL_BINARY_FLAG | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
        );
        Ok(definition)
    }

    fn arithmetic_operand_shape(
        source_metadata: Option<&Self>,
        operand: &ArithmeticOperand,
    ) -> Result<ArithmeticOperandShape, FrontendErrorKind> {
        match operand {
            ArithmeticOperand::Literal { digit_count } => Ok(ArithmeticOperandShape {
                precision: *digit_count,
                not_null: true,
            }),
            ArithmeticOperand::Column { column_name } => {
                let source_metadata = source_metadata.ok_or(FrontendErrorKind::Unsupported)?;
                let (table, ordinal) = source_metadata.column_named(column_name)?;
                let source = &table.columns[ordinal];
                // Only integers here: MySQL's decimal and float arithmetic
                // carry their own precision and scale rules, unmeasured.
                if source.decimal_size().is_some() {
                    return Err(FrontendErrorKind::Unsupported);
                }
                let (precision, _) =
                    decimal_shape_of(source).ok_or(FrontendErrorKind::Unsupported)?;
                Ok(ArithmeticOperandShape {
                    precision,
                    not_null: !source.nullable() && !table.outer,
                })
            }
            ArithmeticOperand::Nested(shape) => {
                let left = Self::arithmetic_operand_shape(source_metadata, &shape.left)?;
                let right = Self::arithmetic_operand_shape(source_metadata, &shape.right)?;
                let precision = match shape.operator {
                    ArithmeticOperator::Add | ArithmeticOperator::Subtract => {
                        left.precision.max(right.precision) + 1
                    }
                    ArithmeticOperator::Multiply => left.precision + right.precision,
                    // The parser refuses a nested division for this reason.
                    ArithmeticOperator::Divide => return Err(FrontendErrorKind::Internal),
                };
                Ok(ArithmeticOperandShape {
                    precision,
                    not_null: left.not_null && right.not_null,
                })
            }
        }
    }
}

/// The precision and nullability one arithmetic operand contributes.
#[cfg(unix)]
struct ArithmeticOperandShape {
    precision: u32,
    not_null: bool,
}

/// Applies MySQL's `SUM` and `AVG` result rules to an already-typed column.
///
/// Measured on MySQL 8.4.11. A `SUM` widens the argument's decimal precision by
/// 22 and keeps its scale: `SUM` over `TINYINT` (precision 3) reports length 26,
/// `SMALLINT` 28, `MEDIUMINT` 31, `INT` 33, `BIGINT` 42, and `DECIMAL(10,2)` 34
/// with 2 decimals. An `AVG` widens precision by 4 and scale by 4: over
/// `TINYINT` it reports length 9, over `INT` 16, and over `DECIMAL(10,2)` 16
/// with 6 decimals. Over a `DOUBLE` both answer `DOUBLE` with length 23 and 31
/// decimals, which is what a float column carries anyway.
#[cfg(unix)]
fn apply_summing_aggregate_metadata(
    definition: &mut ColumnDefinitionConfig,
    source: &MySqlColumnMetadata,
    kind: ColumnAggregateKind,
) -> Result<(), FrontendErrorKind> {
    if source.type_name() == "DOUBLE" {
        definition.column_type = MYSQL_TYPE_DOUBLE;
        definition.column_length = 23;
        definition.decimals = 31;
        return Ok(());
    }
    // MySQL sums a text or temporal column by coercing it, which this has not
    // measured, so those are refused rather than given a decimal's metadata.
    let (precision, scale) = decimal_shape_of(source).ok_or(FrontendErrorKind::Unsupported)?;
    let (precision, scale) = match kind {
        ColumnAggregateKind::Sum => (precision + 22, scale),
        ColumnAggregateKind::Avg => (precision + 4, scale + 4),
        ColumnAggregateKind::MinMax => unreachable!("MIN and MAX keep the column's own type"),
    };
    definition.column_type = MYSQL_TYPE_NEWDECIMAL;
    definition.column_length = precision + 1 + u32::from(scale > 0);
    definition.decimals = scale as u8;
    Ok(())
}

/// Returns the decimal precision and scale MySQL gives a numeric column.
///
/// The integer precisions are the digit counts of each type's range, measured
/// through the `SUM` lengths above.
#[cfg(unix)]
fn decimal_shape_of(source: &MySqlColumnMetadata) -> Option<(u32, u32)> {
    if let Some((precision, scale)) = source.decimal_size() {
        return Some((precision, scale));
    }
    Some((
        match source.type_name() {
            "TINYINT" | "BOOLEAN" => 3,
            "SMALLINT" => 5,
            "MEDIUMINT" => 8,
            "INT" | "INTEGER" => 10,
            "BIGINT" => 19,
            _ => return None,
        },
        0,
    ))
}

/// Builds the result column a checked scalar call reports.
///
/// Measured on MySQL 8.4.11 over a `VARCHAR(8)`, which reports length 32:
/// `LOWER`, `UPPER` and `TRIM` answer a `VAR_STRING` of that same 32 with the
/// not-fixed decimals value, `LENGTH` and `CHAR_LENGTH` answer a `LONGLONG` of
/// length 10, and `NOW()` answers a `DATETIME` of length 19 that is NOT NULL.
/// Only the last is NOT NULL: the others answer NULL when their column does.
#[cfg(unix)]
fn scalar_call_column_definition(
    source_metadata: Option<&TableResultMetadata>,
    name: String,
    function: ScalarFunction,
    columns: &[String],
    literal_characters: u32,
    not_null: bool,
) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
    if function == ScalarFunction::Now {
        let mut definition = column_definition(name, MYSQL_TYPE_DATETIME);
        definition.column_length = 19;
        set_column_flags(&mut definition, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: as wide as its widest branch, and NOT NULL because every
    // branch is a literal and there is an ELSE.
    if function == ScalarFunction::Branches {
        return Ok(text_call_definition(
            name,
            literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            not_null,
        ));
    }
    let source_metadata = source_metadata.ok_or(FrontendErrorKind::Unsupported)?;
    // Measured: the answer is as wide as its arguments laid end to end, a
    // string literal counting the characters it spells.
    if function == ScalarFunction::Concatenates {
        let mut width = literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        for column_name in columns {
            let (table, ordinal) = source_metadata.column_named(column_name)?;
            let source = &table.columns[ordinal];
            if !is_text_column(source) {
                return Err(FrontendErrorKind::Unsupported);
            }
            let length = source
                .character_length()
                .ok_or(FrontendErrorKind::Unsupported)?;
            width = width.saturating_add(length.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER));
        }
        return Ok(text_call_definition(name, width, not_null));
    }
    let [column_name] = columns else {
        return Err(FrontendErrorKind::Internal);
    };
    let (table, ordinal) = source_metadata.column_named(column_name)?;
    let source = &table.columns[ordinal];
    let wants_text = matches!(
        function,
        ScalarFunction::KeepsTextShape
            | ScalarFunction::CountsText
            | ScalarFunction::TakesCharacters
    );
    // MySQL takes each of these over the other kind by coercing it, which has
    // not been measured, so each is answered only over the kind it is for.
    if wants_text != is_text_column(source) {
        return Err(FrontendErrorKind::Unsupported);
    }
    // Measured: as wide as the count it was asked for, whatever the column is.
    if function == ScalarFunction::TakesCharacters {
        return Ok(text_call_definition(
            name,
            literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            not_null,
        ));
    }
    let own_shape = |name: String| {
        source_metadata.column_definition_for_reference(
            Some((table.table_reference.clone(), ordinal)),
            name,
            None,
        )
    };
    let mut definition = match function {
        ScalarFunction::KeepsTextShape => {
            let mut definition = own_shape(name)?;
            // Measured: the answer is a VAR_STRING whatever the argument was,
            // so a CHAR argument widens and a TEXT one narrows to it.
            definition.column_type = MYSQL_TYPE_VAR_STRING;
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            definition.decimals = NOT_FIXED_DECIMALS;
            definition
        }
        ScalarFunction::CountsText => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 10;
            definition
        }
        // Measured: ABS over an INT answers a LONGLONG of the INT's own length
        // 11, and over a DECIMAL(10,2) a NEWDECIMAL of 12 with its scale — the
        // width and the scale are the column's, and only an integer widens.
        ScalarFunction::KeepsNumericShape => {
            let mut definition = own_shape(name)?;
            if definition.column_type != MYSQL_TYPE_NEWDECIMAL {
                definition.column_type = MYSQL_TYPE_LONGLONG;
            }
            definition
        }
        // Measured: a whole number of length 21 however wide the argument was.
        ScalarFunction::Truncates => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 21;
            definition
        }
        // Measured: the column's own shape, and NOT NULL because the fallback
        // cannot be null.
        ScalarFunction::Defaulted => {
            let mut definition = own_shape(name)?;
            if definition.column_type != MYSQL_TYPE_NEWDECIMAL {
                definition.column_type = MYSQL_TYPE_LONGLONG;
            }
            definition
        }
        ScalarFunction::Now => unreachable!("NOW was answered above"),
        ScalarFunction::Concatenates
        | ScalarFunction::TakesCharacters
        | ScalarFunction::Branches => {
            unreachable!("the text-width calls were answered above")
        }
    };
    // The answer belongs to no table, and is null wherever its column is.
    definition.schema.clear();
    definition.table.clear();
    definition.original_table.clear();
    definition.original_name.clear();
    let binary = if wants_text && function == ScalarFunction::KeepsTextShape {
        0
    } else {
        MYSQL_BINARY_FLAG
    };
    set_column_flags(
        &mut definition,
        binary | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
    );
    Ok(definition)
}

/// Builds the `VAR_STRING` a call that answers text of a known width reports.
#[cfg(unix)]
fn text_call_definition(name: String, width: u32, not_null: bool) -> ColumnDefinitionConfig {
    let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
    definition.column_length = width;
    definition.decimals = NOT_FIXED_DECIMALS;
    set_column_flags(
        &mut definition,
        if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
    );
    definition
}

#[cfg(unix)]
fn is_text_column(column: &MySqlColumnMetadata) -> bool {
    matches!(column.type_name(), "VARCHAR" | "CHAR" | "TEXT")
}

/// Reports whether a static projection has to read the source table's columns.
#[cfg(unix)]
fn needs_source_columns(metadata: &turso_mysql_parser::StaticSelectMetadata) -> bool {
    match metadata {
        turso_mysql_parser::StaticSelectMetadata::ColumnAggregate { .. } => true,
        turso_mysql_parser::StaticSelectMetadata::Arithmetic(shape) => shape.names_a_column(),
        turso_mysql_parser::StaticSelectMetadata::ScalarCall { columns, .. } => !columns.is_empty(),
        _ => false,
    }
}

/// Finishes a static projection whose type had to come from the table.
#[cfg(unix)]
fn aggregate_column_definition(
    source_metadata: Option<&TableResultMetadata>,
    name: String,
    metadata: &turso_mysql_parser::StaticSelectMetadata,
) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
    // Reporting either as MYSQL_TYPE_NULL while it holds a real value is worse
    // than refusing: the text protocol survives it and the binary one does not.
    match metadata {
        turso_mysql_parser::StaticSelectMetadata::ColumnAggregate { column_name, kind } => {
            source_metadata
                .ok_or(FrontendErrorKind::Unsupported)?
                .aggregate_column_definition(name, column_name, *kind)
        }
        // `SELECT 1+1` reads no table at all, so this one may have none.
        turso_mysql_parser::StaticSelectMetadata::Arithmetic(shape) => {
            TableResultMetadata::arithmetic_column_definition(source_metadata, name, shape)
        }
        turso_mysql_parser::StaticSelectMetadata::ScalarCall {
            function,
            columns,
            literal_characters,
            not_null,
        } => scalar_call_column_definition(
            source_metadata,
            name,
            *function,
            columns,
            *literal_characters,
            *not_null,
        ),
        _ => Err(FrontendErrorKind::Internal),
    }
}

#[cfg(unix)]
fn table_result_metadata(
    connection: &MySqlConnection,
    statement: &Statement,
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
    needs_source_columns: bool,
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
        source_tables,
        needs_source_columns,
    )
}

#[cfg(unix)]
fn prepared_table_result_metadata(
    connection: &MySqlConnection,
    type_metadata: &[MySqlPreparedResultColumnTypeMetadata],
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
) -> Result<Option<TableResultMetadata>, FrontendErrorKind> {
    let needs_source_columns = type_metadata
        .iter()
        .filter_map(MySqlPreparedResultColumnTypeMetadata::static_metadata)
        .any(needs_source_columns);
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
        source_tables,
        needs_source_columns,
    )
}

#[cfg(unix)]
fn table_result_metadata_for_references(
    connection: &MySqlConnection,
    source_references: &[(String, usize)],
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
    // `SELECT MIN(id) FROM t` reads a column and reports none, so the table has
    // to be looked up even though nothing points at it. Every other statement
    // says so with a source reference, and looking the table up for `SELECT 1`
    // would cost a catalog read for nothing.
    needs_source_columns: bool,
) -> Result<Option<TableResultMetadata>, FrontendErrorKind> {
    if source_tables.is_empty() || (source_references.is_empty() && !needs_source_columns) {
        return Ok(None);
    }
    let Some(selected_database) = selected_database else {
        return Ok(None);
    };
    let listed = connection
        .list_tables()
        .map_err(|_| FrontendErrorKind::Internal)?;
    let mut tables = Vec::with_capacity(source_tables.len());
    for source in source_tables {
        // View output metadata has different visibility and key/default
        // semantics from its base table. Keep it on the established generic
        // path until its MySQL wire fields have an oracle-backed contract.
        let table_kind = listed
            .iter()
            .find(|table| table.name().eq_ignore_ascii_case(source.table().as_str()))
            .map(|table| table.kind())
            .ok_or(FrontendErrorKind::MissingObject)?;
        if table_kind != MySqlTableKind::BaseTable {
            return Ok(None);
        }
        let columns = connection
            .list_columns(source.table())
            .map_err(column_metadata_error_kind)?;
        tables.push(SourceTableColumns {
            source_table: source.table().as_str().to_owned(),
            table_reference: source.reference().to_owned(),
            columns,
            outer: source.outer(),
            projected_columns: source.projected_columns().to_vec(),
        });
    }
    let metadata = TableResultMetadata {
        database: selected_database.to_owned(),
        tables,
        union: source_tables.iter().any(|source| source.branch() > 0),
    };
    for (reference, ordinal) in source_references {
        let Some(table) = metadata.table_for(reference) else {
            return Err(FrontendErrorKind::Unsupported);
        };
        table.column_ordinal(*ordinal)?;
    }
    Ok(Some(metadata))
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
    // Measured on MySQL 8.4.11: both TEXT and BLOB carry the blob flag, and a
    // BLOB carries the binary one on top of it.
    if matches!(column.type_name(), "TEXT" | "BLOB") {
        flags |= MYSQL_BLOB_FLAG;
    }
    if column.type_name() == "BLOB" {
        flags |= MYSQL_BINARY_FLAG;
    }
    flags
}

/// MySQL refuses a `DECIMAL` wider than this, so a result that would need one
/// is refused rather than reported with a precision MySQL cannot express.
const MYSQL_MAX_DECIMAL_PRECISION: u32 = 65;

const MYSQL_TYPE_TINY: u8 = 0x01;
const MYSQL_TYPE_SHORT: u8 = 0x02;
const MYSQL_TYPE_INT24: u8 = 0x09;
const MYSQL_TYPE_FLOAT: u8 = 0x04;
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
const MYSQL_NUM_FLAG: u16 = 32_768;
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
    // A statement that opens with a WITH clause is a SELECT that named its
    // subqueries first.
    statement_keyword(sql).is_some_and(|keyword| {
        keyword.eq_ignore_ascii_case("SELECT") || keyword.eq_ignore_ascii_case("WITH")
    })
}

fn is_checked_write_statement(sql: &str) -> bool {
    statement_keyword(sql).is_some_and(|keyword| {
        keyword.eq_ignore_ascii_case("INSERT")
            || keyword.eq_ignore_ascii_case("REPLACE")
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
        // The engine infers this for any text value, a string literal
        // included, which MySQL reports as VAR_STRING. A column *declared*
        // TEXT is a different question, answered below.
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
    if name.eq_ignore_ascii_case("FLOAT") {
        return Some(MYSQL_TYPE_FLOAT);
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
    // Measured on MySQL 8.4.11: a TEXT column reports BLOB, and differs from a
    // BLOB column only in its collation and length.
    if name.eq_ignore_ascii_case("TEXT") {
        return Some(MYSQL_TYPE_BLOB);
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
        // A MIN or MAX has no type of its own here; the caller reads it from
        // the source column instead.
        return static_result_column_metadata(metadata).map(|metadata| metadata.column_type);
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

/// One warning the last statement raised, as `SHOW WARNINGS` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlWarning {
    level: &'static str,
    code: u16,
    message: String,
}

impl MySqlWarning {
    /// The note MySQL raises for `DROP TABLE IF EXISTS` on a table that is not
    /// there.
    ///
    /// Measured on MySQL 8.4.11: `Note`, code 1051, and a message naming the
    /// table with its database.
    fn unknown_table(database: Option<&str>, table: &str) -> Self {
        let qualified = match database {
            Some(database) => format!("{database}.{table}"),
            None => table.to_owned(),
        };
        Self {
            level: "Note",
            code: 1051,
            message: format!("Unknown table '{qualified}'"),
        }
    }
}

/// Answers `SHOW WARNINGS` for what the last statement raised.
///
/// Measured on MySQL 8.4.11: `Level` is a `VAR_STRING` of length 28, `Code` a
/// `LONG` of length 5 carrying the unsigned, binary and numeric flags, and
/// `Message` a `VAR_STRING` of length 2048; all three are NOT NULL, and the two
/// strings report the not-fixed decimals value.
fn show_warnings_result(
    warnings: &[MySqlWarning],
    status_flags: u16,
    offset: u64,
    row_count: Option<u64>,
) -> CommandExecutionResult {
    let text_column = |name: &str, length: u32| {
        let mut column = column_definition(name.to_owned(), MYSQL_TYPE_VAR_STRING);
        column.column_length = length;
        column.decimals = NOT_FIXED_DECIMALS;
        set_column_flags(&mut column, MYSQL_NOT_NULL_FLAG);
        column
    };
    let mut code = column_definition("Code".to_owned(), MYSQL_TYPE_LONG);
    code.column_length = 5;
    set_column_flags(
        &mut code,
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG,
    );
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![text_column("Level", 28), code, text_column("Message", 2048)],
        rows: warnings
            .iter()
            .skip(offset as usize)
            .take(row_count.map(|c| c as usize).unwrap_or(usize::MAX))
            .map(|warning| {
                vec![
                    Some(warning.level.as_bytes().to_vec()),
                    Some(warning.code.to_string().into_bytes()),
                    Some(warning.message.as_bytes().to_vec()),
                ]
            })
            .collect(),
        warnings: 0,
        status_flags,
    })
}

/// Answers `SHOW ERRORS` for what the last statement raised.
///
/// It uses the same columns as `SHOW WARNINGS`, reporting only the diagnostics
/// with level `Error`.
fn show_errors_result(
    warnings: &[MySqlWarning],
    status_flags: u16,
    offset: u64,
    row_count: Option<u64>,
) -> CommandExecutionResult {
    let errors: Vec<MySqlWarning> = warnings
        .iter()
        .filter(|warning| warning.level.eq_ignore_ascii_case("Error"))
        .cloned()
        .collect();
    show_warnings_result(&errors, status_flags, offset, row_count)
}

/// Returns the flag a column carries because of its type alone.
///
/// Measured on MySQL 8.4.11: every numeric result carries `NUM`, whatever else
/// it carries — a plain `INT`, `TINYINT`, `DECIMAL`, `FLOAT` and `DOUBLE`
/// column each report it on their own, an aggregate and an expression report it
/// beside `BINARY`, and even a bare `SELECT NULL` reports it. A temporal column
/// does not, nor does a text or blob one.
const fn type_only_column_flags(column_type: u8) -> u16 {
    if matches!(
        column_type,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_NULL
    ) {
        MYSQL_NUM_FLAG
    } else {
        0
    }
}

/// Sets a column's flags, keeping the one its type carries on its own.
fn set_column_flags(definition: &mut ColumnDefinitionConfig, flags: u16) {
    definition.flags = flags | type_only_column_flags(definition.column_type);
}

fn column_definition(name: String, column_type: u8) -> ColumnDefinitionConfig {
    let mut definition = ColumnDefinitionConfig::new(name, column_type);
    definition.flags = type_only_column_flags(column_type);
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

/// How one column's values are rendered, where that is not just the engine's
/// own text form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextValueRendering {
    /// The engine's own text form.
    Engine,
    /// A `FLOAT`, whose value MySQL keeps in binary32 while the engine keeps it
    /// in binary64. Rounding it here is what makes `0.1` read back as `0.1`
    /// rather than as the binary64 nearest to a binary32 `0.1`.
    Binary32,
    /// A `DECIMAL`, which MySQL renders at the scale the column declared, so a
    /// `DECIMAL(10,2)` holding 1.5 reads back as `1.50`.
    Scaled(u8),
    /// An integer, which the engine answers as a float only when an arithmetic
    /// result left the range an integer can hold.
    Integer,
}

impl TextValueRendering {
    fn for_column(column: &ColumnDefinitionConfig) -> Self {
        match column.column_type {
            MYSQL_TYPE_FLOAT => Self::Binary32,
            MYSQL_TYPE_NEWDECIMAL => Self::Scaled(column.decimals),
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG => Self::Integer,
            _ => Self::Engine,
        }
    }
}

/// Renders one result value the way the text protocol sends it.
fn value_to_text_ref(
    value: &Value,
    rendering: TextValueRendering,
) -> Result<Option<Vec<u8>>, LimboError> {
    match value {
        Value::Null => Ok(None),
        Value::Numeric(Numeric::Float(float)) if rendering == TextValueRendering::Binary32 => {
            Ok(Some((f64::from(*float) as f32).to_string().into_bytes()))
        }
        Value::Numeric(Numeric::Float(float)) => {
            if let TextValueRendering::Scaled(scale) = rendering {
                return Ok(Some(
                    format!("{:.*}", usize::from(scale), f64::from(*float)).into_bytes(),
                ));
            }
            Ok(Some(value.to_string().into_bytes()))
        }
        Value::Numeric(Numeric::Integer(integer)) => {
            if let TextValueRendering::Scaled(scale) = rendering {
                return Ok(Some(
                    format!("{:.*}", usize::from(scale), *integer as f64).into_bytes(),
                ));
            }
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
mod tests;
