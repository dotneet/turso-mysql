//! Adapter from the bounded MySQL frontend to the transport-neutral server.
//!
//! The dependency points from the protocol crate to the frontend crate.  The
//! frontend does not depend on this crate, so this keeps the execution boundary
//! one-way while allowing a server owner to opt into the checked SELECT slice.

#[cfg(unix)]
mod catalog_results;
mod named_lock_results;

use turso_mysql::named_locks::{MySqlNamedLockSession, MySqlNamedLocks};
#[cfg(unix)]
mod reserved_keywords_84;

#[cfg(unix)]
use catalog_results::{
    admin_result_to_execution_result, analyze_table_result_to_execution_result,
    check_table_result_to_execution_result, gorm_columns_definitions, gorm_columns_result,
    gorm_current_database_result, information_schema_columns_result_to_execution_result,
    information_schema_schemata_result_to_execution_result,
    information_schema_tables_result_to_execution_result, reject_other_database_qualifier,
    show_columns_result, show_create_table_error_kind,
    show_create_table_result_to_execution_result, show_create_trigger_result,
    show_create_view_result, show_full_tables_result_to_execution_result,
    show_index_result_to_execution_result, show_table_status_result_to_execution_result,
    show_tables_result_to_execution_result, show_triggers_result, ShowTableStatusRow,
};

use std::collections::HashMap;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use turso_core::Statement;
use turso_core::{LimboError, Numeric, Value};
#[cfg(unix)]
use turso_mysql::schema_sql::SchemaSqlCreator;
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
    MySqlShowCreateTableError, MySqlShowCreateTableResult, MySqlTriggerMetadata, MySqlViewMetadata,
};
use turso_mysql::{
    MySqlAffectedRowsMode, MySqlAlterTableIndexError, MySqlConnection,
    MySqlCreateTableAsSelectError, MySqlDropTableError, MySqlMarkerType,
    MySqlPreparedExecutionResult, MySqlPreparedResultColumn, MySqlPreparedResultColumnTypeMetadata,
    MySqlQueryError, MySqlRenameTableError, MySqlTruncateTableError,
};
use turso_mysql::{
    MySqlPreparedStatementError, MySqlPreparedStatementMetadata, MySqlPreparedValue,
};
use turso_mysql_parser::{
    compound_drops_repeated_rows, is_connector_j_information_schema_collation_query,
    is_connector_j_reserved_keywords_query, parse_connector_j_foreign_keys,
    parse_optional_account_admin_command, parse_optional_alter_table_indexes,
    parse_optional_analyze_table, parse_optional_check_table,
    parse_optional_connector_j_information_schema_query,
    parse_optional_connector_j_schemata_listing_query, parse_optional_create_table_as_select,
    parse_optional_create_table_like, parse_optional_create_table_with_keys,
    parse_optional_created_table, parse_optional_describe, parse_optional_flush_tables,
    parse_optional_gorm_information_schema_prepared_query,
    parse_optional_information_schema_columns, parse_optional_information_schema_schemata,
    parse_optional_information_schema_tables, parse_optional_lock_tables,
    parse_optional_show_columns, parse_optional_show_create_table,
    parse_optional_show_create_trigger, parse_optional_show_full_tables, parse_optional_show_index,
    parse_optional_show_table_status, parse_optional_show_tables, parse_optional_show_triggers,
    renamed_tables, select_projection_origins, table_comment_change, table_counter_change,
    table_engine_restated, ArithmeticOperand, ArithmeticOperator, ArithmeticShape, Branch,
    ColumnAggregateKind, ConnectorJInformationSchemaQuery, ConnectorJSchemataListingQuery,
    GormInformationSchemaPreparedQuery, MySqlAccountAdminCommand, MySqlCatalogTable,
    MySqlDatabaseName, MySqlDerivedColumns, MySqlInformationSchemaColumnsColumn,
    MySqlInformationSchemaTablesColumn, MySqlLikePattern, MySqlLockTablesCommand,
    MySqlSelectProjectionOrigin, MySqlSelectSource, MySqlTableName, ScalarFunction,
};
use turso_mysql_parser::{
    parse_optional_drop_table, parse_optional_drop_view, parse_optional_show_character_sets,
    parse_optional_show_engines, parse_optional_show_errors, parse_optional_show_warnings,
    parse_optional_truncate_table, parse_select, MySqlShowCharacterSetsCommand,
    MySqlShowListingFilter, MySqlShowValueTest, SessionSqlMode,
};
#[cfg(unix)]
use turso_mysql_parser::{parse_optional_named_lock_query, write_the_current_database_in};

use crate::static_result_metadata::{static_column_definition, static_result_column_metadata};
#[cfg(unix)]
use crate::{
    authorization_frontend_error, AccountAdministration, AdminMutation,
    AuthenticatedCommandExecutor, AuthenticatedExecutorFactory, AuthenticatedPrincipal,
    AuthorizationError, DatabaseAction, DatabaseAuthorizer, TableAction,
};
use crate::{
    decode_statement_execute_parameters_with_long_data, BinaryResultSet, BinaryResultValue,
    ColumnDefinitionConfig, CommandExecutionOptions, CommandExecutionResult, CommandExecutor,
    CommandOkResult, FrontendErrorKind, InitialDatabaseSelector, PreparedStatementExecutionResult,
    PreparedStatementResult, StatementExecuteDecodeError, StatementParameterType,
    StatementParameterValue, TextResultSet, DEFAULT_UTF8MB4_COLLATION, MAX_COMMAND_PAYLOAD_LENGTH,
    MAX_DISPATCH_RESULT_ROWS, MAX_RESPONSE_PACKET_PAYLOAD_LENGTH, MAX_RESULT_COLUMNS,
    MAX_TEXT_ROW_VALUE_LENGTH, MYSQL_TYPE_BIT, SERVER_STATUS_AUTOCOMMIT, SERVER_STATUS_IN_TRANS,
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
    net_write_timeout: Duration,
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
            net_write_timeout: Duration::from_secs(60),
        }
    }

    /// Reports the runtime's bounded socket-write deadline to clients.
    pub fn with_net_write_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "write timeout must be non-zero");
        self.net_write_timeout = whole_second_timeout(timeout);
        self
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

    pub const fn net_write_timeout_seconds(self) -> u64 {
        self.net_write_timeout.as_secs()
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
    /// The largest `group_concat_max_len` each prepared statement was prepared
    /// or executed under, which is the limit MySQL keeps cutting it at.
    prepared_group_concat_max_lens: HashMap<u32, u64>,
    pending_long_data: PendingLongData,
    /// The named locks this session holds. A directly supplied connection has
    /// no server around it, so it has a lock table of its own.
    named_locks: MySqlNamedLockSession,
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
    read_only_select: bool,
    /// Whether this is a `SELECT`, which a session's `max_execution_time`
    /// holds to its limit whether or not it locks the rows it reads.
    selects: bool,
    connection: MySqlConnection,
    connection_statement_id: u32,
    parameter_types: Option<Vec<StatementParameterType>>,
    catalog_query: Option<GormInformationSchemaPreparedQuery>,
    /// A statement with no parameters and no rows that the checked prepared
    /// path does not take, run through the text path when it is executed.
    runs_as_text: Option<String>,
    /// The checked statement's own text, which a window or a `UNION` reads its
    /// result columns' origins from each time it is executed.
    sql: Option<String>,
    /// The limit a `GROUP_CONCAT` in it is cut at. Measured on MySQL 8.4.11:
    /// a prepared statement keeps the largest `group_concat_max_len` it was
    /// prepared or executed under, for both the cut and its column, so a
    /// session lowering the limit afterwards leaves the statement uncut.
    group_concat_max_len: u64,
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
            prepared_group_concat_max_lens: HashMap::new(),
            pending_long_data: PendingLongData::default(),
            named_locks: Arc::new(MySqlNamedLocks::default()).session(),
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

    pub fn with_net_write_timeout(mut self, timeout: Duration) -> Self {
        self.bootstrap_settings = self.bootstrap_settings.with_net_write_timeout(timeout);
        self
    }
}

impl CommandExecutor for MySqlCommandAdapter {
    fn status_flags(&self) -> u16 {
        connection_status_flags(&self.connection)
    }

    fn session_wait_timeout(&self) -> Option<Duration> {
        self.session_variables.wait_timeout()
    }

    fn no_backslash_escapes(&self) -> bool {
        self.connection.parser_mode().no_backslash_escapes
    }

    fn execute_init_db(
        &mut self,
        _database: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        Err(FrontendErrorKind::Unsupported)
    }

    fn execute_query(&mut self, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind> {
        refuse_what_latin1_reads_differently(&self.session_variables, sql)?;
        let connection = self.connection.clone();
        prepare_for_client_statement(&connection, &self.session_variables)?;
        let result = self.execute_query_statement(sql);
        let result = finish_client_statement(&connection, &mut self.session_variables, result);
        refuse_a_result_in_latin1(&self.session_variables, result)
    }

    fn execute_reset_connection(&mut self) -> Result<(), FrontendErrorKind> {
        self.connection
            .reset_connection()
            .map_err(frontend_query_error)?;
        self.prepared_types.clear();
        self.prepared_group_concat_max_lens.clear();
        // MySQL's reset lets go of every named lock the session holds.
        self.named_locks.release_all();
        self.session_variables = crate::session_variables::MySqlSessionVariables::default();
        self.connection.set_time_zone_offset_seconds(0);
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
        refuse_a_prepared_statement_under_latin1(&self.session_variables)?;
        let group_concat_max_len = self.session_variables.group_concat_max_len();
        self.connection
            .set_group_concat_max_len(group_concat_max_len);
        let mut result = prepare_checked_statement(&self.connection, sql)?;
        if let Err(error) = apply_raw_column_collations(
            &self.connection,
            &mut result.columns,
            self.session_variables.raw_character_set_results(),
        ) {
            self.connection
                .remove_prepared_statement(result.statement_id);
            return Err(error);
        }
        self.prepared_group_concat_max_lens
            .insert(result.statement_id, group_concat_max_len);
        Ok(result)
    }

    fn execute_stmt_close(&mut self, statement_id: u32) {
        self.connection.remove_prepared_statement(statement_id);
        self.prepared_types.remove(&statement_id);
        self.prepared_group_concat_max_lens.remove(&statement_id);
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
        refuse_a_prepared_statement_under_latin1(&self.session_variables)?;
        let connection = self.connection.clone();
        prepare_for_client_statement(&connection, &self.session_variables)?;
        let result = self.execute_prepared_statement_command(statement_id, parameter_payload);
        finish_client_statement(&connection, &mut self.session_variables, result)
    }
}

impl MySqlCommandAdapter {
    fn execute_query_statement(
        &mut self,
        sql: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        let status_flags = self.status_flags();
        if let Some(result) = self.session_variables.execute_query(
            sql,
            self.bootstrap_settings,
            None,
            self.connection.parser_mode(),
            status_flags,
        )? {
            self.connection
                .set_time_zone_offset_seconds(self.session_variables.time_zone_offset_seconds());
            return Ok(result);
        }
        if let Some(query) = parse_optional_named_lock_query(sql, self.connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return named_lock_results::named_lock_result(&query, &self.named_locks, status_flags);
        }
        refuse_an_unknown_system_variable(sql)?;
        if is_internal_catalog_select(sql) {
            return Err(FrontendErrorKind::Unsupported);
        }
        if parse_optional_show_engines(sql, self.connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
            .is_some()
        {
            return Ok(show_engines_result(status_flags));
        }
        if let Some(command) =
            parse_optional_show_character_sets(sql, self.connection.parser_mode())
                .map_err(|_| FrontendErrorKind::Unsupported)?
        {
            return show_character_sets_result(&command, status_flags);
        }
        if let Some(command) = parse_optional_show_warnings(sql, self.connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            // MySQL keeps the warnings until the next statement that can raise
            // one, so reading them does not clear them.
            if command.is_count() {
                return Ok(show_warnings_count_result(
                    &self.raised_warnings,
                    status_flags,
                ));
            }
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
            if command.is_count() {
                return Ok(show_errors_count_result(
                    &self.raised_warnings,
                    status_flags,
                ));
            }
            return Ok(show_errors_result(
                &self.raised_warnings,
                status_flags,
                command.offset(),
                command.row_count(),
            ));
        }
        self.raised_warnings.clear();
        let mut result = execute_checked_query(
            &self.connection,
            sql,
            None,
            &[],
            CheckedQueryOptions {
                query_timeout: None,
                select_time_limit: self.session_variables.select_time_limit(),
                affected_rows_mode: MySqlAffectedRowsMode::Changed,
                sql_notes: self.session_variables.sql_notes(),
                group_concat_max_len: self.session_variables.group_concat_max_len(),
                raised: &mut self.raised_warnings,
            },
        )?;
        if let CommandExecutionResult::ResultSet(rows) = &mut result {
            shift_text_timestamp_columns(&self.connection, rows)?;
            apply_raw_column_collations(
                &self.connection,
                &mut rows.columns,
                self.session_variables.raw_character_set_results(),
            )?;
        }
        Ok(result)
    }

    fn execute_prepared_statement_command(
        &mut self,
        statement_id: u32,
        parameter_payload: &[u8],
    ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
        let long_data = self.pending_long_data.take_statement(statement_id);
        let group_concat_max_len = self
            .prepared_group_concat_max_lens
            .entry(statement_id)
            .or_default();
        *group_concat_max_len =
            (*group_concat_max_len).max(self.session_variables.group_concat_max_len());
        self.connection
            .set_group_concat_max_len(*group_concat_max_len);
        self.connection.forget_group_concat_cuts();
        self.raised_warnings.clear();
        let mut result = execute_prepared_statement(
            &self.connection,
            &mut self.prepared_types,
            statement_id,
            parameter_payload,
            long_data,
            None,
            MySqlAffectedRowsMode::Changed,
        )?;
        self.raised_warnings.extend(
            self.connection
                .take_group_concat_cuts()
                .into_iter()
                .map(MySqlWarning::cut_by_group_concat),
        );
        if let PreparedStatementExecutionResult::ResultSet(rows) = &mut result {
            apply_raw_column_collations(
                &self.connection,
                &mut rows.columns,
                self.session_variables.raw_character_set_results(),
            )?;
            rows.warnings = u16::try_from(self.raised_warnings.len()).unwrap_or(u16::MAX);
        }
        Ok(result)
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
    account_administration: Option<Arc<dyn AccountAdministration>>,
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
            account_administration: None,
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

    pub fn with_net_write_timeout(mut self, timeout: Duration) -> Self {
        self.bootstrap_settings = self.bootstrap_settings.with_net_write_timeout(timeout);
        self
    }

    pub fn with_account_administration(
        mut self,
        administration: Arc<dyn AccountAdministration>,
    ) -> Self {
        self.account_administration = Some(administration);
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
        let named_locks = self.catalog.named_locks().session();
        Ok(AuthorizedDatabaseCommandAdapter {
            session: self.catalog.new_session_with_prepared_statement_authority(
                self.schema_context,
                self.prepared_statement_authority.clone(),
            ),
            catalog: self.catalog,
            schema_context: self.schema_context,
            principal,
            authorizer: self.authorizer,
            query_timeout: self.query_timeout,
            bootstrap_settings: self.bootstrap_settings,
            account_administration: self.account_administration,
            session_variables: crate::session_variables::MySqlSessionVariables::default(),
            raised_warnings: Vec::new(),
            command_options,
            prepared_statements: DatabasePreparedStatementRegistry::default(),
            pending_long_data: PendingLongData::default(),
            named_locks,
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
    catalog: Arc<MySqlDatabaseCatalog>,
    schema_context: turso_mysql::schema_sql::SchemaSqlSessionContext,
    principal: AuthenticatedPrincipal,
    authorizer: Arc<A>,
    query_timeout: Option<Duration>,
    bootstrap_settings: MySqlBootstrapSettings,
    account_administration: Option<Arc<dyn AccountAdministration>>,
    session_variables: crate::session_variables::MySqlSessionVariables,
    /// What the last statement warned about, which `SHOW WARNINGS` reports.
    raised_warnings: Vec<MySqlWarning>,
    command_options: CommandExecutionOptions,
    prepared_statements: DatabasePreparedStatementRegistry,
    pending_long_data: PendingLongData,
    /// The named locks this session holds, in the table every session of the
    /// server shares.
    named_locks: MySqlNamedLockSession,
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
            .map_err(database_error_kind)?;
        self.carry_the_session_onto_its_connection()
    }

    /// Gives a connection the session has just opened what the session asked
    /// for before it had one: a dump turns foreign-key checks off before its
    /// `CREATE DATABASE` and `USE`, and its rows rely on that.
    fn carry_the_session_onto_its_connection(&self) -> Result<(), FrontendErrorKind> {
        let connection = self.session.connection().map_err(database_error_kind)?;
        connection.set_time_zone_offset_seconds(self.session_variables.time_zone_offset_seconds());
        connection.set_foreign_key_checks(self.session_variables.foreign_key_checks());
        if let Some(wait) = self.session_variables.lock_wait() {
            connection.set_lock_wait(wait);
        }
        Ok(())
    }

    fn authorize(&self, action: DatabaseAction<'_>) -> Result<(), FrontendErrorKind> {
        self.authorizer
            .authorize(&self.principal, action)
            .map_err(authorization_frontend_error)
    }

    fn execute_account_admin_command(
        &self,
        mut command: MySqlAccountAdminCommand,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        self.authorize(DatabaseAction::ManageAccounts)?;
        let administration = self
            .account_administration
            .as_ref()
            .ok_or(FrontendErrorKind::Unsupported)?;
        if let Ok(connection) = self.session.connection() {
            connection
                .execute_transaction_command("COMMIT")
                .map_err(frontend_query_error)?;
        }
        match &mut command {
            MySqlAccountAdminCommand::CreateUser { username, password } => {
                administration.apply(
                    &self.principal,
                    AdminMutation::CreateUser {
                        username,
                        password: password.as_mut_bytes(),
                    },
                )?;
            }
            MySqlAccountAdminCommand::GrantTableSelect {
                username,
                database,
                table,
            } => {
                administration.apply(
                    &self.principal,
                    AdminMutation::GrantTableSelect {
                        username,
                        database: database.as_str(),
                        table: table.as_str(),
                    },
                )?;
            }
            MySqlAccountAdminCommand::RevokeTableSelect {
                username,
                database,
                table,
            } => {
                administration.apply(
                    &self.principal,
                    AdminMutation::RevokeTableSelect {
                        username,
                        database: database.as_str(),
                        table: table.as_str(),
                    },
                )?;
            }
        }
        Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: self.status_flags(),
            ..CommandOkResult::default()
        }))
    }

    fn authorize_table_select(&self, database: &str, table: &str) -> Result<(), FrontendErrorKind> {
        self.authorizer
            .authorize_table(&self.principal, TableAction::Select { database, table })
            .map_err(authorization_frontend_error)
    }

    /// Leaves the tables this session may see where the catalog tables read
    /// them.
    ///
    /// Those tables are registered on the database rather than on one
    /// connection, so a session that may see only some of them has to say so
    /// before a statement that scans one runs.
    #[cfg(unix)]
    fn publish_catalog_visibility(
        &mut self,
        selected_database: &str,
        visibility: CatalogVisibility,
    ) -> Result<(), FrontendErrorKind> {
        let visible = match visibility {
            CatalogVisibility::All => None,
            CatalogVisibility::GrantedTables => {
                let tables = self
                    .session
                    .connection()
                    .map_err(database_error_kind)?
                    .list_tables()
                    .map_err(|_| FrontendErrorKind::Internal)?;
                Some(
                    self.filter_catalog_tables(
                        selected_database,
                        CatalogVisibility::GrantedTables,
                        tables,
                    )?
                    .into_iter()
                    .map(|table| table.name().to_owned())
                    .collect::<Vec<_>>(),
                )
            }
        };
        self.session
            .connection()
            .map_err(database_error_kind)?
            .set_visible_tables(visible);
        Ok(())
    }

    /// Works out the rows of an `information_schema` table that only this
    /// session can answer, and leaves them where the table reads them.
    ///
    /// They are worked out again for every statement that scans the table, so
    /// a statement never reads rows a schema change has made stale.
    #[cfg(unix)]
    fn publish_catalog_rows(
        &mut self,
        selected_database: &str,
        visibility: CatalogVisibility,
        catalog: MySqlCatalogTable,
    ) -> Result<(), FrontendErrorKind> {
        let rows = match catalog {
            MySqlCatalogTable::Columns => {
                self.information_schema_columns_rows(selected_database, visibility)?
            }
            MySqlCatalogTable::Schemata => {
                self.information_schema_schemata_rows(selected_database)?
            }
            _ => unreachable!("only COLUMNS and SCHEMATA take their rows from the session"),
        };
        self.session
            .connection()
            .map_err(database_error_kind)?
            .set_catalog_rows(catalog, rows);
        Ok(())
    }

    /// The rows of `information_schema.COLUMNS`: every column of every table
    /// and view the session may see.
    ///
    /// A table whose columns this cannot read answers its name and nothing
    /// else, so a query about some other table is not refused for it, while
    /// one that reads its columns is.
    #[cfg(unix)]
    fn information_schema_columns_rows(
        &self,
        selected_database: &str,
        visibility: CatalogVisibility,
    ) -> Result<Vec<Vec<Value>>, FrontendErrorKind> {
        let connection = self.session.connection().map_err(database_error_kind)?;
        let tables = connection
            .list_tables()
            .map_err(|_| FrontendErrorKind::Internal)?;
        let tables = self.filter_catalog_tables(selected_database, visibility, tables)?;
        // A column lists the privileges the session holds on it, the same
        // answer `SHOW FULL COLUMNS` gives here.
        let privileges = match visibility {
            CatalogVisibility::All => "select,insert,update,references",
            CatalogVisibility::GrantedTables => "select",
        };
        let mut rows = Vec::new();
        for table in tables {
            let name =
                MySqlTableName::parse(table.name()).map_err(|_| FrontendErrorKind::Internal)?;
            match connection.list_columns(&name) {
                Ok(columns) => rows.extend(catalog_results::information_schema_columns_rows(
                    selected_database,
                    table.name(),
                    columns,
                    privileges,
                )?),
                Err(MySqlColumnMetadataError::TableNotFound) => {}
                Err(MySqlColumnMetadataError::Engine(_)) => {
                    return Err(FrontendErrorKind::Internal)
                }
                Err(
                    MySqlColumnMetadataError::CorruptDefinition
                    | MySqlColumnMetadataError::UnsupportedDefinition,
                ) => rows.push(vec![
                    Value::build_text("def"),
                    Value::build_text(selected_database.to_owned()),
                    Value::build_text(table.name().to_owned()),
                ]),
            }
            if rows.len() > MAX_DISPATCH_RESULT_ROWS {
                return Err(FrontendErrorKind::Internal);
            }
        }
        Ok(rows)
    }

    /// The rows of `information_schema.SCHEMATA`: every database the session
    /// may list, or the one it is in when it may list none.
    ///
    /// MySQL shows a session the databases it holds any privilege on, and a
    /// session is in one it may reach.
    #[cfg(unix)]
    fn information_schema_schemata_rows(
        &mut self,
        selected_database: &str,
    ) -> Result<Vec<Vec<Value>>, FrontendErrorKind> {
        let databases = self.schemata_databases(selected_database)?;
        let mut rows = Vec::with_capacity(databases.len());
        for database in databases {
            // A database dropped since it was listed is not answered.
            let collation = match self.catalog.collation(&database) {
                Ok(collation) => collation,
                Err(MySqlDatabaseError::DatabaseNotFound(_)) => continue,
                Err(error) => return Err(database_error_kind(error)),
            };
            rows.push(catalog_results::information_schema_schemata_row(
                database, collation,
            ));
        }
        Ok(rows)
    }

    /// The databases `information_schema.SCHEMATA` answers: every one the
    /// session may list, or the one it is in when it may list none.
    #[cfg(unix)]
    fn schemata_databases(
        &mut self,
        selected_database: &str,
    ) -> Result<Vec<String>, FrontendErrorKind> {
        let databases = match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::List)
        {
            Ok(()) => {
                let result = self
                    .session
                    .execute_parsed_admin_command(MySqlAdminCommand::ListDatabases)
                    .map_err(database_error_kind)?;
                let MySqlAdminCommandResult::Listed { databases } = result else {
                    unreachable!("listing the databases answers a list");
                };
                databases
            }
            Err(AuthorizationError::Denied) => vec![selected_database.to_owned()],
            Err(error) => return Err(authorization_frontend_error(error)),
        };
        Ok(databases)
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
    ) -> Result<CatalogVisibility, FrontendErrorKind> {
        match self.authorize_catalog_visibility(database)? {
            CatalogVisibility::All => Ok(CatalogVisibility::All),
            CatalogVisibility::GrantedTables => {
                self.authorize_table_select(database, table)?;
                Ok(CatalogVisibility::GrantedTables)
            }
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
    ) -> Result<(Vec<MySqlSelectSource>, CatalogVisibility), FrontendErrorKind> {
        let source_tables = parsed_source_tables(sql);
        match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::Query { database })
        {
            Ok(()) => {
                if source_tables.iter().any(is_internal_catalog_source) {
                    return Err(FrontendErrorKind::Unsupported);
                }
                Ok((source_tables, CatalogVisibility::All))
            }
            Err(AuthorizationError::Denied) => {
                // Only a session without the database-wide grant needs to know
                // this, and reading it parses the statement again.
                let read_only_select = parse_select(sql, self.session.session_sql_mode())
                    .is_ok_and(|select| !select.locks_rows());
                if !read_only_select {
                    return Err(FrontendErrorKind::AccessDenied);
                }
                // A join reads every table it names, so a grant on one of them
                // is not a grant on the statement.
                if source_tables.is_empty() {
                    return Err(FrontendErrorKind::AccessDenied);
                }
                for source in &source_tables {
                    // An `information_schema` table needs no grant of its own:
                    // what a session may see is decided row by row, against
                    // the grants it holds on the tables those rows name.
                    if source.catalog().is_some() {
                        continue;
                    }
                    let table = source.table().as_str();
                    if is_internal_catalog_table(table) {
                        return Err(FrontendErrorKind::AccessDenied);
                    }
                    self.authorize_table_select(database, table)?;
                }
                Ok((source_tables, CatalogVisibility::GrantedTables))
            }
            Err(error) => Err(authorization_frontend_error(error)),
        }
    }

    fn authorize_prepared_query(
        &self,
        database: &str,
        source_tables: &[MySqlSelectSource],
        read_only_select: bool,
    ) -> Result<(), FrontendErrorKind> {
        if source_tables
            .iter()
            .any(|source| source.catalog() == Some(MySqlCatalogTable::Views))
        {
            return self.authorize(DatabaseAction::Query { database });
        }
        match self
            .authorizer
            .authorize(&self.principal, DatabaseAction::Query { database })
        {
            Ok(()) => Ok(()),
            Err(AuthorizationError::Denied) => {
                if !read_only_select
                    || source_tables.is_empty()
                    || source_tables
                        .iter()
                        .any(|source| source.catalog().is_some())
                {
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
            MySqlAdminCommand::CreateDatabase { name, .. } => {
                let canonical_name =
                    canonicalize_database_name(name.as_str()).map_err(database_error_kind)?;
                self.authorize(DatabaseAction::Create {
                    database: &canonical_name,
                })?;
                self.raised_warnings.clear();
            }
            MySqlAdminCommand::AlterDatabase { name, .. } => {
                let canonical_name = match name {
                    Some(name) => {
                        canonicalize_database_name(name.as_str()).map_err(database_error_kind)?
                    }
                    None => self
                        .session
                        .selected_database()
                        .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                        .to_owned(),
                };
                // Changing what a database gives the tables made in it is a
                // schema change within it, which the grant to query it covers
                // here as it covers `ALTER TABLE`.
                self.authorize(DatabaseAction::Query {
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
            // Measured on MySQL 8.4.11: any privilege in the database lets a
            // session print its `CREATE DATABASE`, a grant on one table of it
            // among them, as any lets it select the database.
            MySqlAdminCommand::ShowCreateDatabase { name, .. } => {
                let canonical_name =
                    canonicalize_database_name(name.as_str()).map_err(database_error_kind)?;
                self.authorize(DatabaseAction::Connect {
                    database: Some(&canonical_name),
                })?;
            }
            MySqlAdminCommand::Use { name } => {
                let canonical_name =
                    canonicalize_database_name(name.as_str()).map_err(database_error_kind)?;
                self.authorize(DatabaseAction::Connect {
                    database: Some(&canonical_name),
                })?;
                if let Ok(connection) = self.session.connection() {
                    if connection.tables_are_locked() {
                        if self.session.selected_database() == Some(canonical_name.as_str()) {
                            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                                status_flags: self.status_flags(),
                                ..CommandOkResult::default()
                            }));
                        }
                        return Err(FrontendErrorKind::Unsupported);
                    }
                    if !connection.is_auto_commit() || !connection.session_autocommit() {
                        return Err(FrontendErrorKind::Unsupported);
                    }
                }
            }
            MySqlAdminCommand::ListDatabases => {
                self.authorize(DatabaseAction::List)?;
            }
        }

        let alters = matches!(command, MySqlAdminCommand::AlterDatabase { .. });
        let result =
            self.session
                .execute_parsed_admin_command(command)
                .map_err(|error| match error {
                    MySqlDatabaseError::DatabaseNotFound(_) if alters => {
                        FrontendErrorKind::NoDatabaseToAlter
                    }
                    error => database_error_kind(error),
                })?;
        if matches!(result, MySqlAdminCommandResult::Selected { .. }) {
            self.carry_the_session_onto_its_connection()?;
        }
        // Measured on MySQL 8.4.11: `CREATE DATABASE IF NOT EXISTS` over a
        // database that is there answers OK with note 1007.
        if let MySqlAdminCommandResult::AlreadyExists { database } = &result {
            let noted = self.session_variables.sql_notes();
            if noted {
                self.raised_warnings
                    .push(MySqlWarning::database_exists(database));
            }
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                warnings: u16::from(noted),
                ..CommandOkResult::default()
            }));
        }
        admin_result_to_execution_result(result)
    }
    fn prepare_gorm_catalog_query(
        &mut self,
        query: GormInformationSchemaPreparedQuery,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        let database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?
            .to_owned();
        match query {
            GormInformationSchemaPreparedQuery::CurrentDatabase => {
                self.authorize(DatabaseAction::Connect {
                    database: Some(&database),
                })?;
            }
            GormInformationSchemaPreparedQuery::Columns
            | GormInformationSchemaPreparedQuery::HasTable
            | GormInformationSchemaPreparedQuery::HasColumn
            | GormInformationSchemaPreparedQuery::HasIndex
            | GormInformationSchemaPreparedQuery::HasConstraint => {
                self.authorize_catalog_visibility(&database)?;
            }
        }
        let connection = self
            .session
            .connection()
            .map_err(database_error_kind)?
            .clone();
        // The engine-owned statement keeps the same global prepared-statement
        // quota and lifecycle as any other binary-protocol statement.
        let (reservation_sql, parameter_count) = match query {
            GormInformationSchemaPreparedQuery::HasTable
            | GormInformationSchemaPreparedQuery::HasColumn
            | GormInformationSchemaPreparedQuery::HasIndex
            | GormInformationSchemaPreparedQuery::HasConstraint => ("SELECT ?, ?, ?", 3),
            _ => ("SELECT ?, ?", 2),
        };
        let reserved = connection
            .prepare_checked_statement(reservation_sql)
            .map_err(prepared_statement_error)?;
        if reserved.parameter_count != parameter_count {
            connection.remove_prepared_statement(reserved.statement_id);
            return Err(FrontendErrorKind::Internal);
        }
        let Some(statement_id) = self.prepared_statements.next_statement_id else {
            connection.remove_prepared_statement(reserved.statement_id);
            return Err(FrontendErrorKind::Internal);
        };
        let columns = match query {
            GormInformationSchemaPreparedQuery::CurrentDatabase => {
                vec![catalog_results::information_schema_schemata_column()]
            }
            GormInformationSchemaPreparedQuery::Columns => gorm_columns_definitions(),
            GormInformationSchemaPreparedQuery::HasTable
            | GormInformationSchemaPreparedQuery::HasColumn
            | GormInformationSchemaPreparedQuery::HasIndex
            | GormInformationSchemaPreparedQuery::HasConstraint => {
                vec![catalog_results::gorm_catalog_count_column()]
            }
        };
        self.prepared_statements.next_statement_id = statement_id.checked_add(1);
        self.prepared_statements.statements.insert(
            statement_id,
            DatabasePreparedStatement {
                database,
                source_tables: Vec::new(),
                read_only_select: true,
                selects: false,
                connection,
                connection_statement_id: reserved.statement_id,
                parameter_types: None,
                catalog_query: Some(query),
                runs_as_text: None,
                sql: None,
                group_concat_max_len: self.session_variables.group_concat_max_len(),
            },
        );
        Ok(PreparedStatementResult {
            statement_id,
            parameters: (1..=parameter_count)
                .map(|index| column_definition(format!("?{index}"), MYSQL_TYPE_NULL))
                .collect(),
            columns,
            warnings: 0,
            status_flags: self.status_flags(),
        })
    }

    fn execute_gorm_catalog_query(
        &mut self,
        statement_id: u32,
        parameter_payload: &[u8],
    ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
        let statement = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
        let query = statement.catalog_query.ok_or(FrontendErrorKind::Internal)?;
        let database = statement.database.clone();
        let connection = statement.connection.clone();
        let cached_types = statement.parameter_types.clone();
        let long_data = self.pending_long_data.take_statement(statement_id);
        if let Some(error) = long_data.error {
            return Err(pending_long_data_error(error));
        }
        let long_data = long_data
            .values
            .iter()
            .map(|value| value.as_deref())
            .collect::<Vec<_>>();
        let parameter_count = match query {
            GormInformationSchemaPreparedQuery::HasTable
            | GormInformationSchemaPreparedQuery::HasColumn
            | GormInformationSchemaPreparedQuery::HasIndex
            | GormInformationSchemaPreparedQuery::HasConstraint => 3,
            _ => 2,
        };
        let decoded = decode_statement_execute_parameters_with_long_data(
            parameter_payload,
            parameter_count,
            cached_types.as_deref(),
            &long_data,
        )
        .map_err(statement_execute_decode_error)?;
        self.prepared_statements
            .statements
            .get_mut(&statement_id)
            .expect("the prepared catalog statement was already found")
            .parameter_types = Some(decoded.types);
        let values = decoded
            .values
            .into_iter()
            .map(|value| match value {
                StatementParameterValue::String(value) => Ok(value),
                _ => Err(FrontendErrorKind::Unsupported),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let status_flags = self.status_flags();
        let mut result = match query {
            GormInformationSchemaPreparedQuery::CurrentDatabase => {
                let [first, second] = values.as_slice() else {
                    return Err(FrontendErrorKind::Internal);
                };
                self.authorize(DatabaseAction::Connect {
                    database: Some(&database),
                })?;
                if first.as_str() != format!("{database}%") || second != &database {
                    return Err(FrontendErrorKind::Unsupported);
                }
                gorm_current_database_result(&database, status_flags)
            }
            GormInformationSchemaPreparedQuery::Columns => {
                let [first, second] = values.as_slice() else {
                    return Err(FrontendErrorKind::Internal);
                };
                let visibility = self.authorize_catalog_visibility(first)?;
                let table =
                    MySqlTableName::parse(second).map_err(|_| FrontendErrorKind::Unsupported)?;
                let columns = if !self.gorm_catalog_table_visible(first, &table, visibility)? {
                    Vec::new()
                } else if let Some(connection) =
                    self.gorm_catalog_connection(first, &database, &connection)?
                {
                    list_gorm_catalog_columns(&connection, &table)?
                } else {
                    Vec::new()
                };
                gorm_columns_result(columns, status_flags)?
            }
            GormInformationSchemaPreparedQuery::HasTable
            | GormInformationSchemaPreparedQuery::HasColumn
            | GormInformationSchemaPreparedQuery::HasIndex
            | GormInformationSchemaPreparedQuery::HasConstraint => {
                let [schema, name, filter] = values.as_slice() else {
                    return Err(FrontendErrorKind::Internal);
                };
                let visibility = self.authorize_catalog_visibility(schema)?;
                let table =
                    MySqlTableName::parse(name).map_err(|_| FrontendErrorKind::Unsupported)?;
                let count = if !self.gorm_catalog_table_visible(schema, &table, visibility)? {
                    0
                } else if let Some(connection) =
                    self.gorm_catalog_connection(schema, &database, &connection)?
                {
                    match query {
                        GormInformationSchemaPreparedQuery::HasTable => connection
                            .list_tables()
                            .map_err(|_| FrontendErrorKind::Internal)?
                            .iter()
                            .filter(|listed| {
                                listed.name().eq_ignore_ascii_case(table.as_str())
                                    && match listed.kind() {
                                        MySqlTableKind::BaseTable => {
                                            filter.eq_ignore_ascii_case("BASE TABLE")
                                        }
                                        MySqlTableKind::View => filter.eq_ignore_ascii_case("VIEW"),
                                    }
                            })
                            .count(),
                        GormInformationSchemaPreparedQuery::HasColumn => {
                            list_gorm_catalog_columns(&connection, &table)?
                                .iter()
                                .filter(|column| column.name().eq_ignore_ascii_case(filter))
                                .count()
                        }
                        GormInformationSchemaPreparedQuery::HasIndex => {
                            match connection.list_indexes(&table) {
                                Ok(indexes) => indexes
                                    .iter()
                                    .filter(|index| index.key_name().eq_ignore_ascii_case(filter))
                                    .count(),
                                Err(
                                    MySqlShowCreateTableError::MissingTable
                                    | MySqlShowCreateTableError::NotTable,
                                ) => 0,
                                Err(error) => return Err(show_create_table_error_kind(error)),
                            }
                        }
                        GormInformationSchemaPreparedQuery::HasConstraint => {
                            match connection.count_constraints(&table, filter) {
                                Ok(count) => count,
                                Err(
                                    MySqlShowCreateTableError::MissingTable
                                    | MySqlShowCreateTableError::NotTable,
                                ) => 0,
                                Err(error) => return Err(show_create_table_error_kind(error)),
                            }
                        }
                        _ => unreachable!("count query variant was already matched"),
                    }
                } else {
                    0
                };
                catalog_results::gorm_catalog_count_result(
                    i64::try_from(count).map_err(|_| FrontendErrorKind::Internal)?,
                    status_flags,
                )
            }
        };
        if let PreparedStatementExecutionResult::ResultSet(rows) = &mut result {
            apply_raw_column_collations(
                &connection,
                &mut rows.columns,
                self.session_variables.raw_character_set_results(),
            )?;
        }
        Ok(result)
    }

    fn gorm_catalog_table_visible(
        &self,
        schema: &str,
        table: &MySqlTableName,
        visibility: CatalogVisibility,
    ) -> Result<bool, FrontendErrorKind> {
        match visibility {
            CatalogVisibility::All => Ok(true),
            CatalogVisibility::GrantedTables => match self.authorizer.authorize_table(
                &self.principal,
                TableAction::Select {
                    database: schema,
                    table: table.as_str(),
                },
            ) {
                Ok(()) => Ok(true),
                Err(AuthorizationError::Denied) => Ok(false),
                Err(error) => Err(authorization_frontend_error(error)),
            },
        }
    }

    fn gorm_catalog_connection(
        &self,
        schema: &str,
        selected_database: &str,
        selected_connection: &MySqlConnection,
    ) -> Result<Option<MySqlConnection>, FrontendErrorKind> {
        if schema == selected_database {
            return Ok(Some(selected_connection.clone()));
        }
        let mut session = self.catalog.new_session(self.schema_context);
        match session.select_database(schema) {
            Ok(()) => Ok(Some(
                session.connection().map_err(database_error_kind)?.clone(),
            )),
            Err(
                MySqlDatabaseError::InvalidDatabaseName | MySqlDatabaseError::DatabaseNotFound(_),
            ) => Ok(None),
            Err(error) => Err(database_error_kind(error)),
        }
    }

    fn execute_connector_j_catalog_query(
        &self,
        query: ConnectorJInformationSchemaQuery,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        let selected_database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?;
        let selected_connection = self.session.connection().map_err(database_error_kind)?;
        let schema = match &query {
            ConnectorJInformationSchemaQuery::Tables { schema, .. }
            | ConnectorJInformationSchemaQuery::Columns { schema, .. }
            | ConnectorJInformationSchemaQuery::PrimaryKeys { schema, .. }
            | ConnectorJInformationSchemaQuery::IndexInfo { schema, .. }
            | ConnectorJInformationSchemaQuery::ImportedKeys { schema, .. }
            | ConnectorJInformationSchemaQuery::ExportedKeys { schema, .. } => schema,
        }
        .clone();
        let visibility = self.authorize_catalog_visibility(&schema)?;
        if schema != selected_database && visibility == CatalogVisibility::GrantedTables {
            return Err(FrontendErrorKind::Unsupported);
        }
        let connection =
            self.gorm_catalog_connection(&schema, selected_database, selected_connection)?;
        let status_flags = self.status_flags();
        match query {
            ConnectorJInformationSchemaQuery::Tables {
                table_pattern,
                types,
                ..
            } => {
                let Some(connection) = connection else {
                    return catalog_results::connector_j_tables_result(
                        &schema,
                        Vec::new(),
                        status_flags,
                    );
                };
                let tables = connection
                    .list_tables()
                    .map_err(|_| FrontendErrorKind::Internal)?;
                let tables = self.filter_catalog_tables(&schema, visibility, tables)?;
                let pattern =
                    MySqlLikePattern::new(&table_pattern, self.session.session_sql_mode());
                let tables = tables
                    .into_iter()
                    .filter(|table| {
                        let kind = match table.kind() {
                            MySqlTableKind::BaseTable => "TABLE",
                            MySqlTableKind::View => "VIEW",
                        };
                        pattern.matches(table.name())
                            && (types.is_empty()
                                || types
                                    .iter()
                                    .any(|expected| expected.eq_ignore_ascii_case(kind)))
                    })
                    .collect::<Vec<_>>();
                catalog_results::connector_j_tables_result(&schema, tables, status_flags)
            }
            ConnectorJInformationSchemaQuery::Columns {
                table_pattern,
                column_pattern,
                ..
            } => {
                let Some(connection) = connection else {
                    return catalog_results::connector_j_columns_result(
                        &schema,
                        Vec::new(),
                        status_flags,
                    );
                };
                let tables = connection
                    .list_tables()
                    .map_err(|_| FrontendErrorKind::Internal)?;
                let tables = self.filter_catalog_tables(&schema, visibility, tables)?;
                let table_pattern = table_pattern
                    .as_deref()
                    .map(|pattern| MySqlLikePattern::new(pattern, self.session.session_sql_mode()));
                let column_pattern =
                    MySqlLikePattern::new(&column_pattern, self.session.session_sql_mode());
                let mut columns = Vec::new();
                for table in tables.into_iter().filter(|table| {
                    table_pattern
                        .as_ref()
                        .is_none_or(|pattern| pattern.matches(table.name()))
                }) {
                    if table.kind() != MySqlTableKind::BaseTable {
                        return Err(FrontendErrorKind::Unsupported);
                    }
                    let name = MySqlTableName::parse(table.name())
                        .map_err(|_| FrontendErrorKind::Internal)?;
                    let listed = list_gorm_catalog_columns(&connection, &name)?
                        .into_iter()
                        .enumerate()
                        .filter(|(_, column)| column_pattern.matches(column.name()))
                        .collect::<Vec<_>>();
                    columns.push((table.name().to_owned(), listed));
                }
                catalog_results::connector_j_columns_result(&schema, columns, status_flags)
            }
            ConnectorJInformationSchemaQuery::ImportedKeys { table, .. } => {
                let table =
                    MySqlTableName::parse(&table).map_err(|_| FrontendErrorKind::Unsupported)?;
                if !self.gorm_catalog_table_visible(&schema, &table, visibility)? {
                    return catalog_results::connector_j_foreign_keys_result(
                        Vec::new(),
                        false,
                        status_flags,
                    );
                }
                let Some(connection) = connection else {
                    return catalog_results::connector_j_foreign_keys_result(
                        Vec::new(),
                        false,
                        status_flags,
                    );
                };
                let keys = match connection.show_create_table(&table) {
                    Ok(created) => parse_connector_j_foreign_keys(
                        created.create_statement(),
                        self.session.session_sql_mode(),
                    )
                    .map_err(|_| FrontendErrorKind::Unsupported)?,
                    Err(
                        MySqlShowCreateTableError::MissingTable
                        | MySqlShowCreateTableError::NotTable,
                    ) => Vec::new(),
                    Err(error) => return Err(show_create_table_error_kind(error)),
                };
                let mut rows = Vec::new();
                for key in keys {
                    let parent = MySqlTableName::parse(&key.parent_table)
                        .map_err(|_| FrontendErrorKind::Unsupported)?;
                    let pk_name =
                        connector_j_parent_key_name(&connection, &parent, &key.parent_columns)?;
                    let update_rule = connector_j_referential_rule(key.on_update.as_deref())?;
                    let delete_rule = connector_j_referential_rule(key.on_delete.as_deref())?;
                    for (position, (child, parent)) in key
                        .child_columns
                        .iter()
                        .zip(&key.parent_columns)
                        .enumerate()
                    {
                        if rows.len() >= MAX_DISPATCH_RESULT_ROWS {
                            return Err(FrontendErrorKind::Internal);
                        }
                        let value = |text: &str| Some(text.as_bytes().to_vec());
                        let number = |value: usize| Some(value.to_string().into_bytes());
                        rows.push(vec![
                            value(&schema),
                            None,
                            value(&key.parent_table),
                            value(parent),
                            value(&schema),
                            None,
                            value(table.as_str()),
                            value(child),
                            number(position + 1),
                            number(update_rule),
                            number(delete_rule),
                            value(&key.name),
                            value(&pk_name),
                            number(7),
                        ]);
                    }
                }
                catalog_results::connector_j_foreign_keys_result(rows, false, status_flags)
            }
            ConnectorJInformationSchemaQuery::PrimaryKeys { table, .. } => {
                let table =
                    MySqlTableName::parse(&table).map_err(|_| FrontendErrorKind::Unsupported)?;
                if !self.gorm_catalog_table_visible(&schema, &table, visibility)? {
                    return catalog_results::connector_j_primary_keys_result(
                        Vec::new(),
                        status_flags,
                    );
                }
                let Some(connection) = connection else {
                    return catalog_results::connector_j_primary_keys_result(
                        Vec::new(),
                        status_flags,
                    );
                };
                let indexes = match connection.list_indexes(&table) {
                    Ok(indexes) => indexes,
                    Err(
                        MySqlShowCreateTableError::MissingTable
                        | MySqlShowCreateTableError::NotTable,
                    ) => Vec::new(),
                    Err(error) => return Err(show_create_table_error_kind(error)),
                };
                let mut indexes = indexes
                    .into_iter()
                    .filter(|index| index.key_name() == "PRIMARY")
                    .collect::<Vec<_>>();
                indexes.sort_unstable_by(|left, right| {
                    left.column_name()
                        .cmp(right.column_name())
                        .then(left.sequence_in_index().cmp(&right.sequence_in_index()))
                });
                let rows = indexes
                    .into_iter()
                    .map(|index| {
                        vec![
                            Some(schema.as_bytes().to_vec()),
                            None,
                            Some(table.as_str().as_bytes().to_vec()),
                            Some(index.column_name().as_bytes().to_vec()),
                            Some(index.sequence_in_index().to_string().into_bytes()),
                            Some(b"PRIMARY".to_vec()),
                        ]
                    })
                    .collect();
                catalog_results::connector_j_primary_keys_result(rows, status_flags)
            }
            ConnectorJInformationSchemaQuery::IndexInfo { table, .. } => {
                let table =
                    MySqlTableName::parse(&table).map_err(|_| FrontendErrorKind::Unsupported)?;
                if !self.gorm_catalog_table_visible(&schema, &table, visibility)? {
                    return catalog_results::connector_j_index_info_result(
                        Vec::new(),
                        status_flags,
                    );
                }
                let Some(connection) = connection else {
                    return catalog_results::connector_j_index_info_result(
                        Vec::new(),
                        status_flags,
                    );
                };
                let mut indexes = match connection.list_indexes(&table) {
                    Ok(indexes) => indexes,
                    Err(
                        MySqlShowCreateTableError::MissingTable
                        | MySqlShowCreateTableError::NotTable,
                    ) => Vec::new(),
                    Err(error) => return Err(show_create_table_error_kind(error)),
                };
                if !indexes.is_empty() && !connector_j_table_is_empty(&connection, &table)? {
                    return Err(FrontendErrorKind::Unsupported);
                }
                indexes.sort_unstable_by(|left, right| {
                    (!left.unique())
                        .cmp(&!right.unique())
                        .then(left.key_name().cmp(right.key_name()))
                        .then(left.sequence_in_index().cmp(&right.sequence_in_index()))
                });
                let rows = indexes
                    .into_iter()
                    .map(|index| {
                        vec![
                            Some(schema.as_bytes().to_vec()),
                            None,
                            Some(table.as_str().as_bytes().to_vec()),
                            Some(u8::from(!index.unique()).to_string().into_bytes()),
                            None,
                            Some(index.key_name().as_bytes().to_vec()),
                            Some(b"3".to_vec()),
                            Some(index.sequence_in_index().to_string().into_bytes()),
                            Some(index.column_name().as_bytes().to_vec()),
                            Some(b"A".to_vec()),
                            Some(b"0".to_vec()),
                            Some(b"0".to_vec()),
                            None,
                        ]
                    })
                    .collect();
                catalog_results::connector_j_index_info_result(rows, status_flags)
            }
            ConnectorJInformationSchemaQuery::ExportedKeys { table, .. } => {
                let parent =
                    MySqlTableName::parse(&table).map_err(|_| FrontendErrorKind::Unsupported)?;
                if !self.gorm_catalog_table_visible(&schema, &parent, visibility)? {
                    return catalog_results::connector_j_foreign_keys_result(
                        Vec::new(),
                        true,
                        status_flags,
                    );
                }
                let Some(connection) = connection else {
                    return catalog_results::connector_j_foreign_keys_result(
                        Vec::new(),
                        true,
                        status_flags,
                    );
                };
                let tables = connection
                    .list_tables()
                    .map_err(|_| FrontendErrorKind::Internal)?;
                let tables = self.filter_catalog_tables(&schema, visibility, tables)?;
                let mut rows = Vec::new();
                for child in tables
                    .into_iter()
                    .filter(|table| table.kind() == MySqlTableKind::BaseTable)
                {
                    let child_name = MySqlTableName::parse(child.name())
                        .map_err(|_| FrontendErrorKind::Internal)?;
                    let created = connection
                        .show_create_table(&child_name)
                        .map_err(show_create_table_error_kind)?;
                    let keys = parse_connector_j_foreign_keys(
                        created.create_statement(),
                        self.session.session_sql_mode(),
                    )
                    .map_err(|_| FrontendErrorKind::Unsupported)?;
                    for key in keys
                        .into_iter()
                        .filter(|key| key.parent_table.eq_ignore_ascii_case(parent.as_str()))
                    {
                        let pk_name =
                            connector_j_parent_key_name(&connection, &parent, &key.parent_columns)?;
                        let update_rule = connector_j_referential_rule(key.on_update.as_deref())?;
                        let delete_rule = connector_j_referential_rule(key.on_delete.as_deref())?;
                        for (position, (child_column, parent_column)) in key
                            .child_columns
                            .iter()
                            .zip(&key.parent_columns)
                            .enumerate()
                        {
                            if rows.len() >= MAX_DISPATCH_RESULT_ROWS {
                                return Err(FrontendErrorKind::Internal);
                            }
                            let value = |text: &str| Some(text.as_bytes().to_vec());
                            let number = |value: usize| Some(value.to_string().into_bytes());
                            rows.push(vec![
                                value(&schema),
                                None,
                                value(parent.as_str()),
                                value(parent_column),
                                value(&schema),
                                None,
                                value(child.name()),
                                value(child_column),
                                number(position + 1),
                                number(update_rule),
                                number(delete_rule),
                                value(&key.name),
                                value(&pk_name),
                                number(7),
                            ]);
                        }
                    }
                }
                catalog_results::connector_j_foreign_keys_result(rows, true, status_flags)
            }
        }
    }
}

#[cfg(unix)]
fn connector_j_parent_key_name(
    connection: &MySqlConnection,
    parent: &MySqlTableName,
    columns: &[String],
) -> Result<String, FrontendErrorKind> {
    let indexes = connection
        .list_indexes(parent)
        .map_err(show_create_table_error_kind)?;
    let names = indexes
        .iter()
        .filter(|index| index.unique())
        .map(|index| index.key_name())
        .collect::<std::collections::BTreeSet<_>>();
    let mut matching = Vec::new();
    for name in names {
        let indexed = indexes
            .iter()
            .filter(|index| index.key_name() == name)
            .map(|index| index.column_name())
            .collect::<Vec<_>>();
        if indexed == columns.iter().map(String::as_str).collect::<Vec<_>>() {
            matching.push(name);
        }
    }
    match matching.as_slice() {
        [name] => Ok((*name).to_owned()),
        _ => Err(FrontendErrorKind::Unsupported),
    }
}

#[cfg(unix)]
fn connector_j_table_is_empty(
    connection: &MySqlConnection,
    table: &MySqlTableName,
) -> Result<bool, FrontendErrorKind> {
    let escaped = table.as_str().replace('`', "``");
    let sql = format!("SELECT 1 FROM `{escaped}` LIMIT 1");
    let rows = connection
        .prepare(&sql)
        .and_then(|mut statement| statement.run_collect_rows())
        .map_err(|_| FrontendErrorKind::Internal)?;
    Ok(rows.is_empty())
}

#[cfg(unix)]
fn connector_j_referential_rule(action: Option<&str>) -> Result<usize, FrontendErrorKind> {
    match action {
        None | Some("RESTRICT" | "NO ACTION") => Ok(1),
        Some("CASCADE") => Ok(0),
        Some("SET NULL") => Ok(2),
        Some("SET DEFAULT") => Ok(4),
        Some(_) => Err(FrontendErrorKind::Unsupported),
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

    fn no_backslash_escapes(&self) -> bool {
        self.session.session_sql_mode().no_backslash_escapes
    }

    fn binary_result_charset(&self) -> bool {
        self.session_variables.binary_character_set_results()
    }

    fn connection_collation(&self) -> u16 {
        self.session_variables.connection_collation_id()
    }

    fn session_wait_timeout(&self) -> Option<Duration> {
        self.session_variables.wait_timeout()
    }

    fn execute_init_db(
        &mut self,
        database: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        self.select_database(database)?;
        Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
    }

    fn execute_query(&mut self, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind> {
        refuse_what_latin1_reads_differently(&self.session_variables, sql)?;
        self.session_variables
            .set_database_collation(self.session.selected_database_collation());
        let connection = self.session.connection().ok().cloned();
        if let Some(connection) = &connection {
            prepare_for_client_statement(connection, &self.session_variables)?;
        }
        let result = self.execute_query_statement(sql);
        let result = match &connection {
            Some(connection) => {
                finish_client_statement(connection, &mut self.session_variables, result)
            }
            None => result,
        };
        refuse_a_result_in_latin1(&self.session_variables, result)
    }

    fn execute_reset_connection(&mut self) -> Result<(), FrontendErrorKind> {
        self.session
            .reset_connection()
            .map_err(frontend_query_error)?;
        for statement in self.prepared_statements.statements.values() {
            statement.connection.clear_prepared_statements();
        }
        self.prepared_statements.statements.clear();
        // MySQL's reset lets go of every named lock the session holds.
        self.named_locks.release_all();
        self.session_variables = crate::session_variables::MySqlSessionVariables::default();
        if let Ok(connection) = self.session.connection() {
            connection.set_time_zone_offset_seconds(0);
        }
        self.raised_warnings.clear();
        self.pending_long_data = PendingLongData::default();
        Ok(())
    }

    fn execute_stmt_prepare(
        &mut self,
        sql: &str,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        refuse_a_prepared_statement_under_latin1(&self.session_variables)?;
        self.session_variables
            .set_database_collation(self.session.selected_database_collation());
        if let Some(query) = parse_optional_gorm_information_schema_prepared_query(
            sql,
            self.session.session_sql_mode(),
        )
        .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return self.prepare_gorm_catalog_query(query);
        }
        let written = write_the_current_database_in(
            sql,
            self.session.selected_database(),
            self.session.session_sql_mode(),
        )
        .map_err(|_| FrontendErrorKind::Syntax)?;
        let sql = written.as_deref().unwrap_or(sql);
        match self.prepare_checked_database_statement(sql) {
            Err(FrontendErrorKind::Unsupported | FrontendErrorKind::Syntax)
                if turso_mysql_parser::answers_no_rows_and_binds_nothing(
                    sql,
                    self.session.session_sql_mode(),
                ) =>
            {
                self.prepare_text_statement(sql)
            }
            prepared => prepared,
        }
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
        refuse_a_prepared_statement_under_latin1(&self.session_variables)?;
        self.session_variables
            .set_database_collation(self.session.selected_database_collation());
        let connection = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .map(|statement| statement.connection.clone());
        if let Some(connection) = &connection {
            prepare_for_client_statement(connection, &self.session_variables)?;
        }
        let result = self.execute_prepared_statement_command(statement_id, parameter_payload);
        match &connection {
            Some(connection) => {
                finish_client_statement(connection, &mut self.session_variables, result)
            }
            None => result,
        }
    }
}

#[cfg(unix)]
impl<A> AuthorizedDatabaseCommandAdapter<A>
where
    A: DatabaseAuthorizer,
{
    fn execute_query_statement(
        &mut self,
        sql: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        let written = write_the_current_database_in(
            sql,
            self.session.selected_database(),
            self.session.session_sql_mode(),
        )
        .map_err(|_| FrontendErrorKind::Syntax)?;
        let sql = written.as_deref().unwrap_or(sql);
        let status_flags = self.status_flags();
        if let Some(result) = self.session_variables.execute_query(
            sql,
            self.bootstrap_settings,
            self.session.selected_database(),
            self.session.session_sql_mode(),
            status_flags,
        )? {
            if let Ok(connection) = self.session.connection() {
                connection.set_time_zone_offset_seconds(
                    self.session_variables.time_zone_offset_seconds(),
                );
            }
            for statement in self.prepared_statements.statements.values() {
                statement.connection.set_time_zone_offset_seconds(
                    self.session_variables.time_zone_offset_seconds(),
                );
            }
            // A lock wait and the foreign-key switch both have to reach the
            // engine connection, which the session variables do not hold, so
            // they are applied here.
            if let Some(wait) = self.session_variables.take_lock_wait_timeout() {
                if let Ok(connection) = self.session.connection() {
                    connection.set_lock_wait(wait);
                }
            }
            if let Some(enabled) = self.session_variables.take_foreign_key_checks() {
                if let Ok(connection) = self.session.connection() {
                    connection.set_foreign_key_checks(enabled);
                }
            }
            return Ok(result);
        }
        if let Some(query) = parse_optional_named_lock_query(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return named_lock_results::named_lock_result(&query, &self.named_locks, status_flags);
        }
        refuse_an_unknown_system_variable(sql)?;
        if is_account_admin_statement(sql) {
            let command =
                parse_optional_account_admin_command(sql, self.session.session_sql_mode())
                    .map_err(|_| FrontendErrorKind::Syntax)?
                    .ok_or(FrontendErrorKind::Syntax)?;
            return self.execute_account_admin_command(command);
        }
        if let Some(command) = self
            .session
            .parse_admin_command(sql)
            .map_err(admin_error_kind)?
        {
            return self.execute_admin_command(command);
        }
        if is_connector_j_information_schema_collation_query(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(
                catalog_results::connector_j_information_schema_collation_result(status_flags),
            );
        }
        if is_connector_j_reserved_keywords_query(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return Ok(catalog_results::connector_j_reserved_keywords_result(
                status_flags,
            ));
        }
        if let Some(query) =
            parse_optional_connector_j_schemata_listing_query(sql, self.session.session_sql_mode())
                .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return match query {
                ConnectorJSchemataListingQuery::Catalogs => {
                    self.authorize(DatabaseAction::List)?;
                    let listed = self
                        .session
                        .execute_parsed_admin_command(MySqlAdminCommand::ListDatabases)
                        .map_err(database_error_kind)?;
                    let MySqlAdminCommandResult::Listed { databases } = listed else {
                        unreachable!("database listing always returns names");
                    };
                    catalog_results::connector_j_catalogs_result(databases, status_flags)
                }
                ConnectorJSchemataListingQuery::Schemas => {
                    Ok(catalog_results::connector_j_schemas_result(status_flags))
                }
            };
        }
        if let Some(query) = parse_optional_connector_j_information_schema_query(
            sql,
            self.session.session_sql_mode(),
        )
        .map_err(|_| FrontendErrorKind::Syntax)?
        {
            return self.execute_connector_j_catalog_query(query);
        }
        // The one written shape this recognized before the engine could scan
        // the table still answers it, because it takes a `WHERE TABLE_SCHEMA =
        // DATABASE()` that the checked `SELECT` surface does not read yet.
        // Anything else falls through to that surface rather than failing here.
        if let Ok(Some(query)) =
            parse_optional_information_schema_tables(sql, SessionSqlMode::default())
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
                query.columns(),
                self.status_flags(),
            );
        }
        // The two written shapes the catalogue reader took before the engine
        // could scan these tables still answer them, and anything else falls
        // through to the tables the engine scans.
        if let Ok(Some(_)) =
            parse_optional_information_schema_schemata(sql, SessionSqlMode::default())
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
        if let Ok(Some(query)) =
            parse_optional_information_schema_columns(sql, SessionSqlMode::default())
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            // A query naming a database reads that one, and this server has
            // the columns of the selected database alone, so any other name
            // answers no rows. The name is read as it was written: measured on
            // MySQL 8.4.11, `TABLE_SCHEMA = 'TURSO_ORACLE'` answers nothing
            // where `'turso_oracle'` answers the columns.
            if query
                .schema()
                .is_some_and(|schema| schema != selected_database)
            {
                return information_schema_columns_result_to_execution_result(
                    Vec::new(),
                    query.columns(),
                    self.status_flags(),
                );
            }
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
                query.columns(),
                self.status_flags(),
            );
        }
        if let Some(command) = parse_optional_check_table(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            match self.authorizer.authorize(
                &self.principal,
                DatabaseAction::Query {
                    database: &selected_database,
                },
            ) {
                Ok(()) => {}
                Err(AuthorizationError::Denied) => {
                    self.authorize_table_select(&selected_database, command.table().as_str())?;
                }
                Err(error) => return Err(authorization_frontend_error(error)),
            }
            let problem = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .check_table(command.table())
                .map_err(|error| match error {
                    LimboError::SchemaUpdated => FrontendErrorKind::UnknownTable,
                    error => frontend_error_kind(error),
                })?;
            return Ok(check_table_result_to_execution_result(
                &selected_database,
                command.table().as_str(),
                problem,
                self.status_flags(),
            ));
        }
        // MySQL closes its table cache here. This server keeps none, so the
        // statement asks for something already true and reads no table, which
        // is why it needs the selected database and nothing else.
        // A FLUSH this server will not answer is valid MySQL, so it reads as
        // unsupported rather than as a syntax error.
        // MySQL locks each table it names and holds the lock across statements.
        // This holds one lock over the whole database, so it locks more than
        // was asked for rather than less — which is a lock all the same, and
        // the one thing the statement asks to be true.
        let locking = match parse_optional_lock_tables(sql, self.session.session_sql_mode()) {
            Ok(locking) => locking,
            Err(turso_mysql_parser::ParseError::Unsupported { .. }) => {
                return Err(FrontendErrorKind::Unsupported)
            }
            Err(_) => return Err(FrontendErrorKind::Syntax),
        };
        if let Some(command) = locking {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            let connection = self.session.connection().map_err(database_error_kind)?;
            match command {
                MySqlLockTablesCommand::Lock => {
                    self.authorize(DatabaseAction::Query {
                        database: &selected_database,
                    })?;
                    connection.lock_tables()
                }
                MySqlLockTablesCommand::Unlock => connection.unlock_tables(),
            }
            .map_err(frontend_query_error)?;
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: self.status_flags(),
                ..CommandOkResult::default()
            }));
        }
        let flush = match parse_optional_flush_tables(sql, self.session.session_sql_mode()) {
            Ok(flush) => flush,
            Err(turso_mysql_parser::ParseError::Unsupported { .. }) => {
                return Err(FrontendErrorKind::Unsupported)
            }
            Err(_) => return Err(FrontendErrorKind::Syntax),
        };
        if flush.is_some() {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            self.authorize_catalog_visibility(&selected_database)?;
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: self.status_flags(),
                ..CommandOkResult::default()
            }));
        }
        if let Some(command) = parse_optional_analyze_table(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            self.authorize(DatabaseAction::Query {
                database: &selected_database,
            })?;
            self.session
                .connection()
                .map_err(database_error_kind)?
                .analyze_table(command.table())
                .map_err(|error| match error {
                    LimboError::SchemaUpdated => FrontendErrorKind::UnknownTable,
                    error => frontend_error_kind(error),
                })?;
            return Ok(analyze_table_result_to_execution_result(
                &selected_database,
                command.table().as_str(),
                self.status_flags(),
            ));
        }
        if let Some(command) = parse_optional_show_table_status(sql, SessionSqlMode::default())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            reject_other_database_qualifier(command.database(), &selected_database)?;
            let visibility = self.authorize_catalog_visibility(&selected_database)?;
            let connection = self.session.connection().map_err(database_error_kind)?;
            let tables = connection
                .list_tables()
                .map_err(|_| FrontendErrorKind::Internal)?;
            let tables = self.filter_catalog_tables(&selected_database, visibility, tables)?;
            // Measured on MySQL 8.4.11: the pattern names the tables to
            // report, and one nothing matches answers no rows.
            let tables = tables.into_iter().filter(|table| {
                command
                    .pattern()
                    .is_none_or(|pattern| pattern.matches(table.name()))
            });
            let mut rows = Vec::new();
            for table in tables {
                // The row count is counted rather than estimated. MySQL's is an
                // InnoDB estimate; a real count is the more useful answer and
                // the only one this can give.
                let counted = connection
                    .count_rows(table.name())
                    .map_err(|_| FrontendErrorKind::Internal)?;
                rows.push(ShowTableStatusRow {
                    name: table.name().to_owned(),
                    rows: counted,
                    auto_increment: None,
                    collation: table.collation().unwrap_or_default().name(),
                    comment: table.comment().unwrap_or_default().to_owned(),
                });
            }
            return show_table_status_result_to_execution_result(rows, self.status_flags());
        }
        let full_tables = parse_optional_show_full_tables(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?;
        let plain_tables = parse_optional_show_tables(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?;
        if full_tables.is_some() || plain_tables.is_some() {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            if let Some(command) = &full_tables {
                reject_other_database_qualifier(command.database(), &selected_database)?;
            }
            let visibility = self.authorize_catalog_visibility(&selected_database)?;
            let tables = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .list_tables()
                .map_err(|_| FrontendErrorKind::Internal)?;
            let tables = self.filter_catalog_tables(&selected_database, visibility, tables)?;
            // The pattern is matched after the scan and the authorization, so
            // an oversized catalog still fails closed on the scan limit rather
            // than being silently cut short by the pattern.
            if let Some(command) = full_tables {
                let tables = tables
                    .into_iter()
                    .filter(|table| command.covers(table.name()))
                    .collect::<Vec<_>>();
                return show_full_tables_result_to_execution_result(
                    &selected_database,
                    command.pattern().map(|pattern| pattern.text()),
                    tables,
                    self.status_flags(),
                );
            }
            let command = plain_tables.expect("one of the two SHOW TABLES forms matched");
            return show_tables_result_to_execution_result(
                &selected_database,
                command.pattern().map(|pattern| pattern.text()),
                tables
                    .into_iter()
                    .map(|table| table.name().to_owned())
                    .filter(|name| command.covers(name)),
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
            let connection = self.session.connection().map_err(database_error_kind)?;
            return match connection.show_create_table(command.table()) {
                Ok(result) => {
                    show_create_table_result_to_execution_result(result, self.status_flags())
                }
                Err(MySqlShowCreateTableError::NotTable) => {
                    self.authorize(DatabaseAction::Query {
                        database: &selected_database,
                    })?;
                    let view = connection
                        .view_metadata(command.table())
                        .map_err(|_| FrontendErrorKind::Internal)?
                        .ok_or(FrontendErrorKind::MissingObject)?;
                    show_create_view_result(view, self.status_flags())
                }
                Err(error) => Err(show_create_table_error_kind(error)),
            };
        }

        if let Some(command) =
            turso_mysql_parser::parse_optional_show_create_view(sql, SessionSqlMode::default())
                .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            reject_other_database_qualifier(command.database(), &selected_database)?;
            self.authorize(DatabaseAction::Query {
                database: &selected_database,
            })?;
            let connection = self.session.connection().map_err(database_error_kind)?;
            if let Some(view) = connection
                .view_metadata(command.table())
                .map_err(|_| FrontendErrorKind::Internal)?
            {
                return show_create_view_result(view, self.status_flags());
            }
            // Measured on MySQL 8.4.11: a table of that name is 1347 and a
            // name nothing has is 1146.
            let tables = connection.list_tables().map_err(frontend_error_kind)?;
            return Err(
                if tables
                    .iter()
                    .any(|table| table.name() == command.table().as_str())
                {
                    FrontendErrorKind::NotView
                } else {
                    FrontendErrorKind::MissingObject
                },
            );
        }

        if let Some(command) = parse_optional_show_triggers(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            reject_other_database_qualifier(command.database(), &selected_database)?;
            self.authorize(DatabaseAction::Query {
                database: &selected_database,
            })?;
            let triggers = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .list_triggers()
                .map_err(|_| FrontendErrorKind::Internal)?
                .into_iter()
                .filter(|trigger| {
                    command
                        .pattern()
                        .is_none_or(|pattern| pattern.matches_keeping_case(&trigger.table))
                })
                .collect();
            return show_triggers_result(triggers, self.status_flags());
        }

        if let Some(command) =
            parse_optional_show_create_trigger(sql, self.session.session_sql_mode())
                .map_err(|_| FrontendErrorKind::Syntax)?
        {
            let selected_database = self
                .session
                .selected_database()
                .ok_or(FrontendErrorKind::NoDatabaseSelected)?
                .to_owned();
            self.authorize(DatabaseAction::Query {
                database: &selected_database,
            })?;
            let trigger = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .trigger_metadata(command.name())
                .map_err(|_| FrontendErrorKind::Internal)?
                .ok_or(FrontendErrorKind::MissingObject)?;
            return show_create_trigger_result(trigger, self.status_flags());
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
            let visibility =
                self.authorize_catalog_table(&selected_database, command.table().as_str())?;
            let columns = self
                .session
                .connection()
                .map_err(database_error_kind)?
                .list_columns(command.table())
                .map_err(column_metadata_error_kind)?;
            // Measured on MySQL 8.4.11: the pattern names the columns to
            // report, and one nothing matches answers no rows rather than an
            // error.
            let columns = match command.pattern() {
                Some(pattern) => columns
                    .into_iter()
                    .filter(|column| pattern.matches(column.name()))
                    .collect(),
                None => columns,
            };
            let privileges = match visibility {
                CatalogVisibility::All => b"select,insert,update,references".as_slice(),
                CatalogVisibility::GrantedTables => b"select".as_slice(),
            };
            return show_columns_result(columns, self.status_flags(), command.full(), privileges);
        }
        // The collations and character sets are the server's, and MySQL lists
        // them with no database selected.
        if let Some(command) =
            parse_optional_show_character_sets(sql, self.session.session_sql_mode())
                .map_err(|_| FrontendErrorKind::Unsupported)?
        {
            return show_character_sets_result(&command, self.status_flags());
        }

        let selected_database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?
            .to_owned();
        // `SHOW ENGINES` describes the server rather than the selected
        // database, so it needs no table authorization.
        if parse_optional_show_engines(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
            .is_some()
        {
            return Ok(show_engines_result(status_flags));
        }
        if let Some(command) = parse_optional_show_warnings(sql, self.session.session_sql_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
        {
            if command.is_count() {
                return Ok(show_warnings_count_result(
                    &self.raised_warnings,
                    status_flags,
                ));
            }
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
            if command.is_count() {
                return Ok(show_errors_count_result(
                    &self.raised_warnings,
                    status_flags,
                ));
            }
            return Ok(show_errors_result(
                &self.raised_warnings,
                status_flags,
                command.offset(),
                command.row_count(),
            ));
        }
        let (source_tables, visibility) = self.authorize_query_text(&selected_database, sql)?;
        if matches!(visibility, CatalogVisibility::GrantedTables)
            && source_tables
                .iter()
                .any(|source| source.catalog() == Some(MySqlCatalogTable::Views))
        {
            return Err(FrontendErrorKind::AccessDenied);
        }
        // A statement that reads an `information_schema` table leaves what
        // this session may see where that table reads it. Nothing else pays
        // for the lookup, and the grant it was just authorized under is the
        // one that decides it.
        if source_tables
            .iter()
            .any(|source| source.catalog().is_some())
        {
            self.publish_catalog_visibility(&selected_database, visibility)?;
        }
        for catalog in source_tables
            .iter()
            .filter_map(MySqlSelectSource::catalog)
            .filter(|catalog| catalog.rows_come_from_the_session())
        {
            self.publish_catalog_rows(&selected_database, visibility, catalog)?;
        }
        self.raised_warnings.clear();
        let connection = self.session.connection().map_err(database_error_kind)?;
        let replacement = if may_create_a_view_or_trigger(sql) {
            turso_mysql_parser::parse_optional_view_replacement(sql, connection.parser_mode())
                .map_err(|_| FrontendErrorKind::Syntax)?
        } else {
            None
        };
        if let Some(replacement) = replacement {
            let creator = self
                .authorizer
                .schema_creator_username(&self.principal)
                .map_err(authorization_frontend_error)?
                .map(|username| {
                    SchemaSqlCreator::new(
                        username,
                        crate::session_variables::reported_sql_mode(connection.parser_mode()),
                    )
                });
            connection
                .replace_view(&replacement, creator)
                .map_err(|error| match error {
                    turso_mysql::MySqlReplaceViewError::NotView => FrontendErrorKind::NotView,
                    turso_mysql::MySqlReplaceViewError::MissingView => {
                        FrontendErrorKind::MissingObject
                    }
                    turso_mysql::MySqlReplaceViewError::Query(error) => frontend_query_error(error),
                })?;
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: connection_status_flags(connection),
                ..CommandOkResult::default()
            }));
        }
        // Measured on MySQL 8.4.11: a `CREATE VIEW` naming a table or a view
        // that is already there is 1050, before anything else is read.
        if may_create_a_view_or_trigger(sql) {
            if let Ok(turso_parser::ast::Stmt::CreateView { view_name, .. }) =
                turso_mysql_parser::parse_schema_ddl_ast(sql, connection.parser_mode())
            {
                let view = MySqlTableName::parse(view_name.name.as_str())
                    .map_err(|_| FrontendErrorKind::Syntax)?;
                if connection
                    .names_a_table(&view)
                    .map_err(frontend_error_kind)?
                {
                    return Err(FrontendErrorKind::DuplicateObject);
                }
            }
        }
        if may_create_a_view_or_trigger(sql)
            && matches!(
                turso_mysql_parser::parse_schema_ddl_ast(sql, connection.parser_mode()),
                Ok(turso_parser::ast::Stmt::CreateView { .. }
                    | turso_parser::ast::Stmt::CreateTrigger { .. })
            )
        {
            if let Some(username) = self
                .authorizer
                .schema_creator_username(&self.principal)
                .map_err(authorization_frontend_error)?
            {
                let creator = SchemaSqlCreator::new(
                    username,
                    crate::session_variables::reported_sql_mode(connection.parser_mode()),
                );
                connection
                    .execute_schema_object_ddl_with_creator(sql, creator)
                    .map_err(frontend_query_error)?;
                return Ok(CommandExecutionResult::Ok(CommandOkResult {
                    status_flags: connection_status_flags(connection),
                    ..CommandOkResult::default()
                }));
            }
        }
        let affected_rows_mode = if self.command_options.client_found_rows() {
            MySqlAffectedRowsMode::Matched
        } else {
            MySqlAffectedRowsMode::Changed
        };
        let mut result = execute_checked_query(
            connection,
            sql,
            Some(&selected_database),
            &source_tables,
            CheckedQueryOptions {
                query_timeout: self.query_timeout,
                select_time_limit: self.session_variables.select_time_limit(),
                affected_rows_mode,
                sql_notes: self.session_variables.sql_notes(),
                group_concat_max_len: self.session_variables.group_concat_max_len(),
                raised: &mut self.raised_warnings,
            },
        )?;
        if let CommandExecutionResult::ResultSet(rows) = &mut result {
            shift_text_timestamp_columns(connection, rows)?;
            apply_raw_column_collations(
                connection,
                &mut rows.columns,
                self.session_variables.raw_character_set_results(),
            )?;
        }
        Ok(result)
    }

    /// Prepares one statement through the checked prepared path.
    fn prepare_checked_database_statement(
        &mut self,
        sql: &str,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        let selected_database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?
            .to_owned();
        let (source_tables, visibility) = self.authorize_query_text(&selected_database, sql)?;
        if matches!(visibility, CatalogVisibility::GrantedTables)
            && source_tables
                .iter()
                .any(|source| source.catalog() == Some(MySqlCatalogTable::Views))
        {
            return Err(FrontendErrorKind::AccessDenied);
        }
        let connection = self
            .session
            .connection()
            .map_err(database_error_kind)?
            .clone();
        connection.set_group_concat_max_len(self.session_variables.group_concat_max_len());
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
            Some(sql),
            Some(&selected_database),
            &source_tables,
        )
        .and_then(|mut result| {
            apply_raw_column_collations(
                &connection,
                &mut result.columns,
                self.session_variables.raw_character_set_results(),
            )?;
            Ok(result)
        });
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
                read_only_select: parse_select(sql, self.session.session_sql_mode())
                    .is_ok_and(|select| !select.locks_rows()),
                selects: parse_select(sql, self.session.session_sql_mode()).is_ok(),
                connection,
                connection_statement_id,
                parameter_types: None,
                catalog_query: None,
                runs_as_text: None,
                sql: Some(sql.to_owned()),
                group_concat_max_len: self.session_variables.group_concat_max_len(),
            },
        );
        result
    }

    /// Retains a statement with no parameters and no rows that the checked
    /// prepared path does not take — Laravel prepares every statement,
    /// `CREATE TABLE` included — to be run through the text path when it is
    /// executed, which is what executing it means.
    fn prepare_text_statement(
        &mut self,
        sql: &str,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        let database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?
            .to_owned();
        let connection = self
            .session
            .connection()
            .map_err(database_error_kind)?
            .clone();
        // An engine-owned statement holds this one's place in the same
        // prepared-statement quota and lifecycle as any other.
        let reserved = connection
            .prepare_checked_statement("SELECT 1")
            .map_err(prepared_statement_error)?;
        let Some(statement_id) = self.prepared_statements.next_statement_id else {
            connection.remove_prepared_statement(reserved.statement_id);
            return Err(FrontendErrorKind::Internal);
        };
        self.prepared_statements.next_statement_id = statement_id.checked_add(1);
        self.prepared_statements.statements.insert(
            statement_id,
            DatabasePreparedStatement {
                database,
                source_tables: Vec::new(),
                read_only_select: false,
                selects: false,
                connection,
                connection_statement_id: reserved.statement_id,
                parameter_types: None,
                catalog_query: None,
                runs_as_text: Some(sql.to_owned()),
                sql: None,
                group_concat_max_len: self.session_variables.group_concat_max_len(),
            },
        );
        Ok(PreparedStatementResult {
            statement_id,
            parameters: Vec::new(),
            columns: Vec::new(),
            warnings: 0,
            status_flags: self.status_flags(),
        })
    }

    fn execute_prepared_statement_command(
        &mut self,
        statement_id: u32,
        parameter_payload: &[u8],
    ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
        if self
            .prepared_statements
            .statements
            .get(&statement_id)
            .and_then(|statement| statement.catalog_query)
            .is_some()
        {
            return self.execute_gorm_catalog_query(statement_id, parameter_payload);
        }
        if let Some(sql) = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .and_then(|statement| statement.runs_as_text.clone())
        {
            self.pending_long_data.take_statement(statement_id);
            return match self.execute_query_statement(&sql)? {
                CommandExecutionResult::Ok(result) => {
                    Ok(PreparedStatementExecutionResult::Ok(result))
                }
                CommandExecutionResult::ResultSet(_) => Err(FrontendErrorKind::Internal),
            };
        }
        let (database, source_tables, read_only_select) = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .map(|statement| {
                (
                    statement.database.clone(),
                    statement.source_tables.clone(),
                    statement.read_only_select,
                )
            })
            .ok_or(FrontendErrorKind::UnknownPreparedStatement)?;
        self.authorize_prepared_query(&database, &source_tables, read_only_select)?;
        let session_catalogs = source_tables
            .iter()
            .filter_map(MySqlSelectSource::catalog)
            .filter(|catalog| catalog.rows_come_from_the_session())
            .collect::<Vec<_>>();
        if !session_catalogs.is_empty() {
            // The rows are worked out over the database the session is in,
            // which a statement prepared in another one does not read.
            if self.session.selected_database() != Some(database.as_str()) {
                return Err(FrontendErrorKind::Unsupported);
            }
            // A statement reading an `information_schema` table was
            // authorized under the database-wide grant, which sees every
            // table.
            for catalog in session_catalogs {
                self.publish_catalog_rows(&database, CatalogVisibility::All, catalog)?;
            }
        }
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
        let timeout = if statement.selects {
            the_shorter_limit(
                self.query_timeout,
                self.session_variables.select_time_limit(),
            )
        } else {
            self.query_timeout
        };
        statement.group_concat_max_len = statement
            .group_concat_max_len
            .max(self.session_variables.group_concat_max_len());
        statement
            .connection
            .set_group_concat_max_len(statement.group_concat_max_len);
        statement.connection.forget_group_concat_cuts();
        self.raised_warnings.clear();
        let mut result = execute_database_prepared_statement(
            statement,
            parameter_payload,
            long_data,
            timeout,
            affected_rows_mode,
        )?;
        self.raised_warnings.extend(
            statement
                .connection
                .take_group_concat_cuts()
                .into_iter()
                .map(MySqlWarning::cut_by_group_concat),
        );
        if let PreparedStatementExecutionResult::ResultSet(rows) = &mut result {
            apply_raw_column_collations(
                &statement.connection,
                &mut rows.columns,
                self.session_variables.raw_character_set_results(),
            )?;
            rows.warnings = u16::try_from(self.raised_warnings.len()).unwrap_or(u16::MAX);
        }
        Ok(result)
    }
}

#[cfg(unix)]
fn list_gorm_catalog_columns(
    connection: &MySqlConnection,
    table: &MySqlTableName,
) -> Result<Vec<MySqlColumnMetadata>, FrontendErrorKind> {
    match connection.list_columns(table) {
        Ok(columns) => Ok(columns),
        Err(MySqlColumnMetadataError::TableNotFound) => Ok(Vec::new()),
        Err(error) => Err(column_metadata_error_kind(error)),
    }
}

fn is_internal_catalog_table(table: &str) -> bool {
    turso_core::schema::is_system_table(table)
        || table
            .get(.."mysql_information_schema_".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mysql_information_schema_"))
}

fn is_internal_catalog_source(source: &MySqlSelectSource) -> bool {
    source.catalog().is_none() && is_internal_catalog_table(source.table().as_str())
}

fn is_internal_catalog_select(sql: &str) -> bool {
    statement_read_tables(sql)
        .iter()
        .any(is_internal_catalog_source)
}

/// Returns every table a statement reads.
///
/// A `SELECT` names them directly. An `INSERT ... SELECT` reads a table too, and
/// it is not a `SELECT`, so asking only the SELECT parser would answer nothing
/// and leave that table unauthorized and unchecked against the internal
/// catalog.
fn statement_read_tables(sql: &str) -> Vec<MySqlSelectSource> {
    if let Ok(translated) = parse_select(sql, SessionSqlMode::default()) {
        return translated.source_tables().to_vec();
    }
    turso_mysql_parser::parse_dml(sql, SessionSqlMode::default())
        .map(|translated| translated.read_tables().to_vec())
        .unwrap_or_default()
}

#[cfg(unix)]
fn parsed_source_tables(sql: &str) -> Vec<MySqlSelectSource> {
    statement_read_tables(sql)
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
fn is_account_admin_statement(sql: &str) -> bool {
    let mut words = sql.split_ascii_whitespace();
    match words.next() {
        Some(word) if word.eq_ignore_ascii_case("GRANT") || word.eq_ignore_ascii_case("REVOKE") => {
            true
        }
        Some(word) if word.eq_ignore_ascii_case("CREATE") => words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("USER")),
        _ => false,
    }
}

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
    /// How long a `SELECT` may run, which the session sets with
    /// `max_execution_time`; the shorter of this and the query timeout
    /// holds.
    select_time_limit: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
    sql_notes: bool,
    /// How many bytes a `GROUP_CONCAT` answers before it is cut, which the
    /// session sets with `group_concat_max_len`.
    group_concat_max_len: u64,
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
        select_time_limit,
        affected_rows_mode,
        sql_notes,
        group_concat_max_len,
        raised,
    } = options;
    let sql = strip_leading_sql_comments(sql);
    connection.set_group_concat_max_len(group_concat_max_len);
    connection.forget_group_concat_cuts();
    if let Some(command) = parse_optional_drop_table(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Syntax)?
    {
        let result = connection
            .drop_table(&command)
            .map_err(|error| match error {
                MySqlDropTableError::MissingTable => FrontendErrorKind::UnknownTable,
                MySqlDropTableError::NamedTwice => FrontendErrorKind::NotUniqueTable,
                MySqlDropTableError::Engine(error) => frontend_error_kind(error),
            })?;
        // Measured on MySQL 8.4.11: one note for each table an `IF EXISTS`
        // named that was not there.
        let noted = if sql_notes { result.missing.len() } else { 0 };
        if sql_notes {
            for table in &result.missing {
                raised.push(MySqlWarning::unknown_table(selected_database, table));
            }
        }
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            warnings: u16::try_from(noted).unwrap_or(u16::MAX),
            ..CommandOkResult::default()
        }));
    }
    if let Some(command) = parse_optional_truncate_table(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Syntax)?
    {
        connection
            .truncate_table(&command)
            .map_err(|error| match error {
                MySqlTruncateTableError::MissingTable => FrontendErrorKind::UnknownTable,
                MySqlTruncateTableError::ReferencedByForeignKey => {
                    FrontendErrorKind::TruncateReferencedByForeignKey
                }
                MySqlTruncateTableError::Engine(error) => frontend_error_kind(error),
            })?;
        // Measured on MySQL 8.4.11: `ROW_COUNT()` after a `TRUNCATE TABLE` is
        // 0, whatever the table held.
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if let Some(table) =
        turso_mysql_parser::parse_optional_alter_table_keys(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
    {
        // Measured on MySQL 8.4.11: InnoDB keeps no switch for its keys, so
        // both answer OK with note 1031 over a table, 1146 over a name that is
        // not there, and 1347 over a view, which is refused here.
        let listed = connection
            .list_tables()
            .map_err(frontend_error_kind)?
            .into_iter()
            .find(|listed| listed.name().eq_ignore_ascii_case(table.as_str()));
        match listed.map(|listed| listed.kind()) {
            None => return Err(FrontendErrorKind::MissingObject),
            Some(MySqlTableKind::View) => return Err(FrontendErrorKind::Unsupported),
            Some(MySqlTableKind::BaseTable) => {}
        }
        if sql_notes {
            raised.push(MySqlWarning::keys_have_no_switch(table.as_str()));
        }
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            warnings: u16::from(sql_notes),
            ..CommandOkResult::default()
        }));
    }
    if let Some(name) =
        turso_mysql_parser::parse_optional_mysqldump_drop_view(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Syntax)?
    {
        match connection.drop_view(&name) {
            Ok(()) | Err(turso_mysql::MySqlDropViewError::MissingView) => {}
            Err(turso_mysql::MySqlDropViewError::NotView) => {
                return Err(FrontendErrorKind::NotView)
            }
            Err(turso_mysql::MySqlDropViewError::NamedTwice) => {
                unreachable!("one dropped view is never named twice")
            }
            Err(turso_mysql::MySqlDropViewError::Engine(error)) => {
                return Err(frontend_error_kind(error))
            }
        }
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if let Some(command) = parse_optional_drop_view(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Syntax)?
    {
        let skipped = connection
            .drop_views(&command)
            .map_err(|error| match error {
                turso_mysql::MySqlDropViewError::MissingView => FrontendErrorKind::UnknownView,
                turso_mysql::MySqlDropViewError::NotView => FrontendErrorKind::NotView,
                turso_mysql::MySqlDropViewError::NamedTwice => FrontendErrorKind::NotUniqueTable,
                turso_mysql::MySqlDropViewError::Engine(error) => frontend_error_kind(error),
            })?;
        // Measured on MySQL 8.4.11: one note for each name an `IF EXISTS`
        // passed over, in the order the statement named them.
        let noted = if sql_notes { skipped.len() } else { 0 };
        if sql_notes {
            for skipped in &skipped {
                raised.push(match skipped {
                    turso_mysql::MySqlSkippedView::Missing(view) => {
                        MySqlWarning::unknown_table(selected_database, view)
                    }
                    turso_mysql::MySqlSkippedView::NotView(table) => {
                        MySqlWarning::not_a_view(selected_database, table)
                    }
                });
            }
        }
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            warnings: u16::try_from(noted).unwrap_or(u16::MAX),
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
            let outcome = connection
                .execute_transaction_command(sql)
                .map_err(frontend_query_error)?;
            if outcome.consistent_snapshot_ignored {
                raised.push(MySqlWarning::consistent_snapshot_ignored());
            }
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: connection_status_flags(connection),
                warnings: u16::from(outcome.consistent_snapshot_ignored),
                ..CommandOkResult::default()
            }));
        }
        Ok(false) => {}
        Err(_) => return Err(FrontendErrorKind::Unsupported),
    }
    if let Some((table, comment)) = table_comment_change(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Unsupported)?
    {
        connection
            .execute_table_comment(&table, comment)
            .map_err(|error| match error {
                MySqlQueryError::MissingTable => FrontendErrorKind::MissingObject,
                error => frontend_query_error(error),
            })?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if let Some(table) = table_engine_restated(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Unsupported)?
    {
        connection
            .execute_table_engine_restated(&table)
            .map_err(|error| match error {
                MySqlQueryError::MissingTable => FrontendErrorKind::MissingObject,
                error => frontend_query_error(error),
            })?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if let Some((table, next)) = table_counter_change(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Unsupported)?
    {
        connection
            .execute_table_counter_change(&table, next)
            .map_err(|error| match error {
                MySqlQueryError::MissingTable => FrontendErrorKind::MissingObject,
                error => frontend_query_error(error),
            })?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if let Some(pairs) = renamed_tables(sql) {
        connection
            .execute_rename_tables(&pairs)
            .map_err(|error| match error {
                // Measured on MySQL 8.4.11: 1146 for a table that is not
                // there and 1050 for a new name that is taken, a view's
                // among them.
                MySqlRenameTableError::MissingTable => FrontendErrorKind::MissingObject,
                MySqlRenameTableError::NameTaken => FrontendErrorKind::DuplicateObject,
                MySqlRenameTableError::Query(error) => frontend_query_error(error),
            })?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if let Some(like) = parse_optional_create_table_like(sql, connection.parser_mode())
        .map_err(|_| FrontendErrorKind::Unsupported)?
    {
        // Measured on MySQL 8.4.11: the source is looked at first — 1146 when
        // it is not there and 1347 when it is a view, even with `IF NOT
        // EXISTS` and a new name that is taken — then 1066 when both names
        // are the same table, and only then 1050 for a new name that is taken.
        let copy = connection
            .create_table_like_statement(&like)
            .map_err(|error| match error {
                MySqlShowCreateTableError::MissingTable => FrontendErrorKind::MissingObject,
                MySqlShowCreateTableError::NotTable => FrontendErrorKind::NotBaseTable,
                MySqlShowCreateTableError::Unsupported => FrontendErrorKind::Unsupported,
                MySqlShowCreateTableError::Engine(error) => frontend_error_kind(error),
            })?;
        if like
            .table()
            .as_str()
            .eq_ignore_ascii_case(like.source().as_str())
        {
            return Err(FrontendErrorKind::NotUniqueTable);
        }
        let copy = if like.only_if_missing() {
            copy.replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1)
        } else {
            copy
        };
        return execute_checked_query(
            connection,
            &copy,
            selected_database,
            source_tables,
            CheckedQueryOptions {
                query_timeout,
                select_time_limit,
                affected_rows_mode,
                sql_notes,
                group_concat_max_len,
                raised,
            },
        );
    }
    if is_schema_statement(sql) {
        if let Some(written) = connection
            .with_the_database_collation(sql)
            .map_err(frontend_query_error)?
        {
            return execute_checked_query(
                connection,
                &written,
                selected_database,
                source_tables,
                CheckedQueryOptions {
                    query_timeout,
                    select_time_limit,
                    affected_rows_mode,
                    sql_notes,
                    group_concat_max_len,
                    raised,
                },
            );
        }
        turso_mysql_parser::refuse_checks_numbered_out_of_order(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Unsupported)?;
        // MySQL answers a name that is already there before it looks at
        // anything else, so the name is looked up before anything runs:
        // measured, 1050 as an error without `IF NOT EXISTS` and as a note
        // with it, both leaving the table exactly as it stands.
        if let Some(created) = parse_optional_created_table(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Unsupported)?
            .filter(|created| !created.temporary())
        {
            if connection
                .names_a_table(created.table())
                .map_err(frontend_error_kind)?
            {
                if !created.only_if_missing() {
                    return Err(FrontendErrorKind::DuplicateObject);
                }
                let noted = sql_notes;
                if noted {
                    raised.push(MySqlWarning::table_exists(created.table().as_str()));
                }
                return Ok(CommandExecutionResult::Ok(CommandOkResult {
                    status_flags: connection_status_flags(connection),
                    warnings: u16::from(noted),
                    ..CommandOkResult::default()
                }));
            }
        }
        if let Some(checked) = parse_optional_create_table_as_select(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Unsupported)?
        {
            let affected_rows = connection
                .execute_create_table_as_select(&checked)
                .map_err(|error| match error {
                    MySqlCreateTableAsSelectError::MissingTable => FrontendErrorKind::UnknownTable,
                    MySqlCreateTableAsSelectError::MissingColumn => {
                        FrontendErrorKind::UnknownColumn
                    }
                    MySqlCreateTableAsSelectError::UnsupportedColumn => {
                        FrontendErrorKind::Unsupported
                    }
                    MySqlCreateTableAsSelectError::Query(error) => frontend_query_error(error),
                    MySqlCreateTableAsSelectError::Engine(error) => frontend_error_kind(error),
                })?;
            // Measured on MySQL 8.4.11: `ROW_COUNT()` after one is the number
            // of rows it copied.
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                affected_rows,
                status_flags: connection_status_flags(connection),
                ..CommandOkResult::default()
            }));
        }
        if let Some(checked) = parse_optional_create_table_with_keys(sql, connection.parser_mode())
            .map_err(|error| match error {
                turso_mysql_parser::ParseError::JsonLiteralDefault => {
                    FrontendErrorKind::JsonLiteralDefault
                }
                turso_mysql_parser::ParseError::JsonIndex => FrontendErrorKind::JsonIndex,
                _ => FrontendErrorKind::Unsupported,
            })?
        {
            connection
                .execute_create_table_with_keys(&checked)
                .map_err(frontend_query_error)?;
            return Ok(CommandExecutionResult::Ok(CommandOkResult {
                status_flags: connection_status_flags(connection),
                ..CommandOkResult::default()
            }));
        }
        if let Some(checked) = parse_optional_alter_table_indexes(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Unsupported)?
        {
            connection
                .execute_alter_table_indexes(&checked)
                .map_err(|error| match error {
                    MySqlAlterTableIndexError::MissingTable => FrontendErrorKind::UnknownTable,
                    // Measured on MySQL 8.4.11: 1091 for a `DROP INDEX` naming
                    // an index that is not there, 1061 for an `ADD INDEX`
                    // naming one that is.
                    MySqlAlterTableIndexError::MissingIndex => FrontendErrorKind::CantDropKey,
                    // Measured on MySQL 8.4.11: 1176 for a `RENAME INDEX`
                    // naming an index that is not there, and 1061 for one
                    // naming a new name that is.
                    MySqlAlterTableIndexError::MissingIndexToRename => {
                        FrontendErrorKind::KeyDoesNotExist
                    }
                    MySqlAlterTableIndexError::DuplicateIndex => {
                        FrontendErrorKind::DuplicateKeyName
                    }
                    MySqlAlterTableIndexError::RenamingAColumnsOwnKey => {
                        FrontendErrorKind::Unsupported
                    }
                    MySqlAlterTableIndexError::JsonIndex => FrontendErrorKind::JsonIndex,
                    MySqlAlterTableIndexError::RequiredByForeignKey => {
                        FrontendErrorKind::RequiredForeignKeyIndex
                    }
                    MySqlAlterTableIndexError::Engine(error) => frontend_error_kind(error),
                })?;
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
            the_shorter_limit(query_timeout, select_time_limit),
        )?;
        result.status_flags = connection_status_flags(connection);
        raised.extend(
            connection
                .take_group_concat_cuts()
                .into_iter()
                .map(MySqlWarning::cut_by_group_concat),
        );
        result.warnings = u16::try_from(raised.len()).unwrap_or(u16::MAX);
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
    let result = prepared_statement_result(connection, metadata, &type_metadata, None, None, &[]);
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
        statement.sql.as_deref(),
        Some(statement.database.as_str()),
        &statement.source_tables,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_prepared_values(
    connection: &MySqlConnection,
    statement_id: u32,
    values: Vec<MySqlPreparedValue>,
    timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
    sql: Option<&str>,
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (sql, selected_database, source_tables);
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
    let result_column_count = metadata.result_columns.len();
    #[cfg(unix)]
    let source_metadata = prepared_table_result_metadata(
        connection,
        &type_metadata,
        selected_database,
        source_tables,
    )?;
    #[cfg(unix)]
    let projection = ProjectionOrigins::read(connection, &type_metadata, sql, source_tables)?;
    let columns = metadata
        .result_columns
        .into_iter()
        .enumerate()
        .zip(&column_types)
        .map(|((index, column), column_type)| {
            if type_metadata[index].is_last_insert_id_result() {
                return Ok(last_insert_id_column_definition(column.name));
            }
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
                if let Some(definition) = projection.windowed_column_definition(
                    source_metadata,
                    result_column_count,
                    index,
                    &column.name,
                    *column_type,
                )? {
                    return Ok(definition);
                }
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
    #[cfg(unix)]
    let columns = projection.shape_compound_columns(columns, source_metadata.as_ref())?;
    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .zip(&columns)
                .map(|(value, column)| {
                    let value = shift_binary_timestamp_value(connection, value, column)?;
                    binary_result_value(value, column)
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

fn shift_binary_timestamp_value(
    connection: &MySqlConnection,
    value: MySqlPreparedValue,
    column: &ColumnDefinitionConfig,
) -> Result<MySqlPreparedValue, FrontendErrorKind> {
    let offset = connection.time_zone_offset_seconds();
    if offset == 0 || column.column_type != MYSQL_TYPE_TIMESTAMP {
        return Ok(value);
    }
    if column.original_table.is_empty() || column.original_name.is_empty() {
        return Err(FrontendErrorKind::Unsupported);
    }
    match value {
        MySqlPreparedValue::Text(written) => Ok(MySqlPreparedValue::Text(
            turso_mysql::shift_timestamp(&written, offset).ok_or(FrontendErrorKind::Unsupported)?,
        )),
        MySqlPreparedValue::Null => Ok(MySqlPreparedValue::Null),
        _ => Err(FrontendErrorKind::Unsupported),
    }
}

fn shift_text_timestamp_columns(
    connection: &MySqlConnection,
    result: &mut TextResultSet,
) -> Result<(), FrontendErrorKind> {
    let offset = connection.time_zone_offset_seconds();
    if offset == 0 {
        return Ok(());
    }
    for (index, column) in result.columns.iter().enumerate() {
        if column.column_type != MYSQL_TYPE_TIMESTAMP {
            continue;
        }
        if column.original_table.is_empty() || column.original_name.is_empty() {
            return Err(FrontendErrorKind::Unsupported);
        }
        for row in &mut result.rows {
            if let Some(written) = &mut row[index] {
                let text = std::str::from_utf8(written).map_err(|_| FrontendErrorKind::Internal)?;
                *written = turso_mysql::shift_timestamp(text, offset)
                    .ok_or(FrontendErrorKind::Unsupported)?
                    .into_bytes();
            }
        }
    }
    Ok(())
}

fn statement_parameter_to_frontend(value: StatementParameterValue) -> MySqlPreparedValue {
    match value {
        StatementParameterValue::Null => MySqlPreparedValue::Null,
        StatementParameterValue::Integer(value) => MySqlPreparedValue::Integer(value),
        StatementParameterValue::UnsignedInteger(value) => {
            MySqlPreparedValue::UnsignedInteger(value)
        }
        StatementParameterValue::Float(value) => MySqlPreparedValue::Real(f64::from(value)),
        StatementParameterValue::Double(value) => MySqlPreparedValue::Real(value),
        StatementParameterValue::String(value) => MySqlPreparedValue::Text(value),
        StatementParameterValue::Bytes(value) => MySqlPreparedValue::Blob(value),
    }
}

fn apply_raw_column_collations(
    connection: &MySqlConnection,
    columns: &mut [ColumnDefinitionConfig],
    raw_character_set_results: bool,
) -> Result<(), FrontendErrorKind> {
    if !raw_character_set_results {
        return Ok(());
    }
    let mut tables = HashMap::<String, Vec<turso_mysql::MySqlColumnMetadata>>::new();
    for definition in columns {
        if definition.character_set != u16::from(DEFAULT_UTF8MB4_COLLATION)
            || definition.original_table.is_empty()
            || definition.original_name.is_empty()
            || definition.schema.eq_ignore_ascii_case("information_schema")
        {
            continue;
        }
        let table_name = definition.original_table.as_str();
        if !tables.contains_key(table_name) {
            let parsed = turso_mysql_parser::MySqlTableName::parse(table_name)
                .map_err(|_| FrontendErrorKind::Unsupported)?;
            let metadata = connection
                .list_columns(&parsed)
                .map_err(|_| FrontendErrorKind::Unsupported)?;
            tables.insert(table_name.to_owned(), metadata);
        }
        let column = tables[table_name]
            .iter()
            .find(|column| {
                column
                    .name()
                    .eq_ignore_ascii_case(&definition.original_name)
            })
            .ok_or(FrontendErrorKind::Unsupported)?;
        definition.character_set = match column.collation_name() {
            Some("utf8mb4_0900_ai_ci") => 255,
            Some("utf8mb4_bin") => 46,
            Some("utf8mb4_unicode_ci") => 224,
            _ => return Err(FrontendErrorKind::Unsupported),
        };
    }
    Ok(())
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
        MySqlPreparedValue::Integer(_) | MySqlPreparedValue::UnsignedInteger(_) => {
            Some(MYSQL_TYPE_LONGLONG)
        }
        MySqlPreparedValue::Real(_) => Some(MYSQL_TYPE_DOUBLE),
        MySqlPreparedValue::Text(_) => Some(MYSQL_TYPE_VAR_STRING),
        MySqlPreparedValue::Blob(_) => Some(MYSQL_TYPE_BLOB),
    }
}

fn binary_result_value(
    value: MySqlPreparedValue,
    column: &ColumnDefinitionConfig,
) -> Result<BinaryResultValue, FrontendErrorKind> {
    let column_type = column.column_type;
    let decimals = column.decimals;
    if is_exact_decimal_column(column)
        && !matches!(
            &value,
            MySqlPreparedValue::Null | MySqlPreparedValue::Text(_)
        )
    {
        return Err(FrontendErrorKind::Internal);
    }
    match value {
        MySqlPreparedValue::Null => Ok(BinaryResultValue::Null),
        MySqlPreparedValue::Text(value)
            if column_type == MYSQL_TYPE_LONGLONG && column.flags & MYSQL_UNSIGNED_FLAG != 0 =>
        {
            let parsed = value
                .parse::<u64>()
                .map_err(|_| FrontendErrorKind::Internal)?;
            Ok(BinaryResultValue::UnsignedInteger(parsed))
        }
        MySqlPreparedValue::Integer(value)
            if column_type == MYSQL_TYPE_LONGLONG && column.flags & MYSQL_UNSIGNED_FLAG != 0 =>
        {
            let parsed = u64::try_from(value).map_err(|_| FrontendErrorKind::Internal)?;
            Ok(BinaryResultValue::UnsignedInteger(parsed))
        }
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
            BinaryResultValue::Text(format_mysql_scaled_integer(value, decimals)),
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
        // A DATE crosses in the same field form with the time left off, which
        // is the four-byte length MySQL sends for one.
        MySqlPreparedValue::Text(value) if column_type == MYSQL_TYPE_DATE => {
            binary_result_date(&value)
        }
        MySqlPreparedValue::Text(value) if column_type == MYSQL_TYPE_TIME => {
            binary_result_time(&value)
        }
        // A YEAR is held as the number it is, so it crosses as one.
        MySqlPreparedValue::Integer(value) if column_type == MYSQL_TYPE_YEAR => {
            Ok(BinaryResultValue::Integer(value))
        }
        // A BIT crosses as the bytes that hold its bits, length-encoded, the
        // same bytes the text protocol sends.
        MySqlPreparedValue::Integer(value) if column_type == MYSQL_TYPE_BIT => {
            Ok(BinaryResultValue::Blob(vec![
                bit_byte(value).map_err(|_| FrontendErrorKind::Internal)?
            ]))
        }
        // A JSON column crosses the same way a BLOB does — measured on MySQL
        // 8.4.11, the document's own bytes, length-encoded, and the same over
        // both protocols.
        MySqlPreparedValue::Blob(value)
            if matches!(
                column_type,
                MYSQL_TYPE_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_JSON
            ) =>
        {
            Ok(BinaryResultValue::Blob(value))
        }
        // A binary string written out — `0x41`, `b'101'` — crosses as its
        // bytes, length-encoded, the way a VARCHAR's text does.
        MySqlPreparedValue::Blob(value)
            if column_type == MYSQL_TYPE_VAR_STRING
                && column.character_set == MYSQL_BINARY_COLLATION =>
        {
            Ok(BinaryResultValue::Blob(value))
        }
        // A TEXT column reports BLOB, and a GROUP_CONCAT a LONG_BLOB, so each
        // value crosses as the same length-encoded bytes.
        MySqlPreparedValue::Text(value)
            if matches!(
                column_type,
                MYSQL_TYPE_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_JSON
            ) =>
        {
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
    let (second, microseconds) = stored_second_and_microseconds(second)?;
    let (year, month, day, hour, minute) = (
        year.parse().map_err(|_| FrontendErrorKind::Internal)?,
        field(month)?,
        field(day)?,
        field(hour)?,
        field(minute)?,
    );
    Ok(if microseconds == 0 {
        BinaryResultValue::DateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    } else {
        BinaryResultValue::DateTimeMicros {
            year,
            month,
            day,
            hour,
            minute,
            second,
            microseconds,
        }
    })
}

/// Reads the `YYYY-MM-DD` a DATE column holds.
///
/// The text is the one this frontend wrote, so anything else means the row and
/// the column disagree about the type.
fn binary_result_date(value: &str) -> Result<BinaryResultValue, FrontendErrorKind> {
    let [year, month, day] = <[&str; 3]>::try_from(value.split('-').collect::<Vec<_>>())
        .map_err(|_| FrontendErrorKind::Internal)?;
    let field = |text: &str| text.parse::<u8>().map_err(|_| FrontendErrorKind::Internal);
    Ok(BinaryResultValue::DateTime {
        year: year.parse().map_err(|_| FrontendErrorKind::Internal)?,
        month: field(month)?,
        day: field(day)?,
        hour: 0,
        minute: 0,
        second: 0,
    })
}

/// Reads the `[-]HH:MM:SS` a TIME column holds.
///
/// A TIME runs to `838:59:59`, and the binary form carries the hours past a day
/// as whole days, so the hours are split here rather than sent as they were
/// written.
fn binary_result_time(value: &str) -> Result<BinaryResultValue, FrontendErrorKind> {
    let (negative, value) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let [hours, minutes, seconds] = <[&str; 3]>::try_from(value.split(':').collect::<Vec<_>>())
        .map_err(|_| FrontendErrorKind::Internal)?;
    let field = |text: &str| text.parse::<u8>().map_err(|_| FrontendErrorKind::Internal);
    let hours = hours
        .parse::<u32>()
        .map_err(|_| FrontendErrorKind::Internal)?;
    let (second, microseconds) = stored_second_and_microseconds(seconds)?;
    let (days, hour, minute) = (hours / 24, (hours % 24) as u8, field(minutes)?);
    Ok(if microseconds == 0 {
        BinaryResultValue::Time {
            negative,
            days,
            hour,
            minute,
            second,
        }
    } else {
        BinaryResultValue::TimeMicros {
            negative,
            days,
            hour,
            minute,
            second,
            microseconds,
        }
    })
}

fn stored_second_and_microseconds(written: &str) -> Result<(u8, u32), FrontendErrorKind> {
    let (second, fraction) = match written.split_once('.') {
        Some((second, fraction)) => (second, Some(fraction)),
        None => (written, None),
    };
    let second = second.parse().map_err(|_| FrontendErrorKind::Internal)?;
    let microseconds = match fraction {
        Some(fraction)
            if !fraction.is_empty()
                && fraction.len() <= 6
                && fraction.bytes().all(|digit| digit.is_ascii_digit()) =>
        {
            let value: u32 = fraction.parse().map_err(|_| FrontendErrorKind::Internal)?;
            value * 10_u32.pow(6 - fraction.len() as u32)
        }
        Some(_) => return Err(FrontendErrorKind::Internal),
        None => 0,
    };
    Ok((second, microseconds))
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
            MySqlPreparedValue::Integer(_)
            | MySqlPreparedValue::UnsignedInteger(_)
            | MySqlPreparedValue::Real(_) => 8,
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
    sql: Option<&str>,
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
) -> Result<PreparedStatementResult, FrontendErrorKind> {
    #[cfg(not(unix))]
    let _ = (sql, selected_database, source_tables);
    if metadata.result_columns.len() != type_metadata.len() {
        return Err(FrontendErrorKind::Internal);
    }
    let parameters = (0..metadata.parameter_count)
        .map(|index| column_definition(format!("?{}", index + 1), MYSQL_TYPE_NULL))
        .collect();
    #[cfg(unix)]
    let result_column_count = metadata.result_columns.len();
    #[cfg(unix)]
    let source_metadata = prepared_table_result_metadata(
        connection,
        type_metadata,
        selected_database,
        source_tables,
    )?;
    #[cfg(unix)]
    let projection = ProjectionOrigins::read(connection, type_metadata, sql, source_tables)?;
    let columns = metadata
        .result_columns
        .into_iter()
        .zip(type_metadata)
        .enumerate()
        .map(|(index, (column, type_metadata))| {
            #[cfg(not(unix))]
            let _ = index;
            if type_metadata.is_last_insert_id_result() {
                return Ok(last_insert_id_column_definition(column.name));
            }
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
                if let Some(definition) = projection.windowed_column_definition(
                    source_metadata,
                    result_column_count,
                    index,
                    &column.name,
                    column_type,
                )? {
                    return Ok(definition);
                }
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
    #[cfg(unix)]
    let columns = projection.shape_compound_columns(columns, source_metadata.as_ref())?;
    Ok(PreparedStatementResult {
        statement_id: metadata.statement_id,
        parameters,
        columns,
        warnings: 0,
        status_flags: connection_status_flags(connection),
    })
}

/// Where each result column of a window or a `UNION` comes from, read off the
/// statement's text.
///
/// The engine answers every column of a window out of its own sorter and every
/// column of a `UNION` out of the compound, so neither says which table column
/// it reads. Preparing and executing a statement both read it here, so the
/// columns executing answers are the ones preparing announced.
#[cfg(unix)]
struct ProjectionOrigins {
    windowed: bool,
    compound: bool,
    origins: Vec<Vec<MySqlSelectProjectionOrigin>>,
    drops_repeated_rows: bool,
}

#[cfg(unix)]
impl ProjectionOrigins {
    fn read(
        connection: &MySqlConnection,
        type_metadata: &[MySqlPreparedResultColumnTypeMetadata],
        sql: Option<&str>,
        source_tables: &[MySqlSelectSource],
    ) -> Result<Self, FrontendErrorKind> {
        let windowed = type_metadata
            .iter()
            .filter_map(MySqlPreparedResultColumnTypeMetadata::static_metadata)
            .any(is_window_call);
        let compound = source_tables.iter().any(|source| source.branch() > 0);
        let origins = if windowed || compound {
            sql.map(|sql| select_projection_origins(sql, connection.parser_mode()))
                .transpose()
                .map_err(|_| FrontendErrorKind::Unsupported)?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let drops_repeated_rows = match (compound, sql) {
            (true, Some(sql)) => compound_drops_repeated_rows(sql, connection.parser_mode())
                .map_err(|_| FrontendErrorKind::Unsupported)?,
            _ => false,
        };
        Ok(Self {
            windowed,
            compound,
            origins,
            drops_repeated_rows,
        })
    }

    /// The column a window's result column reads, without the key flags the
    /// sorter does not carry.
    fn windowed_column_definition(
        &self,
        source_metadata: &TableResultMetadata,
        result_column_count: usize,
        index: usize,
        name: &str,
        column_type: u8,
    ) -> Result<Option<ColumnDefinitionConfig>, FrontendErrorKind> {
        if !self.windowed || self.origins.len() != 1 || self.origins[0].len() != result_column_count
        {
            return Ok(None);
        }
        let MySqlSelectProjectionOrigin::Column {
            table,
            column: source,
        } = &self.origins[0][index]
        else {
            return Ok(None);
        };
        let Some((table, ordinal)) = source_metadata.projection_source(0, table.as_deref(), source)
        else {
            return Ok(None);
        };
        let mut definition = source_metadata.table_column_definition(
            table,
            ordinal,
            name.to_owned(),
            Some(column_type),
        )?;
        definition.flags &= !(MYSQL_PRI_KEY_FLAG
            | MYSQL_PART_KEY_FLAG
            | MYSQL_UNIQUE_KEY_FLAG
            | MYSQL_AUTO_INCREMENT_FLAG);
        Ok(Some(definition))
    }

    fn shape_compound_columns(
        &self,
        mut columns: Vec<ColumnDefinitionConfig>,
        source_metadata: Option<&TableResultMetadata>,
    ) -> Result<Vec<ColumnDefinitionConfig>, FrontendErrorKind> {
        if self.compound {
            apply_compound_shape(
                &mut columns,
                &self.origins,
                source_metadata,
                self.drops_repeated_rows,
            )?;
        }
        Ok(columns)
    }
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
        MySqlQueryError::DuplicateColumn(_) => FrontendErrorKind::DuplicateColumn,
        MySqlQueryError::DuplicateIndex => FrontendErrorKind::DuplicateKeyName,
        MySqlQueryError::MissingIndex => FrontendErrorKind::CantDropKey,
        MySqlQueryError::RequiredByForeignKey => FrontendErrorKind::RequiredForeignKeyIndex,
        MySqlQueryError::MissingTable => FrontendErrorKind::UnknownTable,
        MySqlQueryError::JsonIndex => FrontendErrorKind::JsonIndex,
        MySqlQueryError::JsonLiteralDefault => FrontendErrorKind::JsonLiteralDefault,
        MySqlQueryError::ReadOnlyTransaction => FrontendErrorKind::ReadOnlyTransaction,
        MySqlQueryError::NoSuchSavepoint => FrontendErrorKind::NoSuchSavepoint,
        MySqlQueryError::NoSuchCheck(_) => FrontendErrorKind::NoSuchCheck,
        MySqlQueryError::DuplicateCheckName(_) => FrontendErrorKind::DuplicateCheckName,
        MySqlQueryError::Syntax(_) => FrontendErrorKind::Syntax,
        MySqlQueryError::Unsupported(_) => FrontendErrorKind::Unsupported,
        MySqlQueryError::Engine(error) => frontend_error_kind(error),
    }
}

/// Refuses a statement that a session naming latin1 wrote outside ASCII, the
/// one range where latin1 and utf8mb4 read the same bytes alike.
fn refuse_what_latin1_reads_differently(
    session_variables: &crate::session_variables::MySqlSessionVariables,
    sql: &str,
) -> Result<(), FrontendErrorKind> {
    if session_variables.reads_statements_as_latin1() && !sql.is_ascii() {
        return Err(FrontendErrorKind::Unsupported);
    }
    Ok(())
}

/// Refuses a result the session asked for in latin1, which this server does
/// not convert its utf8mb4 text to.
fn refuse_a_result_in_latin1(
    session_variables: &crate::session_variables::MySqlSessionVariables,
    result: Result<CommandExecutionResult, FrontendErrorKind>,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if session_variables.wants_latin1_results()
        && matches!(result, Ok(CommandExecutionResult::ResultSet(_)))
    {
        return Err(FrontendErrorKind::Unsupported);
    }
    result
}

/// Refuses a prepared statement while the session names latin1 for anything:
/// its bound strings arrive in the client's character set and its rows go out
/// in the results', and neither is converted here.
fn refuse_a_prepared_statement_under_latin1(
    session_variables: &crate::session_variables::MySqlSessionVariables,
) -> Result<(), FrontendErrorKind> {
    if session_variables.reads_statements_as_latin1() || session_variables.wants_latin1_results() {
        return Err(FrontendErrorKind::Unsupported);
    }
    Ok(())
}

/// Readies a connection for one statement from the client.
///
/// A `READ COMMITTED` transaction reads what is committed as each statement
/// starts, a transaction the statement begins takes the level the session
/// asked for, and a 0 written into a counted column means what the session's
/// `sql_mode` says, so all three are settled here rather than by each
/// statement.
fn prepare_for_client_statement(
    connection: &MySqlConnection,
    session_variables: &crate::session_variables::MySqlSessionVariables,
) -> Result<(), FrontendErrorKind> {
    let written_zero = if session_variables.no_auto_value_on_zero() {
        turso_mysql_parser::WrittenZero::Stored
    } else {
        turso_mysql_parser::WrittenZero::AsksForTheNextNumber
    };
    connection
        .prepare_for_client_statement(
            session_variables.isolation_for_next_transaction(),
            written_zero,
        )
        .map_err(frontend_query_error)
}

/// Whether a statement could be a `CREATE VIEW`, an `ALTER VIEW` or a
/// `CREATE TRIGGER`, which every one names in so many words. Parsing a statement as schema DDL to find
/// out costs as much as running a primary-key `SELECT`, so the rest skip it.
fn may_create_a_view_or_trigger(sql: &str) -> bool {
    [b"VIEW".as_slice(), b"TRIGGER".as_slice()]
        .iter()
        .any(|word| {
            sql.as_bytes()
                .windows(word.len())
                .any(|window| window.eq_ignore_ascii_case(word))
        })
}

/// Settles what one statement from the client left behind.
///
/// A level set for the next transaction alone is used up once one begins, and
/// a transaction that could not finish is rolled back the way MySQL rolls one
/// back before answering 1213.
/// A statement that finished with the WAL grown past its bound empties it.
fn finish_client_statement<T>(
    connection: &MySqlConnection,
    session_variables: &mut crate::session_variables::MySqlSessionVariables,
    result: Result<T, FrontendErrorKind>,
) -> Result<T, FrontendErrorKind> {
    if result.is_ok() {
        connection
            .keep_the_wal_small()
            .map_err(frontend_error_kind)?;
    }
    match &result {
        Ok(_) if connection.began_transaction() => {
            session_variables.use_up_next_transaction_isolation();
        }
        Err(FrontendErrorKind::SerializationFailure) => {
            connection
                .roll_back_after_serialization_failure()
                .map_err(frontend_query_error)?;
        }
        _ => {}
    }
    result
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

/// Refuses a `SELECT` of a system variable the session reader left unanswered.
///
/// The reader answers every variable this server has an honest answer for, so
/// what reaches here names one it does not have. Answering it with a value the
/// server does not keep would have the client behave on a setting that is not
/// there.
fn refuse_an_unknown_system_variable(sql: &str) -> Result<(), FrontendErrorKind> {
    if contains_unrecognized_system_variable(sql) {
        return Err(FrontendErrorKind::Unsupported);
    }
    Ok(())
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

/// The limit that stops a statement first, of two it may be held to.
fn the_shorter_limit(first: Option<Duration>, second: Option<Duration>) -> Option<Duration> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (limit, None) | (None, limit) => limit,
    }
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
            let Some(primitive) = primitive else {
                return Ok(None);
            };
            match mysql_type_for_name(&primitive) {
                Some(column_type) => Ok(Some(column_type)),
                // A projection whose shape the statement already fixes does
                // not need the engine's own name for it: `SUM(n) + 1` is
                // reported as NUMERIC, which names no MySQL type, and its
                // shape comes from the arithmetic rule instead.
                None if static_result_metadata.len() == column_count
                    && static_result_metadata[index].is_some() =>
                {
                    Ok(None)
                }
                None => Err(FrontendErrorKind::Unsupported),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    // A window makes the engine answer every column out of its own sorter, so
    // the source reference it reports names that rather than the table. There
    // is no provenance left to report, and reporting the wrong one would be
    // worse than reporting none.
    #[cfg(unix)]
    let windowed = static_result_metadata.iter().flatten().any(is_window_call);
    #[cfg(unix)]
    let compound = source_tables.iter().any(|source| source.branch() > 0);
    #[cfg(unix)]
    let projection_origins = if windowed || compound {
        select_projection_origins(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Unsupported)?
    } else {
        Vec::new()
    };
    #[cfg(unix)]
    let drops_repeated_rows = compound
        && compound_drops_repeated_rows(sql, connection.parser_mode())
            .map_err(|_| FrontendErrorKind::Unsupported)?;
    #[cfg(unix)]
    let source_references = if windowed {
        Vec::new()
    } else {
        // A column whose shape the statement fixes is not looked up through
        // its source, which may be a table the statement made up for itself.
        (0..statement.num_columns())
            .filter(|index| {
                static_result_metadata.len() != column_count
                    || static_result_metadata[*index].is_none()
            })
            .filter_map(|index| {
                statement
                    .get_column_source_reference(index)
                    .map(|(table, ordinal)| (table.into_owned(), ordinal))
            })
            .collect::<Vec<_>>()
    };
    #[cfg(unix)]
    let source_metadata = table_result_metadata_for_references(
        connection,
        &source_references,
        selected_database,
        source_tables,
        // A `LAG` reads a column and the engine points at the window's sorter,
        // so the table has to be looked up even though nothing points at it.
        windowed
            || compound
            || static_result_metadata
                .iter()
                .flatten()
                .any(needs_source_columns),
    )?;

    let columns = (0..column_count)
        .map(|index| {
            let name = statement.get_column_name(index).into_owned();
            if connection.is_last_insert_id_result(&statement, index) {
                return Ok(last_insert_id_column_definition(name));
            }
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
                        if windowed {
                            // The sorter drops source references. A result
                            // name that uniquely names a source column still
                            // has enough information to restore its table and
                            // nullability. MySQL drops its key flags here.
                            if projection_origins.len() == 1
                                && projection_origins[0].len() == column_count
                            {
                                if let MySqlSelectProjectionOrigin::Column { table, column } =
                                    &projection_origins[0][index]
                                {
                                    if let Some((table, ordinal)) = source_metadata
                                        .projection_source(0, table.as_deref(), column)
                                    {
                                        let mut definition = source_metadata
                                            .column_definition_for_reference(
                                                Some((table.table_reference.clone(), ordinal)),
                                                name,
                                                column_types[index],
                                            )?;
                                        definition.flags &= !(MYSQL_PRI_KEY_FLAG
                                            | MYSQL_PART_KEY_FLAG
                                            | MYSQL_UNIQUE_KEY_FLAG
                                            | MYSQL_AUTO_INCREMENT_FLAG);
                                        return Ok(definition);
                                    }
                                }
                            }
                        } else {
                            return source_metadata.column_definition(
                                &statement,
                                index,
                                name,
                                column_types[index],
                            );
                        }
                    }
                    Ok(column_definition(
                        name,
                        column_types[index].unwrap_or(MYSQL_TYPE_NULL),
                    ))
                }
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    #[cfg(unix)]
    let mut columns = columns;
    #[cfg(unix)]
    if compound {
        apply_compound_shape(
            &mut columns,
            &projection_origins,
            source_metadata.as_ref(),
            drops_repeated_rows,
        )?;
    }
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
    branch: usize,
    subquery: bool,
    columns: Vec<MySqlColumnMetadata>,
    /// An `information_schema` table's columns, whose shapes are the ones
    /// MySQL reports for them rather than shapes read out of stored DDL. A
    /// table has these or the ones above, never both.
    catalog_columns: Vec<ColumnDefinitionConfig>,
    /// The columns of a view grouping its rows, whose shapes MySQL reads out
    /// of the table it gathers the groups into rather than out of the table
    /// the view reads. A source has these, the catalog's or the table's.
    view_columns: Vec<ColumnDefinitionConfig>,
    /// The columns a `WITH` name projects, in order, when this reference is a
    /// CTE rather than the table itself. A result column's ordinal counts
    /// through these, not through the table's own columns.
    projected_columns: Vec<String>,
    /// What a derived table or a CTE projects, when this reference is one.
    derived: Option<MySqlDerivedColumns>,
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
    /// The limit a `GROUP_CONCAT` is cut at, which sizes its column.
    group_concat_max_len: u64,
}

#[cfg(unix)]
impl SourceTableColumns {
    /// Turns a result column's ordinal into the table column it names.
    ///
    /// A CTE can project its table's columns in any order, so the ordinal
    /// counts through what the CTE projected and the name it lands on is
    /// looked up in the table.
    /// Returns what the derived table's column at `ordinal` answers, when the
    /// body worked it out rather than reading it from its table.
    fn answer(&self, ordinal: usize) -> Option<&turso_mysql_parser::StaticSelectMetadata> {
        self.derived.as_ref()?.answer(ordinal)
    }

    fn column_ordinal(&self, ordinal: usize) -> Result<usize, FrontendErrorKind> {
        if self.projected_columns.is_empty() {
            return Ok(ordinal);
        }
        if !self.view_columns.is_empty() {
            // A CTE over a grouping view would have to count through what the
            // CTE projected, which is not read here.
            return Err(FrontendErrorKind::Unsupported);
        }
        let name = self
            .projected_columns
            .get(ordinal)
            .ok_or(FrontendErrorKind::Internal)?;
        if !self.catalog_columns.is_empty() {
            return self
                .catalog_columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(name))
                .ok_or(FrontendErrorKind::UnknownColumn);
        }
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
            let position = match table.catalog_columns.is_empty() {
                true => table
                    .columns
                    .iter()
                    .position(|column| column.name().eq_ignore_ascii_case(name)),
                false => table
                    .catalog_columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name)),
            };
            let Some(ordinal) = position else {
                continue;
            };
            if found.is_some() {
                return Err(FrontendErrorKind::Unsupported);
            }
            // Every reader of a named column answers a call, an aggregate or
            // arithmetic over it, and none of those has been measured over a
            // BIT, whose value crosses as a byte rather than as a number.
            if table
                .columns
                .get(ordinal)
                .is_some_and(|column| column.type_name() == "BIT")
            {
                return Err(FrontendErrorKind::Unsupported);
            }
            found = Some((table, ordinal));
        }
        found.ok_or(FrontendErrorKind::UnknownColumn)
    }

    fn projection_source(
        &self,
        branch: usize,
        table_name: Option<&str>,
        column_name: &str,
    ) -> Option<(&SourceTableColumns, usize)> {
        let mut matches = self
            .tables
            .iter()
            .filter(|table| {
                table.branch == branch
                    && !table.subquery
                    && table_name
                        .is_none_or(|name| table.table_reference.eq_ignore_ascii_case(name))
            })
            .filter_map(|table| {
                let named = table
                    .derived
                    .as_ref()
                    .map(MySqlDerivedColumns::names)
                    .filter(|names| !names.is_empty())
                    .unwrap_or(&table.projected_columns);
                let ordinal = if !named.is_empty() {
                    named
                        .iter()
                        .position(|name| name.eq_ignore_ascii_case(column_name))
                } else if !table.catalog_columns.is_empty() {
                    table
                        .catalog_columns
                        .iter()
                        .position(|column| column.name.eq_ignore_ascii_case(column_name))
                } else {
                    table
                        .columns
                        .iter()
                        .position(|column| column.name().eq_ignore_ascii_case(column_name))
                }?;
                Some((table, ordinal))
            });
        let one = matches.next()?;
        matches.next().is_none().then_some(one)
    }

    /// Returns the table column a branch's result column reads, where it is a
    /// base table's own column.
    fn projection_column(
        &self,
        branch: usize,
        table_name: Option<&str>,
        column_name: &str,
    ) -> Option<&MySqlColumnMetadata> {
        let (source, ordinal) = self.projection_source(branch, table_name, column_name)?;
        let ordinal = source.column_ordinal(ordinal).ok()?;
        source.columns.get(ordinal)
    }

    fn projection_is_not_null(
        &self,
        branch: usize,
        origin: &MySqlSelectProjectionOrigin,
    ) -> Option<bool> {
        match origin {
            MySqlSelectProjectionOrigin::NonNullLiteral => Some(true),
            MySqlSelectProjectionOrigin::Null => Some(false),
            MySqlSelectProjectionOrigin::Other => None,
            MySqlSelectProjectionOrigin::Column { table, column } => {
                let (source, ordinal) = self.projection_source(branch, table.as_deref(), column)?;
                let ordinal = source.column_ordinal(ordinal).ok()?;
                if let Some(column) = source.columns.get(ordinal) {
                    Some(!column.nullable() && !source.outer)
                } else {
                    source
                        .catalog_columns
                        .get(ordinal)
                        .map(|column| column.flags & MYSQL_NOT_NULL_FLAG != 0 && !source.outer)
                }
            }
        }
    }
}

/// Works out each result column of a `UNION`, `EXCEPT` or `INTERSECT` from
/// every branch, where the engine reports only the first branch's column.
///
/// A column every branch takes straight from a table, or a `NULL` in some
/// branches, is given the shape MySQL gives the columns, and one whose pair
/// has not been measured is refused rather than answered with the first
/// branch's shape. A column beside a written value is refused: measured,
/// `SELECT i ... UNION SELECT 1` over an `INT` answers a `LONGLONG` of 11,
/// a rule of its own.
///
/// A query dropping repeated rows is refused over words compared without
/// regard to case: measured, `'aa'` then `'AA'` answers `aa` in MySQL, which
/// keeps the first it meets, and `AA` in the engine.
#[cfg(unix)]
fn apply_compound_shape(
    columns: &mut [ColumnDefinitionConfig],
    branches: &[Vec<MySqlSelectProjectionOrigin>],
    source_metadata: Option<&TableResultMetadata>,
    drops_repeated_rows: bool,
) -> Result<(), FrontendErrorKind> {
    if branches.len() < 2 || branches.iter().any(|branch| branch.len() != columns.len()) {
        return Ok(());
    }
    for (index, definition) in columns.iter_mut().enumerate() {
        let mut sources = Vec::with_capacity(branches.len());
        let mut every_branch_is_a_column_or_null = true;
        let mut writes_a_value = false;
        for (branch, origins) in branches.iter().enumerate() {
            match &origins[index] {
                MySqlSelectProjectionOrigin::Column { table, column } => {
                    match source_metadata.and_then(|metadata| {
                        metadata.projection_column(branch, table.as_deref(), column)
                    }) {
                        Some(source) => sources.push(source),
                        None => every_branch_is_a_column_or_null = false,
                    }
                }
                MySqlSelectProjectionOrigin::Null => {}
                MySqlSelectProjectionOrigin::NonNullLiteral => writes_a_value = true,
                MySqlSelectProjectionOrigin::Other => every_branch_is_a_column_or_null = false,
            }
        }
        if writes_a_value && !sources.is_empty() {
            return Err(FrontendErrorKind::Unsupported);
        }
        if drops_repeated_rows && sources.iter().any(|source| compares_words_by_fold(source)) {
            return Err(FrontendErrorKind::Unsupported);
        }
        if every_branch_is_a_column_or_null && !sources.is_empty() {
            apply_compound_column_shape(definition, &sources)?;
        }
    }
    apply_compound_nullability(columns, branches, source_metadata);
    Ok(())
}

/// Answers whether a column holds words two of which can be equal without
/// being spelled alike.
#[cfg(unix)]
fn compares_words_by_fold(source: &MySqlColumnMetadata) -> bool {
    is_text_column(source) && source.collation_name() != Some("utf8mb4_bin")
}

/// One source column as a compound query's result column sees it.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompoundMember {
    /// A signed whole number, ranked by how wide its type is.
    WholeNumber(u8),
    Varchar(u32),
    Char(u32),
    Text,
    Double,
    Decimal(u32, u32),
    DateTime,
    Date,
}

/// Gives a compound result column the shape MySQL gives the columns its
/// branches read.
///
/// Measured on MySQL 8.4.11. Two whole numbers answer the wider one's type at
/// its own width — `TINYINT` with `INT` a `LONG` of 11, `INT` with `BIGINT` a
/// `LONGLONG` of 20 — and a `TINYINT(1)` counts as a `TINYINT`, reporting 4
/// where the column alone reports 1. Two words answer a `VAR_STRING` as wide as
/// the wider, four bytes to the character, and two `CHAR`s stay a `CHAR` as
/// wide as the wider. A `TEXT` beside a `TEXT` or a `VARCHAR` answers a `BLOB` of 1048560,
/// where the column alone reports 262140. A `DOUBLE` reports 23 where the
/// column alone reports 22. A `DECIMAL`, a `DATETIME` and a `DATE` beside their
/// own kind at the same size keep the column's shape.
///
/// Everything else is refused: a word beside a number, which MySQL answers as
/// a word and the engine keeps as two kinds that compare differently; a
/// `DECIMAL` beside a different `DECIMAL`; words under two collations; and
/// every type whose pair has not been measured.
#[cfg(unix)]
fn apply_compound_column_shape(
    definition: &mut ColumnDefinitionConfig,
    sources: &[&MySqlColumnMetadata],
) -> Result<(), FrontendErrorKind> {
    let (first, rest) = sources.split_first().ok_or(FrontendErrorKind::Internal)?;
    if rest
        .iter()
        .any(|source| source.collation_name() != first.collation_name())
    {
        return Err(FrontendErrorKind::Unsupported);
    }
    let mut merged = compound_member(first).ok_or(FrontendErrorKind::Unsupported)?;
    for source in rest {
        let member = compound_member(source).ok_or(FrontendErrorKind::Unsupported)?;
        merged = merge_compound_members(merged, member).ok_or(FrontendErrorKind::Unsupported)?;
    }
    match merged {
        CompoundMember::WholeNumber(rank) => {
            let (column_type, length) = match rank {
                1 => (MYSQL_TYPE_TINY, 4),
                2 => (MYSQL_TYPE_SHORT, 6),
                3 => (MYSQL_TYPE_LONG, 11),
                _ => (MYSQL_TYPE_LONGLONG, 20),
            };
            definition.column_type = column_type;
            definition.column_length = length;
            definition.decimals = 0;
            set_column_flags(definition, 0);
        }
        CompoundMember::Varchar(characters) => {
            definition.column_type = MYSQL_TYPE_VAR_STRING;
            definition.column_length = characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
            definition.decimals = 0;
            set_column_flags(definition, 0);
        }
        CompoundMember::Text => {
            definition.column_type = MYSQL_TYPE_BLOB;
            definition.column_length = 1_048_560;
            definition.decimals = 0;
            set_column_flags(definition, MYSQL_BLOB_FLAG);
        }
        CompoundMember::Double => {
            definition.column_type = MYSQL_TYPE_DOUBLE;
            definition.column_length = 23;
            definition.decimals = NOT_FIXED_DECIMALS;
            set_column_flags(definition, 0);
        }
        CompoundMember::Char(characters) => {
            definition.column_type = MYSQL_TYPE_STRING;
            definition.column_length = characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
            definition.decimals = 0;
            set_column_flags(definition, 0);
        }
        CompoundMember::Decimal(..) | CompoundMember::DateTime | CompoundMember::Date => {}
    }
    Ok(())
}

#[cfg(unix)]
fn compound_member(source: &MySqlColumnMetadata) -> Option<CompoundMember> {
    Some(match source.type_name() {
        "TINYINT" | "BOOLEAN" => CompoundMember::WholeNumber(1),
        "SMALLINT" => CompoundMember::WholeNumber(2),
        "INT" | "INTEGER" => CompoundMember::WholeNumber(3),
        "BIGINT" => CompoundMember::WholeNumber(4),
        "VARCHAR" => CompoundMember::Varchar(source.character_length()?),
        "CHAR" => CompoundMember::Char(source.character_length()?),
        "TEXT" => CompoundMember::Text,
        "DOUBLE" => CompoundMember::Double,
        "DECIMAL" => {
            let (precision, scale) = source.decimal_size()?;
            CompoundMember::Decimal(precision, scale)
        }
        "DATETIME" if source.temporal_precision().unwrap_or(0) == 0 => CompoundMember::DateTime,
        "DATE" => CompoundMember::Date,
        _ => return None,
    })
}

#[cfg(unix)]
fn merge_compound_members(left: CompoundMember, right: CompoundMember) -> Option<CompoundMember> {
    use CompoundMember::{Char, Text, Varchar, WholeNumber};
    match (left, right) {
        (WholeNumber(left), WholeNumber(right)) => Some(WholeNumber(left.max(right))),
        (Char(left), Char(right)) => Some(Char(left.max(right))),
        (Varchar(left) | Char(left), Varchar(right) | Char(right)) => {
            Some(Varchar(left.max(right)))
        }
        (Text, Text | Varchar(_)) | (Varchar(_), Text) => Some(Text),
        (left, right) if left == right => Some(left),
        _ => None,
    }
}

#[cfg(unix)]
fn apply_compound_nullability(
    columns: &mut [ColumnDefinitionConfig],
    branches: &[Vec<MySqlSelectProjectionOrigin>],
    source_metadata: Option<&TableResultMetadata>,
) {
    for (index, definition) in columns.iter_mut().enumerate() {
        let all_not_null =
            branches
                .iter()
                .enumerate()
                .all(|(branch, origins)| match &origins[index] {
                    MySqlSelectProjectionOrigin::NonNullLiteral => true,
                    MySqlSelectProjectionOrigin::Null | MySqlSelectProjectionOrigin::Other => false,
                    origin @ MySqlSelectProjectionOrigin::Column { .. } => source_metadata
                        .and_then(|metadata| metadata.projection_is_not_null(branch, origin))
                        .unwrap_or(false),
                });
        if all_not_null {
            definition.flags |= MYSQL_NOT_NULL_FLAG;
        }
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
        if let Some(answer) = table.answer(ordinal) {
            return self.derived_answer_definition(table, ordinal, answer, name);
        }
        let mut definition = self.table_column_definition(
            table,
            table.column_ordinal(ordinal)?,
            name,
            fallback_type,
        )?;
        if !self.union && table.derived.is_none() && table.catalog_columns.is_empty() {
            // The engine reports the name the statement read the table under,
            // spelled the way it was declared. A derived table's name was
            // declared by the statement, and MySQL reports it as written.
            definition.table = table_reference;
        }
        if let Some(derived) = &table.derived {
            read_through_a_derived_table(&mut definition, derived, ordinal);
        }
        Ok(definition)
    }

    /// Builds the result column one of a table's own columns reports, by its
    /// place among the table's columns.
    fn table_column_definition(
        &self,
        table: &SourceTableColumns,
        ordinal: usize,
        name: String,
        fallback_type: Option<u8>,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        // An `information_schema` column reports the shape MySQL reports for
        // it, which is pinned rather than worked out from a declared type.
        // Only the column itself is answered: an aggregate or a call over one
        // has not been measured.
        if !table.catalog_columns.is_empty() {
            let mut definition = table
                .catalog_columns
                .get(ordinal)
                .ok_or(FrontendErrorKind::Unsupported)?
                .clone();
            definition.name = name;
            return Ok(definition);
        }
        if !table.view_columns.is_empty() {
            let mut definition = table
                .view_columns
                .get(ordinal)
                .ok_or(FrontendErrorKind::Internal)?
                .clone();
            definition.name = name;
            definition.table.clone_from(&table.table_reference);
            return Ok(definition);
        }
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
            // (20,4)=22. An unsigned one spends no character on the sign, so it
            // is one narrower throughout: (10,2)=11, (5,0)=5, (65,30)=66 and
            // (1,1)=2.
            let sign = u32::from(source.type_name() != "DECIMAL UNSIGNED");
            definition.column_length = precision + sign + u32::from(scale > 0);
            definition.decimals = scale as u8;
        }
        if let Some(precision) = source.temporal_precision() {
            let whole_seconds_length = if source.type_name() == "TIME" { 10 } else { 19 };
            definition.column_length = whole_seconds_length
                + if precision == 0 {
                    0
                } else {
                    1 + u32::from(precision)
                };
            definition.decimals = precision;
        }
        if source.type_name() == "YEAR" {
            // Measured on MySQL 8.4.11: 4, the four digits it prints.
            definition.column_length = 4;
        }
        if let Some(members) = turso_mysql_parser::enum_members(source.type_name()) {
            // Measured: the width of the longest member, counting the four
            // bytes utf8mb4 reserves for a character — `medium` reports 24.
            let widest = members
                .iter()
                .map(|member| member.chars().count() as u32)
                .max()
                .unwrap_or(0);
            definition.column_length = widest.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        }
        if let Some(members) = turso_mysql_parser::set_members(source.type_name()) {
            // Measured: the width of every member laid end to end with the
            // commas that would join them — `read`, `write` and `exec` report
            // 60 — counting the four bytes utf8mb4 reserves for a character.
            let characters: u32 = members
                .iter()
                .map(|member| member.chars().count() as u32)
                .sum::<u32>()
                .saturating_add(members.len().saturating_sub(1) as u32);
            definition.column_length = characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        }
        if source.type_name() == "JSON" {
            // Measured on MySQL 8.4.11: a JSON column reports the widest length
            // there is and the binary collation, the way a LONGBLOB does.
            definition.column_length = u32::MAX;
            definition.character_set = MYSQL_BINARY_COLLATION;
        }
        if source.type_name() == "DATE" {
            // Measured on MySQL 8.4.11: 10 for both — the width of
            // `YYYY-MM-DD`, and for a TIME the width of the widest span it
            // holds without its sign, `838:59:59`.
            definition.column_length = 10;
        }
        if matches!(source.type_name(), "FLOAT" | "FLOAT UNSIGNED") {
            // Measured on MySQL 8.4.11: a FLOAT column reports 12 where a
            // DOUBLE reports 22, both with the not-fixed decimals value, and
            // an unsigned one of either reports the same width as its signed
            // form — unlike an integer, which spends no character on a sign.
            definition.column_length = 12;
            definition.decimals = NOT_FIXED_DECIMALS;
        }
        if source.type_name() == "BOOLEAN" {
            // Measured on MySQL 8.4.11: a BOOLEAN column reports 1, the display
            // width in `tinyint(1)`, where a plain TINYINT reports 4.
            definition.column_length = 1;
        }
        if let Some(length) = unsigned_integer_column_length(source.type_name()) {
            definition.column_length = length;
        }
        // Measured on MySQL 8.4.11: a TEXT column reports 262140, the four
        // bytes utf8mb4 needs for each of 65,535 characters, and carries the
        // text collation; a BLOB reports 65535 and the binary one. The sized
        // variants follow their own byte limits the same way.
        let blob_or_text_info = match source.type_name() {
            "TINYTEXT" => Some((1_020, true)),
            "TEXT" => Some((262_140, true)),
            "MEDIUMTEXT" => Some((67_108_860, true)),
            "LONGTEXT" => Some((u32::MAX, true)),
            "TINYBLOB" => Some((255, false)),
            "BLOB" => Some((65_535, false)),
            "MEDIUMBLOB" => Some((16_777_215, false)),
            "LONGBLOB" => Some((u32::MAX, false)),
            _ => None,
        };
        if let Some((length, text)) = blob_or_text_info {
            definition.column_length = length;
            definition.character_set = if text {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            } else {
                MYSQL_BINARY_COLLATION
            };
        }
        if let Some(length) = source.character_length() {
            if source.type_name() == "VARBINARY" {
                // Measured on MySQL 8.4.11: a `VARBINARY(255)` reports 255. The
                // declared count is already bytes, so nothing is reserved on
                // top of it, and the column carries the binary collation and
                // flag rather than utf8mb4.
                definition.column_length = length;
                definition.character_set = MYSQL_BINARY_COLLATION;
            } else {
                // Measured on MySQL 8.4.11: a `VARCHAR(4)` and a `CHAR(4)` both
                // report 16. The declared count is characters, and the reported
                // length reserves the four bytes utf8mb4 needs for one.
                definition.column_length = length.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
            }
        }
        definition.schema.clone_from(&self.database);
        definition.table.clone_from(&table.table_reference);
        definition.original_table.clone_from(&table.source_table);
        source.name().clone_into(&mut definition.original_name);
        definition.flags = mysql_table_column_flags(source);
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
        if matches!(
            source.type_name(),
            "DATETIME" | "TIMESTAMP" | "DATE" | "TIME"
        ) {
            // Measured: a temporal column carries the binary flag, because it
            // has no collation of its own.
            definition.flags |= MYSQL_BINARY_FLAG;
        }
        if turso_mysql_parser::enum_members(source.type_name()).is_some() {
            // Measured: an ENUM column carries the flag that says so, and no
            // binary flag — it has a collation of its own.
            definition.flags |= MYSQL_ENUM_FLAG;
        }
        if turso_mysql_parser::set_members(source.type_name()).is_some() {
            definition.flags |= MYSQL_SET_FLAG;
        }
        if source.type_name() == "BIT" {
            // Measured on MySQL 8.4.11: a `bit(1)` reports a length of 1, the
            // binary collation and the unsigned flag, and no binary flag.
            definition.column_length = 1;
            definition.character_set = MYSQL_BINARY_COLLATION;
            definition.flags |= MYSQL_UNSIGNED_FLAG;
        }
        if source.type_name() == "YEAR" {
            // Measured: a YEAR carries the flags of a number rather than of a
            // moment — unsigned, zerofilled and numeric — and no binary flag at
            // all, which every other temporal column has.
            definition.flags |= MYSQL_UNSIGNED_FLAG | MYSQL_ZEROFILL_FLAG | MYSQL_NUM_FLAG;
        }
        Ok(definition)
    }

    /// Builds the result column a key of a statement grouping `WITH ROLLUP`
    /// reports.
    ///
    /// Measured on MySQL 8.4.11: the column's own type and length, naming no
    /// table and no column, nullable whatever the column is — a super total
    /// answers it as NULL — and with none of its keys. A whole number and a
    /// `DECIMAL` carry the binary flag and keep their unsigned one, a
    /// `TINYINT(1)` reporting a `TINYINT`'s 4; a `VARCHAR` and a `CHAR` carry
    /// no flags and 31 decimals. Any other column has not been measured there
    /// and is refused.
    fn rolled_up_key_definition(
        &self,
        name: String,
        column_name: &str,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        let (table, ordinal) = self.column_named(column_name)?;
        let source = table
            .columns
            .get(ordinal)
            .ok_or(FrontendErrorKind::Unsupported)?;
        let mut definition = self.table_column_definition(table, ordinal, name, None)?;
        definition.schema.clear();
        definition.table.clear();
        definition.original_table.clear();
        definition.original_name.clear();
        let flags = if is_whole_number_column(source.type_name())
            || matches!(source.type_name(), "DECIMAL" | "DECIMAL UNSIGNED")
        {
            if source.type_name() == "BOOLEAN" {
                definition.column_length = 4;
            }
            MYSQL_BINARY_FLAG | (definition.flags & MYSQL_UNSIGNED_FLAG)
        } else if matches!(source.type_name(), "VARCHAR" | "CHAR")
            && definition.character_set != MYSQL_BINARY_COLLATION
        {
            definition.decimals = NOT_FIXED_DECIMALS;
            0
        } else {
            return Err(FrontendErrorKind::Unsupported);
        };
        set_column_flags(&mut definition, flags);
        Ok(definition)
    }

    /// Builds the result column a derived table's worked-out column reports.
    ///
    /// Measured on MySQL 8.4.11: MySQL writes a body that aggregates out into
    /// a table of its own, and the column is that table's. It names the
    /// derived table, goes by the name the body gave it, and names no
    /// database and no original table. Its shape is the answer's own, stored
    /// — see [`stored_in_a_derived_table`].
    fn derived_answer_definition(
        &self,
        table: &SourceTableColumns,
        ordinal: usize,
        answer: &turso_mysql_parser::StaticSelectMetadata,
        name: String,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        let derived = table.derived.as_ref().ok_or(FrontendErrorKind::Internal)?;
        let mut definition = match static_column_definition(name.clone(), answer) {
            Some(definition) => definition,
            None => aggregate_column_definition(Some(self), name, answer)?,
        };
        stored_in_a_derived_table(&mut definition, answer)?;
        definition.schema.clear();
        definition.table.clone_from(&table.table_reference);
        definition.original_table.clear();
        definition.original_name.clone_from(
            derived
                .names()
                .get(ordinal)
                .ok_or(FrontendErrorKind::Internal)?,
        );
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
        let source = table
            .columns
            .get(ordinal)
            // An `information_schema` table names its columns itself, and an
            // aggregate or a call over one of them has not been measured.
            .ok_or(FrontendErrorKind::Unsupported)?;
        let mut definition = self.table_column_definition(table, ordinal, name, None)?;
        // Measured on MySQL 8.4.11: `STDDEV_SAMP(v)` answers a DOUBLE of length
        // 23 with the not-fixed decimals value, whatever the column is, and it
        // is nullable — a single row has no sample deviation, which both
        // engines answer NULL for.
        if kind == ColumnAggregateKind::DeviatesBySample {
            definition.column_type = MYSQL_TYPE_DOUBLE;
            definition.column_length = 23;
            definition.decimals = NOT_FIXED_DECIMALS;
        } else if kind == ColumnAggregateKind::CollectsIntoJson {
            // Measured on MySQL 8.4.11: the JSON type at the widest a document
            // can be, with the text collation and the binary flag, as
            // `JSON_ARRAY` answers. Only words and whole numbers are taken: a
            // DOUBLE, a DECIMAL, a moment or a JSON document is written into
            // the array by a rule of its own.
            if !(is_text_column(source) || is_signed_whole_number_column(source.type_name())) {
                return Err(FrontendErrorKind::Unsupported);
            }
            let mut definition = column_definition(definition.name, MYSQL_TYPE_JSON);
            definition.column_length = u32::MAX - 3;
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            definition.decimals = NOT_FIXED_DECIMALS;
            set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
            return Ok(definition);
        } else if kind == ColumnAggregateKind::Concatenated {
            if !joins_as_the_engine_writes_it(source) {
                return Err(FrontendErrorKind::Unsupported);
            }
            // Measured on MySQL 8.4.11, in both protocols: up to 512 bytes a
            // `VAR_STRING` four bytes to each, past that a `LONG_BLOB` 64
            // bytes to each, up to the widest a column can say it is.
            let max_len = self.group_concat_max_len;
            if max_len <= 512 {
                definition.column_type = MYSQL_TYPE_VAR_STRING;
                definition.column_length =
                    u32::try_from(max_len * 4).expect("512 bytes, four to each, fit a u32");
            } else {
                definition.column_type = MYSQL_TYPE_LONG_BLOB;
                definition.column_length =
                    u32::try_from(max_len.saturating_mul(64)).unwrap_or(u32::MAX);
            }
            definition.decimals = 31;
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        } else if kind != ColumnAggregateKind::MinMax {
            apply_summing_aggregate_metadata(&mut definition, source, kind)?;
        } else if source.type_name() == "TEXT" {
            // MySQL gives MIN/MAX over TEXT the aggregate's own width, rather
            // than the width of the TEXT column it reads.
            definition.column_length = 1_048_560;
        }
        definition.schema.clear();
        definition.table.clear();
        definition.original_table.clear();
        definition.original_name.clear();
        let aggregate_flags = if matches!(
            definition.column_type,
            MYSQL_TYPE_VAR_STRING
                | MYSQL_TYPE_STRING
                | MYSQL_TYPE_DATETIME
                | MYSQL_TYPE_TIMESTAMP
                | MYSQL_TYPE_BLOB
                | MYSQL_TYPE_LONG_BLOB
        ) {
            // Measured: a MIN over a text or temporal column reports no flags
            // at all, losing even the BINARY a temporal column carries.
            // GROUP_CONCAT answers a BLOB with no flags at all.
            0
        } else if kind == ColumnAggregateKind::MinMax {
            // Measured: a numeric aggregate answers with the binary collation
            // where the plain column does not, and a largest or smallest of an
            // unsigned column is unsigned too — `MAX(id)` over a BIGINT
            // UNSIGNED reports both.
            MYSQL_BINARY_FLAG | (definition.flags & MYSQL_UNSIGNED_FLAG)
        } else {
            MYSQL_BINARY_FLAG
        };
        set_column_flags(&mut definition, aggregate_flags);
        Ok(definition)
    }

    /// Builds the result column `ROUND(SUM(col), n)` or `ROUND(AVG(col), n)`
    /// reports.
    ///
    /// Measured on MySQL 8.4.11, over the precision and scale the aggregate
    /// answers on its own — a `SUM` 22 more digits than the column and its
    /// scale, an `AVG` 4 more digits and 4 more places. Rounding to more places
    /// than that scale, or to as many when there are any, keeps the
    /// aggregate's shape: `ROUND(AVG(views), 6)` and `ROUND(AVG(views), 4)`
    /// over an `INT` both answer 16 characters with 4 places. Rounding to
    /// fewer keeps the whole part, adds one digit for the carry rounding can
    /// make and keeps the places named: `ROUND(AVG(views), 2)` answers 15 with
    /// 2, and `ROUND(SUM(views))` 34 with none where `SUM(views)` answers 33.
    /// The length counts a sign and, when there are places, the point.
    ///
    /// The answer is nullable, belongs to no table, and carries the binary
    /// flag a numeric aggregate carries. A column that is not a whole number
    /// or a `DECIMAL` has not been measured and is refused.
    fn rounded_aggregate_column_definition(
        &self,
        name: String,
        column_name: &str,
        kind: ColumnAggregateKind,
        places: u32,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        let (table, ordinal) = self.column_named(column_name)?;
        let source = table
            .columns
            .get(ordinal)
            .ok_or(FrontendErrorKind::Unsupported)?;
        let (column_precision, column_scale) =
            summed_shape_of(source).ok_or(FrontendErrorKind::Unsupported)?;
        let (precision, scale) = match kind {
            ColumnAggregateKind::Sum => (column_precision + 22, column_scale),
            ColumnAggregateKind::Avg => (
                column_precision + 4,
                (column_scale + 4).min(MYSQL_MAX_DECIMAL_SCALE),
            ),
            _ => return Err(FrontendErrorKind::Internal),
        };
        let (precision, scale) = if places > 0 && places >= scale {
            (precision, scale)
        } else {
            (precision - scale + 1 + places, places)
        };
        let mut definition = column_definition(name, MYSQL_TYPE_NEWDECIMAL);
        definition.column_length = precision + 1 + u32::from(scale > 0);
        definition.decimals = scale as u8;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        Ok(definition)
    }

    /// Builds the result column an arithmetic expression reports.
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
        if let Some(definition) = Self::catalog_counter_sum(source_metadata, &name, shape)? {
            return Ok(definition);
        }
        let left = Self::arithmetic_operand_shape(source_metadata, &shape.left)?;
        let right = Self::arithmetic_operand_shape(source_metadata, &shape.right)?;
        let ArithmeticOperandShape {
            precision,
            scale,
            decimal,
            float,
            not_null,
        } = arithmetic_result_shape(shape.operator, &left, &right);
        if float {
            let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
            definition.column_length = 23;
            definition.decimals = NOT_FIXED_DECIMALS;
            set_column_flags(
                &mut definition,
                MYSQL_BINARY_FLAG | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
            );
            return Ok(definition);
        }
        let max_precision = MYSQL_MAX_DECIMAL_PRECISION
            + u32::from(
                decimal
                    && matches!(
                        shape.operator,
                        ArithmeticOperator::Add | ArithmeticOperator::Subtract
                    ),
            );
        if precision > max_precision {
            return Err(FrontendErrorKind::Unsupported);
        }
        let column_type = if decimal {
            MYSQL_TYPE_NEWDECIMAL
        } else {
            MYSQL_TYPE_LONGLONG
        };
        let mut definition = column_definition(name, column_type);
        definition.column_length = precision + 1 + u32::from(scale > 0);
        definition.decimals = scale as u8;
        set_column_flags(
            &mut definition,
            MYSQL_BINARY_FLAG | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
        );
        Ok(definition)
    }

    /// Finishes the sum of two unsigned `information_schema` counters, which
    /// is how Laravel lists tables: `(data_length + index_length) as size`.
    ///
    /// Measured on MySQL 8.4.11: a `LONGLONG` one digit wider than the wider
    /// counter, unsigned and numeric but without the binary flag the counters
    /// themselves also lack, and nullable. Every other arithmetic over one of
    /// these tables' columns has not been measured and is refused.
    fn catalog_counter_sum(
        source_metadata: Option<&Self>,
        name: &str,
        shape: &ArithmeticShape,
    ) -> Result<Option<ColumnDefinitionConfig>, FrontendErrorKind> {
        let counter = |operand: &ArithmeticOperand| -> Result<Option<u32>, FrontendErrorKind> {
            let ArithmeticOperand::Column { column_name } = operand else {
                return Ok(None);
            };
            let Some(source_metadata) = source_metadata else {
                return Ok(None);
            };
            let (table, ordinal) = source_metadata.column_named(column_name)?;
            let Some(column) = table.catalog_columns.get(ordinal) else {
                return Ok(None);
            };
            Ok((column.column_type == MYSQL_TYPE_LONGLONG
                && column.flags & MYSQL_UNSIGNED_FLAG != 0)
                .then_some(column.column_length))
        };
        if shape.operator != ArithmeticOperator::Add {
            return Ok(None);
        }
        let (Some(left), Some(right)) = (counter(&shape.left)?, counter(&shape.right)?) else {
            return Ok(None);
        };
        let mut definition = column_definition(name.to_owned(), MYSQL_TYPE_LONGLONG);
        definition.column_length = left.max(right) + 1;
        definition.decimals = 0;
        set_column_flags(&mut definition, MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG);
        Ok(Some(definition))
    }

    /// Finishes a `CASE` or `IF` naming a column or answering whole numbers,
    /// and an `IFNULL` or `COALESCE` falling one column back onto another.
    ///
    /// Measured on MySQL 8.4.11, the answer is the kind every branch shares
    /// and as wide as the widest of them. A `CASE` is NOT NULL only when every
    /// branch is and a row cannot fall past them all; an `IFNULL` or a
    /// `COALESCE` is NOT NULL when any one of its columns is.
    fn branches_column_definition(
        source_metadata: Option<&Self>,
        name: String,
        branches: &[Branch],
        may_be_null: bool,
        falls_back: bool,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        let answer = Self::branches_answer(source_metadata, branches)?;
        let not_null = if falls_back {
            answer.any_not_null
        } else {
            !may_be_null && answer.all_not_null
        };
        Ok(answer.kind.column_definition(name, not_null))
    }

    /// Finishes `SUM`, `AVG`, `MIN` or `MAX` over a `CASE` or `IF`.
    ///
    /// Measured on MySQL 8.4.11: each answers the shape it gives a column of
    /// the kind the `CASE` answers. `SUM` widens the `CASE`'s precision by 22
    /// and keeps its scale, `AVG` widens both by 4, and both answer a
    /// NEWDECIMAL over whole numbers — `SUM(CASE WHEN ... THEN 1 ELSE 0 END)`
    /// reports 24 and `AVG` of it 7 with 4 places. Over a `DOUBLE` all of
    /// them answer a `DOUBLE`. `MIN` and `MAX` answer the `CASE`'s own shape.
    /// Every one of them is nullable, since there may be no row at all.
    fn aggregate_over_branches_definition(
        source_metadata: Option<&Self>,
        name: String,
        kind: ColumnAggregateKind,
        branches: &turso_mysql_parser::StaticSelectMetadata,
    ) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
        // A `CASE` of written words alone is summed and compared by a
        // coercion or a collation, neither of which has been measured.
        let turso_mysql_parser::StaticSelectMetadata::Branches { branches, .. } = branches else {
            return Err(FrontendErrorKind::Unsupported);
        };
        let answer = Self::branches_answer(source_metadata, branches)?;
        let kind = match (kind, answer.kind) {
            (ColumnAggregateKind::Sum, BranchesKind::WholeNumber { precision, .. }) => {
                BranchesKind::Decimal {
                    precision: (precision + 22).min(MYSQL_MAX_DECIMAL_PRECISION),
                    scale: 0,
                }
            }
            (ColumnAggregateKind::Sum, BranchesKind::Decimal { precision, scale }) => {
                BranchesKind::Decimal {
                    precision: (precision + 22).min(MYSQL_MAX_DECIMAL_PRECISION),
                    scale,
                }
            }
            (ColumnAggregateKind::Avg, BranchesKind::WholeNumber { precision, .. }) => {
                BranchesKind::Decimal {
                    precision: (precision + 4).min(MYSQL_MAX_DECIMAL_PRECISION),
                    scale: 4,
                }
            }
            (ColumnAggregateKind::Avg, BranchesKind::Decimal { precision, scale }) => {
                BranchesKind::Decimal {
                    precision: (precision + 4).min(MYSQL_MAX_DECIMAL_PRECISION),
                    scale: (scale + 4).min(MYSQL_MAX_DECIMAL_SCALE),
                }
            }
            (
                ColumnAggregateKind::Sum | ColumnAggregateKind::Avg | ColumnAggregateKind::MinMax,
                BranchesKind::Double,
            ) => BranchesKind::Double,
            (ColumnAggregateKind::MinMax, whole @ BranchesKind::WholeNumber { .. }) => whole,
            // MySQL compares words under a collation and a `DECIMAL` as a
            // number, and the engine would compare either as the words they
            // are written as.
            _ => return Err(FrontendErrorKind::Unsupported),
        };
        Ok(kind.column_definition(name, false))
    }

    fn branches_answer(
        source_metadata: Option<&Self>,
        branches: &[Branch],
    ) -> Result<BranchesAnswer, FrontendErrorKind> {
        let mut shapes = Vec::with_capacity(branches.len());
        let mut text_collation = None;
        for branch in branches {
            let shape = match branch {
                Branch::WholeNumber { digit_count } => (
                    BranchesKind::WholeNumber {
                        column_type: MYSQL_TYPE_LONGLONG,
                        length: digit_count + 1,
                        precision: *digit_count,
                    },
                    true,
                ),
                Branch::Word { characters } => (
                    BranchesKind::Text {
                        characters: *characters,
                    },
                    true,
                ),
                Branch::Column { column_name } => {
                    let source_metadata = source_metadata.ok_or(FrontendErrorKind::Unsupported)?;
                    let (table, ordinal) = source_metadata.column_named(column_name)?;
                    let source = table
                        .columns
                        .get(ordinal)
                        // An `information_schema` table names its columns
                        // itself, and a CASE over one has not been measured.
                        .ok_or(FrontendErrorKind::Unsupported)?;
                    let kind = branch_column_kind(source)?;
                    if matches!(kind, BranchesKind::Text { .. }) {
                        // Two columns of words under different collations are
                        // 1267 in MySQL.
                        let collation = source.collation_name();
                        if text_collation.get_or_insert(collation) != &collation {
                            return Err(FrontendErrorKind::Unsupported);
                        }
                    }
                    (kind, !source.nullable() && !table.outer)
                }
            };
            shapes.push(shape);
        }
        let kind = shapes
            .iter()
            .map(|(kind, _)| *kind)
            .try_fold(None, |shared: Option<BranchesKind>, kind| {
                Ok(Some(match shared {
                    None => kind,
                    Some(shared) => shared.widened_by(kind)?,
                }))
            })?
            .ok_or(FrontendErrorKind::Unsupported)?;
        if let BranchesKind::Decimal { precision, .. } = kind {
            if precision > MYSQL_MAX_DECIMAL_PRECISION {
                return Err(FrontendErrorKind::Unsupported);
            }
        }
        Ok(BranchesAnswer {
            kind,
            all_not_null: shapes.iter().all(|(_, not_null)| *not_null),
            any_not_null: shapes.iter().any(|(_, not_null)| *not_null),
        })
    }

    fn arithmetic_operand_shape(
        source_metadata: Option<&Self>,
        operand: &ArithmeticOperand,
    ) -> Result<ArithmeticOperandShape, FrontendErrorKind> {
        match operand {
            ArithmeticOperand::Literal { digit_count } => Ok(ArithmeticOperandShape {
                precision: *digit_count,
                scale: 0,
                decimal: false,
                float: false,
                not_null: true,
            }),
            ArithmeticOperand::DecimalLiteral { precision, scale } => Ok(ArithmeticOperandShape {
                precision: *precision,
                scale: *scale,
                decimal: true,
                float: false,
                not_null: true,
            }),
            ArithmeticOperand::Column { column_name } => {
                let source_metadata = source_metadata.ok_or(FrontendErrorKind::Unsupported)?;
                let (table, ordinal) = source_metadata.column_named(column_name)?;
                let source = table
                    .columns
                    .get(ordinal)
                    // An `information_schema` table names its columns itself, and an
                    // aggregate or a call over one of them has not been measured.
                    .ok_or(FrontendErrorKind::Unsupported)?;
                let not_null = !source.nullable() && !table.outer;
                // A float carries no precision and scale of its own, and it
                // does not need any: what it touches answers a float.
                if source.type_name() == "DOUBLE" {
                    return Ok(ArithmeticOperandShape {
                        precision: 0,
                        scale: 0,
                        decimal: false,
                        float: true,
                        not_null,
                    });
                }
                let (precision, scale) =
                    decimal_shape_of(source).ok_or(FrontendErrorKind::Unsupported)?;
                Ok(ArithmeticOperandShape {
                    precision,
                    scale,
                    decimal: source.decimal_size().is_some(),
                    float: false,
                    not_null,
                })
            }
            // Measured on MySQL 8.4.11: `COUNT(*)` reports a LONGLONG of
            // length 21 whatever it counts, and it is never null.
            ArithmeticOperand::Count => Ok(ArithmeticOperandShape {
                precision: 20,
                scale: 0,
                decimal: false,
                float: false,
                not_null: true,
            }),
            // An aggregate carries the shape it answers on its own, which is
            // what makes `SUM(amount) * 2` the same width as `SUM(amount)`
            // multiplied by a single digit. It is nullable whatever its column
            // is: an empty table answers NULL.
            ArithmeticOperand::Aggregate { column_name, kind } => {
                let source_metadata = source_metadata.ok_or(FrontendErrorKind::Unsupported)?;
                let (table, ordinal) = source_metadata.column_named(column_name)?;
                let source = table
                    .columns
                    .get(ordinal)
                    .ok_or(FrontendErrorKind::Unsupported)?;
                // Measured: a SUM or an AVG over a float answers a float, and
                // so does a MIN or a MAX.
                if source.type_name() == "DOUBLE" {
                    return Ok(ArithmeticOperandShape {
                        precision: 0,
                        scale: 0,
                        decimal: false,
                        float: true,
                        not_null: false,
                    });
                }
                let (precision, scale) =
                    decimal_shape_of(source).ok_or(FrontendErrorKind::Unsupported)?;
                // A SUM and an AVG answer a decimal whatever they were given;
                // a MIN and a MAX answer the column's own kind.
                let (precision, scale, decimal) = match kind {
                    ColumnAggregateKind::Sum => (
                        (precision + 22).min(MYSQL_MAX_DECIMAL_PRECISION),
                        scale,
                        true,
                    ),
                    ColumnAggregateKind::Avg => (
                        (precision + 4).min(MYSQL_MAX_DECIMAL_PRECISION),
                        (scale + 4).min(MYSQL_MAX_DECIMAL_SCALE),
                        true,
                    ),
                    ColumnAggregateKind::MinMax => {
                        (precision, scale, source.decimal_size().is_some())
                    }
                    ColumnAggregateKind::Concatenated
                    | ColumnAggregateKind::DeviatesBySample
                    | ColumnAggregateKind::CollectsIntoJson => {
                        return Err(FrontendErrorKind::Unsupported)
                    }
                };
                Ok(ArithmeticOperandShape {
                    precision,
                    scale,
                    decimal,
                    float: false,
                    not_null: false,
                })
            }
            ArithmeticOperand::Nested(shape) => {
                let left = Self::arithmetic_operand_shape(source_metadata, &shape.left)?;
                let right = Self::arithmetic_operand_shape(source_metadata, &shape.right)?;
                // The parser refuses a nested division, so this never sees one.
                if shape.operator == ArithmeticOperator::Divide {
                    return Err(FrontendErrorKind::Internal);
                }
                Ok(arithmetic_result_shape(shape.operator, &left, &right))
            }
        }
    }
}

/// What a `CASE`, `IF`, `IFNULL` or `COALESCE` answers, and whether its
/// branches can be null.
#[cfg(unix)]
struct BranchesAnswer {
    kind: BranchesKind,
    all_not_null: bool,
    any_not_null: bool,
}

/// The kind of value a `CASE`, `IF`, `IFNULL` or `COALESCE` answers.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchesKind {
    /// A whole number, reported as the widest integer type among the
    /// branches, as long as the longest of them. `precision` counts digits.
    WholeNumber {
        column_type: u8,
        length: u32,
        precision: u32,
    },
    Decimal {
        precision: u32,
        scale: u32,
    },
    Double,
    /// Words, as many characters as the longest branch can hold.
    Text {
        characters: u32,
    },
}

#[cfg(unix)]
impl BranchesKind {
    /// The kind two branches share, measured on MySQL 8.4.11.
    ///
    /// Whole numbers answer the wider integer type — a written number counts
    /// as a `BIGINT` — as long as the longer branch: `CASE ... THEN age ELSE
    /// small END` over an `INT` and a `SMALLINT` is a LONG of 11, and
    /// `THEN 5 ELSE small` a LONGLONG of 6. A `DECIMAL` beside a whole number
    /// or another `DECIMAL` keeps the most digits before the point and the
    /// most after it: a `DECIMAL(10,2)` beside an `INT` is a NEWDECIMAL of 14
    /// with 2 places. A `DOUBLE` beside any number answers a `DOUBLE`. Words
    /// only go with words; a word beside a number is a coercion that has not
    /// been measured.
    fn widened_by(self, other: Self) -> Result<Self, FrontendErrorKind> {
        Ok(match (self, other) {
            (Self::Text { characters }, Self::Text { characters: other }) => Self::Text {
                characters: characters.max(other),
            },
            (Self::Text { .. }, _) | (_, Self::Text { .. }) => {
                return Err(FrontendErrorKind::Unsupported)
            }
            (Self::Double, _) | (_, Self::Double) => Self::Double,
            (
                Self::WholeNumber {
                    column_type,
                    length,
                    precision,
                },
                Self::WholeNumber {
                    column_type: other_type,
                    length: other_length,
                    precision: other_precision,
                },
            ) => Self::WholeNumber {
                column_type: if integer_type_rank(other_type) > integer_type_rank(column_type) {
                    other_type
                } else {
                    column_type
                },
                length: length.max(other_length),
                precision: precision.max(other_precision),
            },
            (left, right) => {
                let (left_digits, left_scale) = left.digits_before_and_after_the_point();
                let (right_digits, right_scale) = right.digits_before_and_after_the_point();
                let scale = left_scale.max(right_scale);
                Self::Decimal {
                    precision: left_digits.max(right_digits) + scale,
                    scale,
                }
            }
        })
    }

    fn digits_before_and_after_the_point(self) -> (u32, u32) {
        match self {
            Self::WholeNumber { precision, .. } => (precision, 0),
            Self::Decimal { precision, scale } => (precision - scale, scale),
            Self::Double | Self::Text { .. } => {
                unreachable!("only whole numbers and decimals are counted in digits")
            }
        }
    }

    fn column_definition(self, name: String, not_null: bool) -> ColumnDefinitionConfig {
        let not_null_flag = if not_null { MYSQL_NOT_NULL_FLAG } else { 0 };
        let mut definition = match self {
            Self::Text { characters } => {
                return text_call_definition(
                    name,
                    characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
                    not_null,
                );
            }
            Self::WholeNumber {
                column_type,
                length,
                ..
            } => {
                let mut definition = column_definition(name, column_type);
                definition.column_length = length;
                definition.decimals = 0;
                definition
            }
            Self::Decimal { precision, scale } => {
                let mut definition = column_definition(name, MYSQL_TYPE_NEWDECIMAL);
                definition.column_length = precision + 1 + u32::from(scale > 0);
                definition.decimals = scale as u8;
                definition
            }
            Self::Double => {
                let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
                definition.column_length = 23;
                definition.decimals = NOT_FIXED_DECIMALS;
                definition
            }
        };
        set_column_flags(
            &mut definition,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | not_null_flag,
        );
        definition
    }
}

/// Orders the integer types from narrowest to widest.
#[cfg(unix)]
fn integer_type_rank(column_type: u8) -> u8 {
    match column_type {
        MYSQL_TYPE_TINY => 0,
        MYSQL_TYPE_SHORT => 1,
        MYSQL_TYPE_INT24 => 2,
        MYSQL_TYPE_LONG => 3,
        MYSQL_TYPE_LONGLONG => 4,
        _ => unreachable!("only integer types are ranked"),
    }
}

/// The kind of value one column contributes as a branch.
///
/// An unsigned column is refused: measured on MySQL 8.4.11, a `BIGINT
/// UNSIGNED` beside a written number answers a NEWDECIMAL, and a `TINYINT
/// UNSIGNED` beside a `TINYINT` a SHORT, each a rule of its own. So is a
/// `FLOAT`, a `TEXT` and anything holding a moment.
#[cfg(unix)]
fn branch_column_kind(source: &MySqlColumnMetadata) -> Result<BranchesKind, FrontendErrorKind> {
    let whole = |column_type, length, precision| BranchesKind::WholeNumber {
        column_type,
        length,
        precision,
    };
    Ok(match source.type_name() {
        // Measured: a `TINYINT(1)` answers a TINY of 4 here, as any `TINYINT`
        // does, though on its own it reports 1.
        "TINYINT" | "BOOLEAN" => whole(MYSQL_TYPE_TINY, 4, 3),
        "SMALLINT" => whole(MYSQL_TYPE_SHORT, 6, 5),
        "MEDIUMINT" => whole(MYSQL_TYPE_INT24, 9, 8),
        "INT" | "INTEGER" => whole(MYSQL_TYPE_LONG, 11, 10),
        "BIGINT" => whole(MYSQL_TYPE_LONGLONG, 20, 19),
        "DECIMAL" => {
            let (precision, scale) = source.decimal_size().ok_or(FrontendErrorKind::Internal)?;
            BranchesKind::Decimal { precision, scale }
        }
        "DOUBLE" => BranchesKind::Double,
        "VARCHAR" | "CHAR" => BranchesKind::Text {
            characters: source
                .character_length()
                .ok_or(FrontendErrorKind::Internal)?,
        },
        _ => return Err(FrontendErrorKind::Unsupported),
    })
}

/// The precision and nullability one arithmetic operand contributes.
#[cfg(unix)]
struct ArithmeticOperandShape {
    precision: u32,
    scale: u32,
    /// Whether this side is a decimal rather than a whole number.
    ///
    /// It is not the same as carrying a scale: measured on MySQL 8.4.11,
    /// `SUM(n)` over an `INT` answers a NEWDECIMAL with no decimal places at
    /// all, and `SUM(n) + 1` answers a NEWDECIMAL too — where `COUNT(*) + 1`
    /// and `MAX(n) + 1` each answer a LONGLONG.
    decimal: bool,
    /// Whether this side is a float, which carries no precision and scale of
    /// its own.
    ///
    /// Measured on MySQL 8.4.11: arithmetic touching a `DOUBLE` answers a
    /// DOUBLE of length 23 with 31 decimals whatever the other side is and
    /// whichever operator it was — so a float swallows the precision rules
    /// rather than taking part in them.
    float: bool,
    not_null: bool,
}

/// Works out the shape one arithmetic operator answers over two operands.
///
/// Measured on MySQL 8.4.11 over an `INT`, a `DECIMAL(10,2)` and the
/// aggregates over each. Adding and subtracting keep the widest whole part and
/// the widest scale and add a digit — `amount + 1` over a `DECIMAL(10,2)`
/// answers 11 digits with 2 places, and `SUM(n) + SUM(amount)` 35 with 2.
/// Multiplying adds both precisions and both scales: `SUM(amount) * 2` answers
/// 33 with 2. Dividing widens the left side by four digits and four places,
/// plus the divisor's scale in the precision: `AVG(n) / 2` answers 18 with 8.
/// Multiplication and division stop at 65 digits and 30 decimal places for wide decimals;
/// addition can report a 66th digit for a carry.
#[cfg(unix)]
fn arithmetic_result_shape(
    operator: ArithmeticOperator,
    left: &ArithmeticOperandShape,
    right: &ArithmeticOperandShape,
) -> ArithmeticOperandShape {
    match operator {
        _ if left.float || right.float => ArithmeticOperandShape {
            precision: 0,
            scale: 0,
            decimal: false,
            float: true,
            not_null: match operator {
                ArithmeticOperator::Divide => false,
                _ => left.not_null && right.not_null,
            },
        },
        ArithmeticOperator::Add | ArithmeticOperator::Subtract => {
            let scale = left.scale.max(right.scale);
            ArithmeticOperandShape {
                precision: (left.precision - left.scale).max(right.precision - right.scale)
                    + scale
                    + 1,
                scale,
                decimal: left.decimal || right.decimal,
                float: false,
                not_null: left.not_null && right.not_null,
            }
        }
        ArithmeticOperator::Multiply => {
            let decimal = left.decimal || right.decimal;
            let precision = left.precision + right.precision;
            let scale = left.scale + right.scale;
            ArithmeticOperandShape {
                precision: if decimal {
                    precision.min(MYSQL_MAX_DECIMAL_PRECISION)
                } else {
                    precision
                },
                scale: scale.min(MYSQL_MAX_DECIMAL_SCALE),
                decimal,
                float: false,
                not_null: left.not_null && right.not_null,
            }
        }
        // A division always answers a decimal, whichever whole numbers it was
        // given: MySQL's `/` is decimal division.
        ArithmeticOperator::Divide => ArithmeticOperandShape {
            precision: (left.precision + 4 + right.scale).min(MYSQL_MAX_DECIMAL_PRECISION),
            scale: (left.scale + 4).min(MYSQL_MAX_DECIMAL_SCALE),
            decimal: true,
            float: false,
            not_null: false,
        },
    }
}

/// Applies MySQL's `SUM` and `AVG` result rules to an already-typed column.
///
/// Measured on MySQL 8.4.11. A `SUM` widens the argument's decimal precision by
/// 22 and keeps its scale: `SUM` over `TINYINT` (precision 3) reports length 26,
/// `SMALLINT` 28, `MEDIUMINT` 31, `INT` 33, `BIGINT` 42, and `DECIMAL(10,2)` 34
/// with 2 decimals. An `AVG` widens precision by 4 and scale by 4: over
/// `TINYINT` it reports length 9, over `INT` 16, and over `DECIMAL(10,2)` 16
/// with 6 decimals. The reported precision still widens past 65 for wide
/// columns, while the scale stops at 30. Over a `DOUBLE` both answer `DOUBLE`
/// with length 23 and 31 decimals, which is what a float column carries anyway.
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
    let (precision, scale) = summed_shape_of(source).ok_or(FrontendErrorKind::Unsupported)?;
    let (precision, scale) = match kind {
        ColumnAggregateKind::Sum => (precision + 22, scale),
        ColumnAggregateKind::Avg => (precision + 4, (scale + 4).min(MYSQL_MAX_DECIMAL_SCALE)),
        ColumnAggregateKind::MinMax
        | ColumnAggregateKind::Concatenated
        | ColumnAggregateKind::DeviatesBySample
        | ColumnAggregateKind::CollectsIntoJson => {
            unreachable!("only SUM and AVG use summing metadata")
        }
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
/// How many characters one column's value can spell, which is what `CONCAT`
/// lays end to end.
///
/// A word spells as many characters as it was declared to hold. A number
/// spells as many as its type does rather than as many as its column reports:
/// measured on MySQL 8.4.11, `CONCAT` over a `BOOLEAN` reserves four
/// characters where the column itself reports one, being a `TINYINT` under the
/// display width MySQL keeps for it.
///
/// Every count here is measured: `CONCAT(name, n)` over a `VARCHAR(40)` and an
/// `INT` reports 204 — forty characters and eleven, four bytes reserved for
/// each — and one column of every other kind was read the same way.
fn spelled_characters(source: &MySqlColumnMetadata) -> Option<u32> {
    if let Some(characters) = source.character_length() {
        return Some(characters);
    }
    if let Some(length) = unsigned_integer_column_length(source.type_name()) {
        return Some(length);
    }
    match source.type_name() {
        "TINYINT" | "BOOLEAN" | "YEAR" => Some(4),
        "SMALLINT" => Some(6),
        "MEDIUMINT" => Some(9),
        "INT" | "INTEGER" => Some(11),
        "BIGINT" => Some(20),
        // The width of `YYYY-MM-DD hh:mm:ss`, and for a day or a span of time
        // the width of `YYYY-MM-DD` and of `838:59:59`.
        "DATETIME" | "TIMESTAMP" => Some(19),
        "DATE" | "TIME" => Some(10),
        // A `DECIMAL`, a `FLOAT` and a `DOUBLE` are refused, because what
        // lands in the answer is the number spelled out and MySQL spells those
        // its own way: measured on 8.4.11, a `DECIMAL(10,2)` holding 1.50
        // spells `1.50` where the engine spells `1.5`, a `FLOAT` holding a
        // third spells `0.333333`, and a `DOUBLE` holding 1.2345678901234567e19
        // spells that. Answering a different string would be worse than
        // refusing the shape.
        _ => None,
    }
}

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

/// The precision and scale a `SUM` or an `AVG` widens, which counts an
/// unsigned whole number's digits too.
///
/// Measured on MySQL 8.4.11 through the lengths the two answer: a `TINYINT
/// UNSIGNED` counts 3 digits, an `INT UNSIGNED` 10 and a `BIGINT UNSIGNED` 20
/// — `SUM` over one answers 43 — and the answer is signed whatever the column
/// was.
#[cfg(unix)]
fn summed_shape_of(source: &MySqlColumnMetadata) -> Option<(u32, u32)> {
    decimal_shape_of(source).or_else(|| {
        Some((
            match source.type_name() {
                "TINYINT UNSIGNED" => 3,
                "SMALLINT UNSIGNED" => 5,
                "MEDIUMINT UNSIGNED" => 8,
                "INT UNSIGNED" | "INTEGER UNSIGNED" => 10,
                "BIGINT UNSIGNED" => 20,
                _ => return None,
            },
            0,
        ))
    })
}

/// Reports whether a column counts in whole numbers.
#[cfg(unix)]
fn is_whole_number_column(type_name: &str) -> bool {
    matches!(
        type_name,
        "TINYINT"
            | "SMALLINT"
            | "MEDIUMINT"
            | "INT"
            | "INTEGER"
            | "BIGINT"
            | "BOOLEAN"
            | "TINYINT UNSIGNED"
            | "SMALLINT UNSIGNED"
            | "MEDIUMINT UNSIGNED"
            | "INT UNSIGNED"
            | "INTEGER UNSIGNED"
            | "BIGINT UNSIGNED"
    )
}

/// Reports whether a column holds a number that is not counted in whole ones.
#[cfg(unix)]
fn is_real_column(type_name: &str) -> bool {
    matches!(
        type_name,
        "DECIMAL" | "DECIMAL UNSIGNED" | "DOUBLE" | "DOUBLE UNSIGNED" | "FLOAT" | "FLOAT UNSIGNED"
    )
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
    // Measured on MySQL 8.4.11: a clock reading to a fraction of a second
    // reports the places it was asked for and is that much wider, one more
    // for the point — `NOW(6)` a DATETIME of 26, `CURTIME(3)` a TIME of 12.
    if let ScalarFunction::NowToAFraction { places }
    | ScalarFunction::TimeOfDayToAFraction { places } = function
    {
        let moment = matches!(function, ScalarFunction::NowToAFraction { .. });
        let mut definition = column_definition(
            name,
            if moment {
                MYSQL_TYPE_DATETIME
            } else {
                MYSQL_TYPE_TIME
            },
        );
        definition.column_length = if moment { 20 } else { 9 } + places;
        definition.decimals = u8::try_from(places).map_err(|_| FrontendErrorKind::Internal)?;
        set_column_flags(&mut definition, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: a shift of a clock reading answers the same type the reading
    // does — a DATETIME of 19 for a moment, a DATE of 10 for a day shifted by
    // whole days — and is nullable where the reading itself is not. It reads
    // no column, so it is answered before anything asks which column it read.
    if matches!(
        function,
        ScalarFunction::ShiftsTheMoment | ScalarFunction::ShiftsTheDay
    ) {
        let day = function == ScalarFunction::ShiftsTheDay;
        let mut definition = column_definition(
            name,
            if day {
                MYSQL_TYPE_DATE
            } else {
                MYSQL_TYPE_DATETIME
            },
        );
        definition.column_length = if day { 10 } else { 19 };
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: a shift of a moment written out as a word answers a word — a
    // STRING of 116 in utf8mb4 with the not-fixed decimals value — whatever
    // the interval named.
    if function == ScalarFunction::ShiftsAWrittenMoment {
        let mut definition = column_definition(name, MYSQL_TYPE_STRING);
        definition.column_length = 116;
        definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        definition.decimals = NOT_FIXED_DECIMALS;
        set_column_flags(&mut definition, 0);
        return Ok(definition);
    }
    // Measured: `UNIX_TIMESTAMP()` with nothing to read answers a LONGLONG of
    // 21 reporting NOT NULL, and `FROM_UNIXTIME` a DATETIME of 19 that does
    // not — a count it cannot read answers no moment at all.
    if columns.is_empty()
        && matches!(
            function,
            ScalarFunction::CountsEpochSeconds | ScalarFunction::ReadsFromEpoch
        )
    {
        return Ok(epoch_call_definition(name, function, true));
    }
    // Measured: `JSON_CONTAINS` and `JSON_CONTAINS_PATH` answer a LONGLONG of
    // 21 carrying the binary and numeric flags, whatever they were given, and
    // `JSON_OVERLAPS` one of 1 — the width of the one digit it writes.
    if matches!(
        function,
        ScalarFunction::SearchesJson | ScalarFunction::SharesJson
    ) {
        let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
        definition.column_length = if function == ScalarFunction::SharesJson {
            1
        } else {
            21
        };
        definition.decimals = 0;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG);
        return Ok(definition);
    }
    // Measured: `JSON_ARRAY` and `JSON_OBJECT` answer the JSON type at the
    // widest a document can be, whatever they were given — so this is answered
    // before any column is looked at, the way `RAND()` is.
    if matches!(
        function,
        ScalarFunction::BuildsJson
            | ScalarFunction::ChangesJson
            | ScalarFunction::CollectsBuiltJson
    ) {
        for column_name in columns {
            let (table, ordinal) = source_metadata
                .ok_or(FrontendErrorKind::Unsupported)?
                .column_named(column_name)?;
            let source = table
                .columns
                .get(ordinal)
                .ok_or(FrontendErrorKind::Unsupported)?;
            if !writes_into_a_document_the_way_mysql_does(source) {
                return Err(FrontendErrorKind::Unsupported);
            }
        }
        let mut definition = column_definition(name, MYSQL_TYPE_JSON);
        definition.column_length = u32::MAX - 3;
        definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        definition.decimals = NOT_FIXED_DECIMALS;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: `RAND()` answers a double reporting NOT NULL, and `UUID()` a
    // VAR_STRING of 144 — the thirty-six characters it writes.
    if matches!(
        function,
        ScalarFunction::Randomises | ScalarFunction::Identifies
    ) {
        if function == ScalarFunction::Randomises {
            let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
            definition.column_length = 23;
            definition.decimals = NOT_FIXED_DECIMALS;
            set_column_flags(
                &mut definition,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            );
            return Ok(definition);
        }
        let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
        definition.column_length = 144;
        definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        definition.decimals = NOT_FIXED_DECIMALS;
        set_column_flags(&mut definition, 0);
        return Ok(definition);
    }
    // Measured on MySQL 8.4.11: `CURDATE()` and `CURRENT_DATE` each answer a
    // DATE of length 10, the width of the text form, with the NOT NULL and
    // binary flags. A DATE column reports the same 10 but is nullable.
    if function == ScalarFunction::NamesTheCircle {
        let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
        definition.column_length = 8;
        definition.decimals = 6;
        set_column_flags(
            &mut definition,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        );
        return Ok(definition);
    }
    if function == ScalarFunction::Today {
        let mut definition = column_definition(name, MYSQL_TYPE_DATE);
        definition.column_length = 10;
        set_column_flags(&mut definition, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: `CURTIME()` and `CURRENT_TIME` answer a TIME of length 8, the
    // width of `HH:MM:SS`, where a TIME column reports 10 — the same type is
    // narrower here because a clock reading holds no span past a day.
    if function == ScalarFunction::TimeOfDay {
        let mut definition = column_definition(name, MYSQL_TYPE_TIME);
        definition.column_length = 8;
        set_column_flags(&mut definition, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: ROW_NUMBER, RANK, DENSE_RANK and NTILE each answer a LONGLONG
    // of length 21 with no decimals, carrying the NOT NULL, unsigned and
    // numeric flags, whatever the window is over. They read no column.
    if function == ScalarFunction::RanksRows {
        let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
        definition.column_length = 21;
        definition.decimals = 0;
        set_column_flags(
            &mut definition,
            MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
        );
        return Ok(definition);
    }
    // Measured: PERCENT_RANK and CUME_DIST answer a DOUBLE of length 23 with
    // the not-fixed decimals value, NOT NULL and numeric but not binary.
    if function == ScalarFunction::RanksFraction {
        let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
        definition.column_length = 23;
        definition.decimals = NOT_FIXED_DECIMALS;
        set_column_flags(&mut definition, MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG);
        return Ok(definition);
    }
    // Measured: as wide as its widest string branch, and NOT NULL only when
    // there is an ELSE and no branch is NULL.
    if function == ScalarFunction::Branches {
        return Ok(text_call_definition(
            name,
            literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            not_null,
        ));
    }
    // Measured on MySQL 8.4.11: `DATE_FORMAT(NOW(), '%Y-%m-%d')` and
    // `STR_TO_DATE('2024-03-05', '%Y-%m-%d')` report exactly what the same
    // calls over a column report — the shape comes from the format, not from
    // what was read. Neither reads a column, so both are answered before
    // anything asks which column they read.
    if columns.is_empty() {
        if matches!(
            function,
            ScalarFunction::WritesAMoment | ScalarFunction::WritesAnEpochMoment
        ) {
            return Ok(written_moment_definition(name, literal_characters));
        }
        if matches!(
            function,
            ScalarFunction::ReadsADay | ScalarFunction::ReadsAClock | ScalarFunction::ReadsAMoment
        ) {
            return Ok(read_moment_definition(name, function));
        }
    }
    // Measured: `DATEDIFF(NOW(), '2024-01-01')` reports what the same count
    // over a column reports, and reads no column to be held to a date.
    if columns.is_empty()
        && matches!(
            function,
            ScalarFunction::CountsDaysBetween | ScalarFunction::CountsUnitsBetween
        )
    {
        return Ok(counted_between_definition(name, function));
    }
    // Measured: `DAYNAME(NOW())` and `WEEK(NOW())` report what the same call
    // over a column reports.
    if columns.is_empty()
        && matches!(
            function,
            ScalarFunction::NamesTheDayOrMonth
                | ScalarFunction::ReadsTheWeek
                | ScalarFunction::ReadsTheYearAndWeek
                | ScalarFunction::CountsDaysFromTheYearZero
        )
    {
        return Ok(calendar_name_or_week_definition(name, function));
    }
    // Measured on MySQL 8.4.11: `TIME_TO_SEC` answers a LONGLONG of 10 and
    // `SEC_TO_TIME` a TIME of 10, each nullable even over a NOT NULL column.
    // The seconds are counted in a TIME, a DATETIME or a DATE, and a time is
    // written out of a whole number.
    if matches!(
        function,
        ScalarFunction::CountsTheSecondsOfATime | ScalarFunction::WritesSecondsAsATime
    ) {
        let counts = function == ScalarFunction::CountsTheSecondsOfATime;
        for column_name in columns {
            let (table, ordinal) = source_metadata
                .ok_or(FrontendErrorKind::Unsupported)?
                .column_named(column_name)?;
            let source = table
                .columns
                .get(ordinal)
                .ok_or(FrontendErrorKind::Unsupported)?;
            let taken = if counts {
                matches!(source.type_name(), "TIME" | "DATETIME" | "DATE")
            } else {
                is_whole_number_column(source.type_name())
            };
            if !taken {
                return Err(FrontendErrorKind::Unsupported);
            }
        }
        let mut definition = column_definition(
            name,
            if counts {
                MYSQL_TYPE_LONGLONG
            } else {
                MYSQL_TYPE_TIME
            },
        );
        definition.column_length = 10;
        definition.decimals = 0;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured on MySQL 8.4.11: `INET_ATON` answers an unsigned LONGLONG of
    // 21, `INET_NTOA` a VAR_STRING of 124 — the fifteen characters an address
    // runs to and more — and both are nullable whatever they read, a word or a
    // number no address holds answering NULL. `IS_IPV4` answers a LONGLONG
    // of 1, NULL only where what it reads is. An address is read out of a
    // word and written out of a whole number.
    if matches!(
        function,
        ScalarFunction::ReadsAnAddress
            | ScalarFunction::WritesAnAddress
            | ScalarFunction::ChecksAnAddress
    ) {
        let mut reads_nothing_null = true;
        for column_name in columns {
            let (table, ordinal) = source_metadata
                .ok_or(FrontendErrorKind::Unsupported)?
                .column_named(column_name)?;
            let source = table
                .columns
                .get(ordinal)
                .ok_or(FrontendErrorKind::Unsupported)?;
            let reads_a_number = function == ScalarFunction::WritesAnAddress;
            if (reads_a_number && !is_whole_number_column(source.type_name()))
                || (!reads_a_number && !is_text_column(source))
            {
                return Err(FrontendErrorKind::Unsupported);
            }
            reads_nothing_null &= !source.nullable() && !table.outer;
        }
        let mut definition = match function {
            ScalarFunction::WritesAnAddress => text_call_definition(name, 124, false),
            _ => column_definition(name, MYSQL_TYPE_LONGLONG),
        };
        if function != ScalarFunction::WritesAnAddress {
            let checks = function == ScalarFunction::ChecksAnAddress;
            definition.column_length = if checks { 1 } else { 21 };
            definition.decimals = 0;
            set_column_flags(
                &mut definition,
                MYSQL_BINARY_FLAG
                    | if checks {
                        if reads_nothing_null {
                            MYSQL_NOT_NULL_FLAG
                        } else {
                            0
                        }
                    } else {
                        MYSQL_UNSIGNED_FLAG
                    },
            );
        }
        return Ok(definition);
    }
    // Measured on MySQL 8.4.11: a comparison, `NOT col` and `col IS TRUE`
    // each answer a LONGLONG of length 1. A comparison or a negation is NOT
    // NULL where what it reads cannot be null — `id > 1` and `NOT id` over a
    // key, `COUNT(*) > 0` always — and the truth tests always are.
    if matches!(
        function,
        ScalarFunction::Compares | ScalarFunction::NegatesTruth | ScalarFunction::TestsTruth
    ) {
        let mut reads_nothing_null = true;
        for column_name in columns {
            let (table, ordinal) = source_metadata
                .ok_or(FrontendErrorKind::Unsupported)?
                .column_named(column_name)?;
            let source = table
                .columns
                .get(ordinal)
                .ok_or(FrontendErrorKind::Unsupported)?;
            // MySQL reads a word or a DECIMAL as a truth by a rule of its own
            // — `NOT 'apple'` is 1 — which the engine does not share.
            if function != ScalarFunction::Compares
                && !is_signed_whole_number_column(source.type_name())
                && source.type_name() != "DOUBLE"
            {
                return Err(FrontendErrorKind::Unsupported);
            }
            reads_nothing_null &= !source.nullable() && !table.outer;
        }
        let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
        definition.column_length = 1;
        set_column_flags(
            &mut definition,
            MYSQL_BINARY_FLAG
                | if not_null || reads_nothing_null {
                    MYSQL_NOT_NULL_FLAG
                } else {
                    0
                },
        );
        return Ok(definition);
    }
    let source_metadata = source_metadata.ok_or(FrontendErrorKind::Unsupported)?;
    // Measured: the answer is as wide as its arguments laid end to end, a
    // string literal counting the characters it spells.
    // Measured on MySQL 8.4.11: beside a `TEXT` the answer is a MEDIUM_BLOB
    // four times as wide again — every part, the `TEXT`'s own 262140 bytes and
    // each word and column beside it, counts four times over, so `CONCAT(t)`
    // reports 1048560 and `CONCAT(t, 'x')` 1048576.
    if function == ScalarFunction::Concatenates {
        let mut width = literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        let mut beside_a_text = false;
        for column_name in columns {
            let (table, ordinal) = source_metadata.column_named(column_name)?;
            let source = table
                .columns
                .get(ordinal)
                // An `information_schema` table names its columns itself, and an
                // aggregate or a call over one of them has not been measured.
                .ok_or(FrontendErrorKind::Unsupported)?;
            let length = if source.type_name() == "TEXT" {
                beside_a_text = true;
                MYSQL_TEXT_CHARACTERS
            } else {
                spelled_characters(source).ok_or(FrontendErrorKind::Unsupported)?
            };
            width = width.saturating_add(length.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER));
        }
        if beside_a_text {
            return medium_blob_text_definition(
                name,
                width.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            );
        }
        return Ok(text_call_definition(name, width, not_null));
    }
    // Measured: GREATEST and LEAST take the widest shape among their arguments.
    if function == ScalarFunction::Widest {
        let (first_table, first_ordinal) = source_metadata.column_named(&columns[0])?;
        let first_source = &first_table.columns[first_ordinal];
        let text_mode = is_text_column(first_source);

        if !text_mode && literal_characters > 0 {
            return Err(FrontendErrorKind::Unsupported);
        }

        let mut max_width = literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        let mut all_not_null = true;

        for column_name in columns {
            let (table, ordinal) = source_metadata.column_named(column_name)?;
            let source = table
                .columns
                .get(ordinal)
                // An `information_schema` table names its columns itself, and an
                // aggregate or a call over one of them has not been measured.
                .ok_or(FrontendErrorKind::Unsupported)?;
            if is_text_column(source) != text_mode {
                return Err(FrontendErrorKind::Unsupported);
            }
            if source.nullable() {
                all_not_null = false;
            }
            if text_mode {
                let length = source
                    .character_length()
                    .ok_or(FrontendErrorKind::Unsupported)?;
                let col_width = length.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
                max_width = max_width.max(col_width);
            }
        }

        if text_mode {
            return Ok(text_call_definition(
                name,
                max_width,
                not_null && all_not_null,
            ));
        } else {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 11;
            definition.decimals = 0;
            set_column_flags(
                &mut definition,
                MYSQL_BINARY_FLAG | if all_not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
            );
            return Ok(definition);
        }
    }
    // Measured on MySQL 8.4.11: `DATEDIFF(b, a)` answers a LONGLONG of length
    // 9, and it is nullable because either date may be. Every column counted
    // from has to hold a date: what MySQL does with anything else is a
    // coercion.
    if matches!(
        function,
        ScalarFunction::CountsDaysBetween | ScalarFunction::CountsUnitsBetween
    ) {
        for column_name in columns {
            let (table, ordinal) = source_metadata.column_named(column_name)?;
            let source = table
                .columns
                .get(ordinal)
                // An `information_schema` table names its columns itself, and an
                // aggregate or a call over one of them has not been measured.
                .ok_or(FrontendErrorKind::Unsupported)?;
            if !matches!(source.type_name(), "DATE" | "DATETIME" | "TIMESTAMP") {
                return Err(FrontendErrorKind::Unsupported);
            }
        }
        return Ok(counted_between_definition(name, function));
    }
    if matches!(
        function,
        ScalarFunction::NamesTheDayOrMonth
            | ScalarFunction::ReadsTheWeek
            | ScalarFunction::ReadsTheYearAndWeek
            | ScalarFunction::CountsDaysFromTheYearZero
    ) {
        let [column_name] = columns else {
            return Err(FrontendErrorKind::Internal);
        };
        let (table, ordinal) = source_metadata.column_named(column_name)?;
        let source = table
            .columns
            .get(ordinal)
            // An `information_schema` table names its columns itself, and an
            // aggregate or a call over one of them has not been measured.
            .ok_or(FrontendErrorKind::Unsupported)?;
        if !matches!(source.type_name(), "DATE" | "DATETIME" | "TIMESTAMP") {
            return Err(FrontendErrorKind::Unsupported);
        }
        return Ok(calendar_name_or_week_definition(name, function));
    }
    // Every call past here reads one column and answers its shape. `NULLIF`
    // is the one that may name a second: it compares the two and answers the
    // first's shape, so the first is the one read here and the second is
    // checked beside it.
    let ([column_name] | [column_name, _]) = columns else {
        return Err(FrontendErrorKind::Internal);
    };
    let (table, ordinal) = source_metadata.column_named(column_name)?;
    let source = table
        .columns
        .get(ordinal)
        // An `information_schema` table names its columns itself, and an
        // aggregate or a call over one of them has not been measured.
        .ok_or(FrontendErrorKind::Unsupported)?;
    // Measured on MySQL 8.4.11: writing a column out answers a VAR_STRING as
    // wide as the column's own display width counted in utf8mb4 bytes — an
    // INT of 11 answers 44, a BIGINT of 20 answers 80, a DATETIME of 19
    // answers 76 and a DATE of 10 answers 40 — and it is nullable with no
    // flags. Only the columns the engine writes out the same way are taken: a
    // DECIMAL keeps its declared scale in MySQL and not here, so `1.50` would
    // come back as `1.5`, and a DOUBLE prints by a rule of its own.
    if function == ScalarFunction::CastsToText {
        // The width is four times what the column can spell, a character of
        // utf8mb4 running to four bytes. Every kind whose spelling the engine
        // writes out the way MySQL does is taken; a DECIMAL, a FLOAT and a
        // DOUBLE answer nothing here, for the reason above.
        let Some(characters) = spelled_characters(source) else {
            return Err(FrontendErrorKind::Unsupported);
        };
        let mut definition = source_metadata.table_column_definition(table, ordinal, name, None)?;
        definition.column_length = characters * UTF8MB4_MAX_BYTES_PER_CHARACTER;
        definition.column_type = MYSQL_TYPE_VAR_STRING;
        definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        definition.decimals = NOT_FIXED_DECIMALS;
        definition.flags = 0;
        return Ok(definition);
    }
    // Measured: reading a whole number out of a column answers a LONGLONG of
    // 21 carrying the binary flag, nullable. A word is not read here: measured,
    // `CAST('  7 apples' AS SIGNED)` answers 7 and warns, and the warning is
    // not raised here.
    if function == ScalarFunction::CastsToWholeNumber {
        if !is_whole_number_column(source.type_name()) && !is_real_column(source.type_name()) {
            return Err(FrontendErrorKind::Unsupported);
        }
        let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
        definition.column_length = 21;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: reading the day out answers a DATE of 10 and reading the
    // moment out answers a DATETIME of 19, both nullable and both carrying the
    // binary flag. A word is not read into either: measured, a word that names
    // no day answers NULL and warns.
    if matches!(
        function,
        ScalarFunction::CastsToDay | ScalarFunction::CastsToMoment
    ) {
        if !matches!(source.type_name(), "DATE" | "DATETIME" | "TIMESTAMP") {
            return Err(FrontendErrorKind::Unsupported);
        }
        let day = function == ScalarFunction::CastsToDay;
        let mut definition = column_definition(
            name,
            if day {
                MYSQL_TYPE_DATE
            } else {
                MYSQL_TYPE_DATETIME
            },
        );
        definition.column_length = if day { 10 } else { 19 };
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured: LAG, LEAD, FIRST_VALUE, LAST_VALUE and NTH_VALUE answer the
    // column's own shape, widened to LONGLONG where it is an integer, and are
    // always nullable because the row they reach for may not be there. They
    // carry the numeric flag and, unlike ABS, not the binary one, and a text
    // column keeps its collation.
    //
    // A default for the row that is not there widens the answer to its own
    // width, and over a NOT NULL column makes it NOT NULL too: measured,
    // `LAG(nn, 1, 0)` over an `INT NOT NULL` reports NOT NULL where `LAG(nn)`
    // does not.
    if matches!(
        function,
        ScalarFunction::ShiftsRow
            | ScalarFunction::ShiftsRowOrNumber
            | ScalarFunction::ShiftsRowOrWord
    ) {
        let defaults_to_a_number = function == ScalarFunction::ShiftsRowOrNumber;
        let defaults_to_a_word = function == ScalarFunction::ShiftsRowOrWord;
        if (defaults_to_a_number && !is_whole_number_column(source.type_name()))
            || (defaults_to_a_word && source.type_name() != "VARCHAR")
        {
            return Err(FrontendErrorKind::Unsupported);
        }
        let mut definition = source_metadata.table_column_definition(table, ordinal, name, None)?;
        if matches!(
            definition.column_type,
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        ) {
            definition.column_type = MYSQL_TYPE_LONGLONG;
        }
        let default_width = if defaults_to_a_word {
            literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER)
        } else {
            literal_characters
        };
        definition.column_length = definition.column_length.max(default_width);
        definition.schema.clear();
        definition.table.clear();
        definition.original_table.clear();
        definition.original_name.clear();
        let mut flags = if is_text_column(source) {
            0
        } else {
            MYSQL_NUM_FLAG
        };
        if function != ScalarFunction::ShiftsRow && !source.nullable() && !table.outer {
            flags |= MYSQL_NOT_NULL_FLAG;
        }
        set_column_flags(&mut definition, flags);
        return Ok(definition);
    }
    // Measured: the count is read out of a DATE, a DATETIME or a TIMESTAMP and
    // nothing else — MySQL reads one out of a number by coercing it, which
    // this has not measured — and the moment out of a whole number.
    if matches!(
        function,
        ScalarFunction::CountsEpochSeconds | ScalarFunction::ReadsFromEpoch
    ) {
        let reads_a_moment = matches!(source.type_name(), "DATE" | "DATETIME" | "TIMESTAMP");
        let counts = function == ScalarFunction::CountsEpochSeconds;
        if counts != reads_a_moment || (!counts && is_text_column(source)) {
            return Err(FrontendErrorKind::Unsupported);
        }
        return Ok(epoch_call_definition(name, function, false));
    }
    // Measured on MySQL 8.4.11: `FORMAT` answers a VAR_STRING whose width is
    // the column's own length plus a comma for every three of its digits plus
    // thirty for the widest fraction it writes and two more — 184 over an INT
    // of 11, 232 over a BIGINT of 20, 244 over a DOUBLE of 22, and the same
    // 192 over a FLOAT of 12 and a DECIMAL(10,3) of 12. The count it is asked
    // for does not change the width.
    // Measured: `TRUNCATE` answers a LONGLONG of 21 over any integer column, a
    // DOUBLE of 23 over a FLOAT or a DOUBLE, and over a DECIMAL a NEWDECIMAL
    // of its own — `DECIMAL(10,3)` cut at two places reports 11 with a scale
    // of 2, the precision losing the digit the scale lost. Only the last needs
    // the count, so a DECIMAL is refused until the count can be read here.
    if function == ScalarFunction::CutsDigits {
        // Measured: a NOT NULL column cut short cannot be null either.
        let not_null_flag = if !source.nullable() && !table.outer {
            MYSQL_NOT_NULL_FLAG
        } else {
            0
        };
        // Measured: over a DECIMAL the answer is a DECIMAL of its own — the
        // scale is the count it was asked for held to the column's, and the
        // width is the column's whole digits plus a sign, plus the fraction
        // and its point where there is one. `DECIMAL(10,3)` cut at two reports
        // 11 with a scale of 2, at five reports the column's own 12 and 3, and
        // at zero or below reports 8 with no scale at all.
        if let Some((precision, scale)) = source.decimal_size() {
            let cut = literal_characters.min(scale);
            let mut definition = column_definition(name, MYSQL_TYPE_NEWDECIMAL);
            definition.column_length = precision.saturating_sub(scale).saturating_add(1)
                + if cut > 0 { cut + 1 } else { 0 };
            definition.decimals = cut as u8;
            set_column_flags(
                &mut definition,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | not_null_flag,
            );
            return Ok(definition);
        }
        let mut definition = match source.type_name() {
            "FLOAT" | "DOUBLE" | "FLOAT UNSIGNED" | "DOUBLE UNSIGNED" => {
                let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
                definition.column_length = 23;
                definition.decimals = NOT_FIXED_DECIMALS;
                definition
            }
            "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT" | "BOOLEAN"
            | "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
            | "INTEGER UNSIGNED" | "BIGINT UNSIGNED" => {
                let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
                definition.column_length = 21;
                definition.decimals = 0;
                definition
            }
            _ => return Err(FrontendErrorKind::Unsupported),
        };
        set_column_flags(
            &mut definition,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | not_null_flag,
        );
        return Ok(definition);
    }
    if function == ScalarFunction::GroupsDigits {
        if is_text_column(source) {
            return Err(FrontendErrorKind::Unsupported);
        }
        let own = source_metadata.table_column_definition(table, ordinal, name.clone(), None)?;
        let length = own.column_length;
        let width = length
            .saturating_add(length / 3)
            .saturating_add(32)
            .saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
        definition.column_length = width;
        definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        definition.decimals = NOT_FIXED_DECIMALS;
        set_column_flags(&mut definition, 0);
        return Ok(definition);
    }
    let wants_text = matches!(
        function,
        ScalarFunction::KeepsTextShape
            | ScalarFunction::CountsText
            | ScalarFunction::TakesCharacters
            | ScalarFunction::TakesASubstring { .. }
            | ScalarFunction::SplitsOnADelimiter
            | ScalarFunction::Digests
            | ScalarFunction::Repeats
            | ScalarFunction::Locates
            | ScalarFunction::QuotesAsJson
            | ScalarFunction::ReadsADay
            | ScalarFunction::ReadsAClock
            | ScalarFunction::ReadsAMoment
            | ScalarFunction::FindsThePlace
            | ScalarFunction::DefaultedText
            | ScalarFunction::ReadsTheFirstByte
            | ScalarFunction::ReadsTheFirstCharacter
            | ScalarFunction::ChecksTheBytes
            | ScalarFunction::QuotesForSql
            | ScalarFunction::EncodesInBase64
    );
    // Measured on MySQL 8.4.11, `YEAR` over a TIME column answers the current
    // year, which is a coercion rather than a reading, so the readings are
    // held to the columns that hold what they read.
    if matches!(
        function,
        ScalarFunction::ReadsTheYear | ScalarFunction::ReadsAMonthOrDay
    ) && !matches!(source.type_name(), "DATE" | "DATETIME" | "TIMESTAMP")
    {
        return Err(FrontendErrorKind::Unsupported);
    }
    // Measured on MySQL 8.4.11: shifting a DATE by whole days, months or years
    // answers a DATE, and every other shift — a time interval, or any shift
    // of a DATETIME — answers a DATETIME. A TIME holds no date to shift. A
    // moment keeps its fraction of a second: a `DATETIME(3)` shifted answers
    // a DATETIME of 23 with 3 decimals, and so does a `TIMESTAMP(3)`.
    if matches!(
        function,
        ScalarFunction::ShiftsByWholeDays | ScalarFunction::ShiftsByTime
    ) {
        if !matches!(source.type_name(), "DATE" | "DATETIME" | "TIMESTAMP") {
            return Err(FrontendErrorKind::Unsupported);
        }
        let keeps_the_day =
            function == ScalarFunction::ShiftsByWholeDays && source.type_name() == "DATE";
        let mut definition = column_definition(
            name,
            if keeps_the_day {
                MYSQL_TYPE_DATE
            } else {
                MYSQL_TYPE_DATETIME
            },
        );
        let precision = if keeps_the_day {
            0
        } else {
            source.temporal_precision().unwrap_or(0)
        };
        definition.column_length = match (keeps_the_day, precision) {
            (true, _) => 10,
            (false, 0) => 19,
            (false, precision) => 20 + u32::from(precision),
        };
        definition.decimals = precision;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // A TIME is left out of the clock readings as well, for a reason of its
    // own: it holds a span running to 838 hours, which MySQL reads out whole
    // where the engine has no reader for it.
    if matches!(
        function,
        ScalarFunction::ReadsTheHour | ScalarFunction::ReadsAMinuteOrSecond
    ) && !matches!(source.type_name(), "DATETIME" | "TIMESTAMP")
    {
        return Err(FrontendErrorKind::Unsupported);
    }
    // MySQL takes each of these over the other kind by coercing it, which has
    // not been measured, so each is answered only over the kind it is for.
    // Measured over a utf8mb4 connection: `HEX` over a `VARCHAR(8)` reports 256
    // — two hex characters for each of the eight, and the four bytes utf8mb4
    // reserves for each of those — and over a column holding a number it
    // reports 64 whatever the number's width is, because a number is written
    // in at most sixteen hexadecimal characters.
    if function == ScalarFunction::Hexadecimal {
        let width = match source.character_length() {
            Some(length) => length.saturating_mul(8).saturating_mul(4),
            None if is_text_column(source) => return Err(FrontendErrorKind::Unsupported),
            None => 64,
        };
        return Ok(text_call_definition(name, width, not_null));
    }
    // Measured on MySQL 8.4.11: `LPAD(id, 5, '0')` writes the number out and
    // pads it, `00001`, and `LEFT` cuts it the same way, which the engine
    // does for every kind it spells the way MySQL does. A `DOUBLE` or a
    // `DECIMAL` with places is spelled by a rule of its own.
    let cuts_or_pads_a_spelled_value = matches!(
        function,
        ScalarFunction::TakesCharacters
            | ScalarFunction::ChecksTheBytes
            | ScalarFunction::QuotesForSql
            | ScalarFunction::EncodesInBase64
    ) && spelled_characters(source).is_some();
    if wants_text != is_text_column(source)
        && function != ScalarFunction::NullsOnMatch
        && !cuts_or_pads_a_spelled_value
    {
        return Err(FrontendErrorKind::Unsupported);
    }
    // Measured on MySQL 8.4.11: `MD5`, `SHA1` and `SHA2` each answer a
    // VAR_STRING as wide as the hexadecimal characters of the digest, four
    // bytes to the character — 128, 160, and 224 to 512 — whatever the column
    // was. A number is refused above: MySQL writes it out before it digests
    // it, and the engine would digest nothing.
    if function == ScalarFunction::Digests {
        return Ok(text_call_definition(
            name,
            literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            not_null,
        ));
    }
    // Measured on MySQL 8.4.11: `SUBSTRING_INDEX` answers a VAR_STRING as wide
    // as its column — a `CHAR(8)` reports 32 — however the column is cut.
    if function == ScalarFunction::SplitsOnADelimiter {
        let length = source
            .character_length()
            .ok_or(FrontendErrorKind::Unsupported)?;
        return Ok(text_call_definition(
            name,
            length.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            not_null,
        ));
    }
    // Measured on MySQL 8.4.11 over a `VARCHAR(100)`: what is left of the
    // column after the place, held to the count — `SUBSTR(name, 2)` reports
    // 396, `SUBSTR(name, -10)` 40, `SUBSTR(name, 99, 5)` 8, and a place of 0, a
    // place reaching back past the start or a count below one report 0.
    if let ScalarFunction::TakesASubstring { from, count } = function {
        let length = source
            .character_length()
            .ok_or(FrontendErrorKind::Unsupported)?;
        return Ok(text_call_definition(
            name,
            substring_characters(length, from, count)
                .saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            not_null,
        ));
    }
    // Measured: as wide as the count it was asked for, whatever the column is.
    // Measured on MySQL 8.4.11: `QUOTE` reserves two characters for each one
    // the value can spell and two for the quotes — 88 over a VARCHAR(10), 96
    // over an INT — and `TO_BASE64` what the base64 of the value's bytes runs
    // to, a newline after every 76 characters counted — 324 over a
    // VARCHAR(15), whose 60 bytes answer 81 — a number or a moment spelling
    // one byte to the character. Both are nullable with no flags.
    if matches!(
        function,
        ScalarFunction::QuotesForSql | ScalarFunction::EncodesInBase64
    ) {
        let characters = spelled_characters(source).ok_or(FrontendErrorKind::Unsupported)?;
        let written = if function == ScalarFunction::QuotesForSql {
            u64::from(characters) * 2 + 2
        } else {
            let bytes_per_character = if is_text_column(source) {
                UTF8MB4_MAX_BYTES_PER_CHARACTER
            } else {
                1
            };
            turso_mysql_parser::base64_length(
                u64::from(characters) * u64::from(bytes_per_character),
            )
        };
        let width = u32::try_from(written * u64::from(UTF8MB4_MAX_BYTES_PER_CHARACTER))
            .map_err(|_| FrontendErrorKind::Unsupported)?;
        return Ok(text_call_definition(name, width, false));
    }
    if function == ScalarFunction::TakesCharacters {
        return Ok(text_call_definition(
            name,
            literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            not_null,
        ));
    }
    // Measured: as wide as the column's length times the count it was asked for.
    if function == ScalarFunction::Repeats {
        let length = source
            .character_length()
            .ok_or(FrontendErrorKind::Unsupported)?;
        let width = length
            .saturating_mul(literal_characters)
            .saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        return Ok(text_call_definition(name, width, not_null));
    }
    let own_shape =
        |name: String| source_metadata.table_column_definition(table, ordinal, name, None);
    if function == ScalarFunction::NullsOnMatch {
        // A second column travels with the first where the statement compared
        // two of them. MySQL compares them by their own rules — a number
        // against a word reads the word as a number, and two words are
        // compared without regard to case — and the engine compares them by
        // their kinds, so only two columns that count in whole numbers are
        // taken; the shape is still the first's.
        if let Some(compared) = columns.get(1) {
            let (table, ordinal) = source_metadata.column_named(compared)?;
            let compared = table
                .columns
                .get(ordinal)
                .ok_or(FrontendErrorKind::Unsupported)?;
            if !is_whole_number_column(source.type_name())
                || !is_whole_number_column(compared.type_name())
            {
                return Err(FrontendErrorKind::Unsupported);
            }
        }
        let mut definition = own_shape(name)?;
        definition.schema.clear();
        definition.table.clear();
        definition.original_table.clear();
        definition.original_name.clear();
        let binary = if is_text_column(source) {
            0
        } else {
            MYSQL_BINARY_FLAG
        };
        set_column_flags(&mut definition, binary);
        definition.flags &= !MYSQL_NOT_NULL_FLAG;
        return Ok(definition);
    }
    // Measured on MySQL 8.4.11 over a JSON column: JSON_EXTRACT answers the
    // JSON type at the widest length a document has minus its quotes,
    // JSON_UNQUOTE answers a LONG_BLOB at the widest length there is, and both
    // carry the text collation with the binary flag. JSON_VALID answers a
    // LONGLONG of 21 with the binary collation.
    // Measured on MySQL 8.4.11: STR_TO_DATE answers the type its format names —
    // a DATE of 10, a TIME of 10 or a DATETIME of 19 — each with the binary
    // collation and flag, as a stored column of that type reports.
    if matches!(
        function,
        ScalarFunction::ReadsADay | ScalarFunction::ReadsAClock | ScalarFunction::ReadsAMoment
    ) {
        if !is_text_column(source) {
            return Err(FrontendErrorKind::Unsupported);
        }
        return Ok(read_moment_definition(name, function));
    }
    // Measured on MySQL 8.4.11: DATE_FORMAT answers a VAR_STRING as wide as the
    // format could make it, with the text collation and no flags at all — not
    // even the binary one every other reading of a moment carries.
    if function == ScalarFunction::WritesAMoment {
        if !matches!(source.type_name(), "DATE" | "DATETIME" | "TIMESTAMP") {
            return Err(FrontendErrorKind::Unsupported);
        }
        return Ok(written_moment_definition(name, literal_characters));
    }
    // A count of seconds is a whole number: MySQL carries a fraction into the
    // moment and reads a word as the number it begins with, neither of which
    // has been measured here.
    if function == ScalarFunction::WritesAnEpochMoment {
        if !is_whole_number_column(source.type_name()) {
            return Err(FrontendErrorKind::Unsupported);
        }
        return Ok(written_moment_definition(name, literal_characters));
    }
    // Measured on MySQL 8.4.11: JSON_QUOTE answers a VAR_STRING as wide as the
    // six characters each of its column's could need plus its two quotes, and
    // carries the text collation with the binary flag.
    if function == ScalarFunction::QuotesAsJson {
        let length = source
            .character_length()
            .ok_or(FrontendErrorKind::Unsupported)?;
        let width = length
            .saturating_mul(6)
            .saturating_add(2)
            .saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
        definition.column_length = width;
        definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        definition.decimals = NOT_FIXED_DECIMALS;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return Ok(definition);
    }
    // Measured over a JSON column: JSON_EXTRACT and JSON_KEYS answer the JSON
    // type at the widest length a document has minus its quotes, JSON_UNQUOTE
    // answers a LONG_BLOB at the widest length there is, JSON_TYPE a
    // VAR_STRING of 68, and JSON_LENGTH and JSON_VALID a LONGLONG of 21 with
    // the binary collation. All but the last two carry the text collation, and
    // every one of them the binary flag.
    if matches!(
        function,
        ScalarFunction::ReadsAJsonValue
            | ScalarFunction::ReadsJsonText
            | ScalarFunction::ChecksJson
            | ScalarFunction::NamesAJsonKind
            | ScalarFunction::CountsJsonMembers
            | ScalarFunction::ListsJsonKeys
    ) {
        if source.type_name() != "JSON" {
            return Err(FrontendErrorKind::Unsupported);
        }
        let mut definition = match function {
            ScalarFunction::ReadsAJsonValue | ScalarFunction::ListsJsonKeys => {
                let mut definition = column_definition(name, MYSQL_TYPE_JSON);
                definition.column_length = u32::MAX - 3;
                definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
                definition.decimals = NOT_FIXED_DECIMALS;
                definition
            }
            ScalarFunction::ReadsJsonText => {
                let mut definition = column_definition(name, MYSQL_TYPE_LONG_BLOB);
                definition.column_length = u32::MAX;
                definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
                definition.decimals = NOT_FIXED_DECIMALS;
                definition
            }
            ScalarFunction::NamesAJsonKind => {
                let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
                definition.column_length = 68;
                definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
                definition.decimals = NOT_FIXED_DECIMALS;
                definition
            }
            _ => {
                let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
                definition.column_length = 21;
                definition.decimals = 0;
                definition
            }
        };
        let numeric = if matches!(
            function,
            ScalarFunction::ChecksJson | ScalarFunction::CountsJsonMembers
        ) {
            MYSQL_NUM_FLAG
        } else {
            0
        };
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG | numeric);
        return Ok(definition);
    }
    let mut definition = match function {
        ScalarFunction::ReadsAJsonValue
        | ScalarFunction::ReadsJsonText
        | ScalarFunction::ChecksJson
        | ScalarFunction::NamesAJsonKind
        | ScalarFunction::CountsJsonMembers
        | ScalarFunction::ListsJsonKeys
        | ScalarFunction::QuotesAsJson
        | ScalarFunction::WritesAMoment
        | ScalarFunction::ReadsADay
        | ScalarFunction::ReadsAClock
        | ScalarFunction::ReadsAMoment
        | ScalarFunction::Randomises
        | ScalarFunction::Identifies
        | ScalarFunction::Digests
        | ScalarFunction::BuildsJson
        | ScalarFunction::ChangesJson
        | ScalarFunction::CollectsBuiltJson
        | ScalarFunction::GroupsDigits
        | ScalarFunction::CutsDigits
        | ScalarFunction::SearchesJson
        | ScalarFunction::SharesJson
        | ScalarFunction::CountsEpochSeconds
        | ScalarFunction::ReadsFromEpoch
        | ScalarFunction::WritesAnEpochMoment => {
            unreachable!("a JSON, moment or plain reading answered above")
        }
        // Measured on MySQL 8.4.11: over a `TEXT` the answer is a MEDIUM_BLOB
        // of 1048560, the `TEXT`'s 262140 bytes counted four times over, and
        // over a `MEDIUMTEXT` a LONG_BLOB, which is not taken.
        ScalarFunction::KeepsTextShape if source.type_name() == "TEXT" => {
            medium_blob_text_definition(
                name,
                MYSQL_TEXT_CHARACTERS
                    .saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER)
                    .saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            )?
        }
        ScalarFunction::KeepsTextShape
            if matches!(source.type_name(), "MEDIUMTEXT" | "LONGTEXT") =>
        {
            return Err(FrontendErrorKind::Unsupported);
        }
        ScalarFunction::KeepsTextShape => {
            let mut definition = own_shape(name)?;
            // Measured: the answer is a VAR_STRING whatever the argument was,
            // so a CHAR argument widens to it.
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
        ScalarFunction::Locates => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 11;
            definition.decimals = 0;
            definition
        }
        // Measured on MySQL 8.4.11: `ASCII` answers a LONGLONG of 3, `ORD`
        // one of 21 and `CRC32` an unsigned one of 10.
        ScalarFunction::ReadsTheFirstByte
        | ScalarFunction::ReadsTheFirstCharacter
        | ScalarFunction::ChecksTheBytes => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = match function {
                ScalarFunction::ReadsTheFirstByte => 3,
                ScalarFunction::ReadsTheFirstCharacter => 21,
                _ => 10,
            };
            definition.decimals = 0;
            definition
        }
        ScalarFunction::QuotesForSql | ScalarFunction::EncodesInBase64 => {
            unreachable!("QUOTE and TO_BASE64 were answered above")
        }
        ScalarFunction::ReadsAnAddress
        | ScalarFunction::WritesAnAddress
        | ScalarFunction::ChecksAnAddress
        | ScalarFunction::CountsTheSecondsOfATime
        | ScalarFunction::WritesSecondsAsATime => {
            unreachable!("the address and time calls were answered above")
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
        // Measured on MySQL 8.4.11: `MOD(n, 2)`, `n % 2`, `n DIV 2` and `-n`
        // each answer a LONGLONG as wide as the column — 4 over a TINYINT, 6
        // over a SMALLINT, 11 over an INT and 20 over a BIGINT, and 4 over a
        // TINYINT(1), which reports 1 on its own. Over a DOUBLE or a DECIMAL
        // they answer a real or a decimal instead, which is not taken here.
        // A BIGINT is not negated: its smallest value has no negative, which
        // MySQL answers 1690 for and the engine a real number.
        ScalarFunction::Modulo | ScalarFunction::DividesWhole | ScalarFunction::Negates => {
            if !is_signed_whole_number_column(source.type_name())
                || (function == ScalarFunction::Negates && source.type_name() == "BIGINT")
            {
                return Err(FrontendErrorKind::Unsupported);
            }
            let mut definition = own_shape(name)?;
            definition.column_type = MYSQL_TYPE_LONGLONG;
            if source.type_name() == "BOOLEAN" {
                definition.column_length = 4;
            }
            definition
        }
        ScalarFunction::RoundsToPlaces { places } => {
            rounded_definition(name, source, Some(places))?
        }
        ScalarFunction::RoundsToWhole => rounded_definition(name, source, None)?,
        // Measured: a whole number of length 21 however wide the argument was.
        ScalarFunction::Truncates => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 21;
            definition
        }
        // Measured on MySQL 8.4.11: `PI()` answers a DOUBLE of length 8 with
        // 6 decimals, reporting NOT NULL — the one reading here that names a
        // number of its own rather than working one out.
        ScalarFunction::NamesTheCircle => {
            unreachable!("PI was answered above")
        }
        // Measured on MySQL 8.4.11: `BIN(n)` and `OCT(n)` each answer a
        // VAR_STRING of length 260 with the not-fixed decimals and no flags at
        // all — wide enough for the sixty-four bits a whole number can carry,
        // whatever the column's own width was. A word is refused above: MySQL
        // reads one as the number it names.
        ScalarFunction::WritesInAnotherRadix => {
            if !is_whole_number_column(source.type_name()) {
                return Err(FrontendErrorKind::Unsupported);
            }
            let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
            definition.column_length = 260;
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            definition.decimals = NOT_FIXED_DECIMALS;
            set_column_flags(&mut definition, 0);
            definition
        }
        // Measured: `FIELD` answers a LONGLONG of length 3 reporting NOT NULL
        // — a word that is not among the choices, and one that is nothing at
        // all, each answer 0 rather than nothing.
        ScalarFunction::FindsThePlace => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 3;
            definition.decimals = 0;
            definition
        }
        // Measured: `ELT` answers a VAR_STRING as wide as its widest choice,
        // four bytes to the character, and is nullable — a place below one or
        // past the last answers nothing.
        ScalarFunction::ReadsThePlace => {
            let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
            definition.column_length =
                literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            definition.decimals = NOT_FIXED_DECIMALS;
            set_column_flags(&mut definition, 0);
            definition
        }
        // Measured: SQRT and POW answer a DOUBLE of length 23 and not-fixed decimals.
        ScalarFunction::Approximates => {
            let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
            definition.column_length = 23;
            definition.decimals = NOT_FIXED_DECIMALS;
            definition
        }
        // Measured: the column's own shape, and NOT NULL because the fallback
        // cannot be null. A whole number widens to a LONGLONG, and a `DOUBLE`
        // or a `FLOAT` keeps its kind at a length of 23. A day or a moment
        // answers a VAR_STRING and a JSON a LONG_BLOB, which are refused
        // rather than modelled.
        ScalarFunction::Defaulted => {
            let mut definition = own_shape(name)?;
            match definition.column_type {
                MYSQL_TYPE_NEWDECIMAL => {}
                MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
                | MYSQL_TYPE_LONGLONG => definition.column_type = MYSQL_TYPE_LONGLONG,
                MYSQL_TYPE_DOUBLE | MYSQL_TYPE_FLOAT => {
                    definition.column_length = 23;
                    definition.decimals = NOT_FIXED_DECIMALS;
                }
                _ => return Err(FrontendErrorKind::Unsupported),
            }
            definition
        }
        // Measured: a written word falling back onto a column of words answers
        // that column's own width whatever the word's own is —
        // `IFNULL(email, 'none')` and `IFNULL(email, 'x')` over a
        // `VARCHAR(80)` both report a `VAR_STRING` of 320 — and reports
        // `VAR_STRING` even over a `CHAR`, which on its own reports `STRING`.
        // A `TEXT` is refused: it reports four times its own width there, a
        // rule of its own that has not been measured further.
        ScalarFunction::DefaultedText => {
            if !matches!(source.type_name(), "VARCHAR" | "CHAR") {
                return Err(FrontendErrorKind::Unsupported);
            }
            let mut definition = own_shape(name)?;
            definition.column_type = MYSQL_TYPE_VAR_STRING;
            definition
        }
        // Measured on MySQL 8.4.11: `YEAR(a)` answers a YEAR of length 4 with
        // the unsigned, binary and numeric flags — and no zerofill, which a
        // YEAR column does carry. `MONTH(a)` and `DAY(a)` each answer a
        // LONGLONG of length 3. All three are nullable even over a NOT NULL
        // column.
        ScalarFunction::ReadsTheYear => {
            let mut definition = column_definition(name, MYSQL_TYPE_YEAR);
            definition.column_length = 4;
            definition
        }
        // Measured: MONTH, DAY, MINUTE and SECOND each answer a LONGLONG of
        // length 3, HOUR one of length 4 — its span runs past a day — and
        // DATEDIFF one of length 9.
        ScalarFunction::ReadsAMonthOrDay | ScalarFunction::ReadsAMinuteOrSecond => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 3;
            definition
        }
        ScalarFunction::ReadsTheHour => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 4;
            definition
        }
        // Measured on MySQL 8.4.11: `QUARTER(d)` answers 1 to 4 as a LONGLONG
        // of length 2, and so do `WEEKDAY(d)`, counting the week from Monday
        // as 0, and `DAYOFWEEK(d)`, counting it from Sunday as 1.
        ScalarFunction::ReadsTheQuarter | ScalarFunction::ReadsADayOfTheWeek => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 2;
            definition
        }
        // Measured: `DAYOFYEAR(d)` answers 1 to 366 as a LONGLONG of length 4.
        ScalarFunction::ReadsTheDayOfTheYear => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 4;
            definition
        }
        // Measured: `EXTRACT(YEAR FROM d)` answers a LONGLONG of length 5,
        // where `YEAR(d)` answers a YEAR of length 4 — the one part of an
        // EXTRACT whose shape differs from its call spelling.
        ScalarFunction::ReadsTheYearAsANumber => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 5;
            definition
        }
        // Measured: `LAST_DAY(d)` answers a DATE of length 10.
        ScalarFunction::ReadsTheLastDay => {
            let mut definition = column_definition(name, MYSQL_TYPE_DATE);
            definition.column_length = 10;
            definition
        }
        ScalarFunction::CountsDaysBetween | ScalarFunction::CountsUnitsBetween => {
            unreachable!("the counts between two moments were answered above")
        }
        ScalarFunction::NamesTheDayOrMonth
        | ScalarFunction::ReadsTheWeek
        | ScalarFunction::ReadsTheYearAndWeek
        | ScalarFunction::CountsDaysFromTheYearZero => {
            unreachable!("the calendar names and the week were answered above")
        }
        ScalarFunction::ShiftsByWholeDays | ScalarFunction::ShiftsByTime => {
            unreachable!("the shifts were answered above")
        }
        ScalarFunction::ShiftsTheMoment
        | ScalarFunction::ShiftsTheDay
        | ScalarFunction::ShiftsAWrittenMoment => {
            unreachable!("the shifts of a clock reading or a written moment were answered above")
        }
        ScalarFunction::CastsToText
        | ScalarFunction::CastsToWholeNumber
        | ScalarFunction::CastsToDay
        | ScalarFunction::CastsToMoment => unreachable!("the casts were answered above"),
        ScalarFunction::Now => unreachable!("NOW was answered above"),
        ScalarFunction::Today => unreachable!("CURDATE was answered above"),
        ScalarFunction::TimeOfDay => unreachable!("CURTIME was answered above"),
        ScalarFunction::NowToAFraction { .. } | ScalarFunction::TimeOfDayToAFraction { .. } => {
            unreachable!("a clock reading to a fraction was answered above")
        }
        ScalarFunction::RanksRows
        | ScalarFunction::RanksFraction
        | ScalarFunction::ShiftsRow
        | ScalarFunction::ShiftsRowOrNumber
        | ScalarFunction::ShiftsRowOrWord => {
            unreachable!("the window calls were answered above")
        }
        ScalarFunction::Compares | ScalarFunction::NegatesTruth | ScalarFunction::TestsTruth => {
            unreachable!("the truth answers were answered above")
        }
        ScalarFunction::Concatenates
        | ScalarFunction::TakesCharacters
        | ScalarFunction::TakesASubstring { .. }
        | ScalarFunction::SplitsOnADelimiter
        | ScalarFunction::Branches
        | ScalarFunction::Repeats
        | ScalarFunction::Hexadecimal
        | ScalarFunction::Widest
        | ScalarFunction::NullsOnMatch => {
            unreachable!("the text-width calls were answered above")
        }
    };
    // The answer belongs to no table, and is null wherever its column is.
    definition.schema.clear();
    definition.table.clear();
    definition.original_table.clear();
    definition.original_name.clear();
    // Measured: `BIN`, `OCT` and `ELT` report no flags at all, answering text
    // with the ordinary collation rather than bytes.
    let binary = if (wants_text && function == ScalarFunction::KeepsTextShape)
        || matches!(
            function,
            ScalarFunction::WritesInAnotherRadix | ScalarFunction::ReadsThePlace
        ) {
        0
    } else {
        MYSQL_BINARY_FLAG
    };
    // Measured on MySQL 8.4.11: a count or a place read out of a NOT NULL
    // column cannot be null either, and neither can a sign or a rounding —
    // unless the column is on the outer side of a join, which can answer a
    // row without it.
    let not_null = not_null
        || (!source.nullable()
            && !table.outer
            && matches!(
                function,
                ScalarFunction::KeepsNumericShape
                    | ScalarFunction::Truncates
                    | ScalarFunction::RoundsToPlaces { .. }
                    | ScalarFunction::RoundsToWhole
                    | ScalarFunction::Negates
                    | ScalarFunction::CountsText
                    | ScalarFunction::Locates
                    | ScalarFunction::ReadsTheFirstByte
                    | ScalarFunction::ReadsTheFirstCharacter
                    | ScalarFunction::ChecksTheBytes
            ));
    set_column_flags(
        &mut definition,
        binary | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
    );
    if matches!(
        function,
        ScalarFunction::ReadsTheYear | ScalarFunction::ChecksTheBytes
    ) {
        // Measured: a year reading is unsigned where the column it reads is
        // unsigned and zerofilled, and a checksum is never negative, so the
        // sign is put back after the flags every call shares.
        definition.flags |= MYSQL_UNSIGNED_FLAG;
    }
    Ok(definition)
}

/// Builds the column `ROUND`, or with no places `FLOOR` and `CEIL`, reports.
///
/// Measured on MySQL 8.4.11: over a whole number a LONGLONG of 21 whatever the
/// places; over a DOUBLE a DOUBLE of 23; over a DECIMAL a DECIMAL whose scale
/// is the places held to the column's own, with one more whole digit when a
/// place is cut away for the carry — `DECIMAL(10,3)` rounded to 1 reports 11
/// with a scale of 1, to none 9, and to 5 the column's own 12 and 3. A `BIGINT`
/// rounded left of the point, which MySQL answers 1690 for past the largest
/// one, a DECIMAL rounded left of the point, `FLOOR` and `CEIL` over a DECIMAL
/// too wide for a LONGLONG, and every other type are refused.
#[cfg(unix)]
fn rounded_definition(
    name: String,
    source: &MySqlColumnMetadata,
    places: Option<i32>,
) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
    if is_signed_whole_number_column(source.type_name()) {
        if source.type_name() == "BIGINT" && places.is_some_and(|places| places < 0) {
            return Err(FrontendErrorKind::Unsupported);
        }
        let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
        definition.column_length = 21;
        return Ok(definition);
    }
    if source.type_name() == "DOUBLE" {
        let mut definition = column_definition(name, MYSQL_TYPE_DOUBLE);
        definition.column_length = 23;
        definition.decimals = NOT_FIXED_DECIMALS;
        return Ok(definition);
    }
    // Measured on MySQL 8.4.11: `FLOOR` and `CEIL` over a DECIMAL answer a
    // LONGLONG of 21 while its whole digits, and one more where it has a
    // fraction, number no more than eighteen — `DECIMAL(18,0)` and
    // `DECIMAL(10,2)` do, `DECIMAL(19,1)` does not — and a NEWDECIMAL past
    // that, which is not taken. An unsigned one answers a signed LONGLONG.
    if let (None, "DECIMAL" | "DECIMAL UNSIGNED", Some((precision, scale))) =
        (places, source.type_name(), source.decimal_size())
    {
        if precision - scale + u32::from(scale > 0) > 18 {
            return Err(FrontendErrorKind::Unsupported);
        }
        let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
        definition.column_length = 21;
        return Ok(definition);
    }
    let (Some(places), "DECIMAL", Some((precision, scale))) =
        (places, source.type_name(), source.decimal_size())
    else {
        return Err(FrontendErrorKind::Unsupported);
    };
    let kept = u32::try_from(places)
        .map_err(|_| FrontendErrorKind::Unsupported)?
        .min(scale);
    let cut = scale - kept;
    let precision = if cut > 0 {
        precision - cut + 1
    } else {
        precision
    };
    let mut definition = column_definition(name, MYSQL_TYPE_NEWDECIMAL);
    definition.column_length = precision + 1 + u32::from(kept > 0);
    definition.decimals = u8::try_from(kept).map_err(|_| FrontendErrorKind::Internal)?;
    Ok(definition)
}

/// Reports whether a column counts in whole numbers that carry a sign.
#[cfg(unix)]
fn is_signed_whole_number_column(type_name: &str) -> bool {
    matches!(
        type_name,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT" | "BOOLEAN"
    )
}

/// Builds the column `UNIX_TIMESTAMP` or `FROM_UNIXTIME` reports.
///
/// Measured on MySQL 8.4.11: the count is a LONGLONG of 21 with the binary and
/// numeric flags, NOT NULL only when there is nothing to read that could be
/// null, and the moment a DATETIME of 19 with the binary flag alone.
#[cfg(unix)]
fn epoch_call_definition(
    name: String,
    function: ScalarFunction,
    not_null: bool,
) -> ColumnDefinitionConfig {
    if function == ScalarFunction::ReadsFromEpoch {
        let mut definition = column_definition(name, MYSQL_TYPE_DATETIME);
        definition.column_length = 19;
        set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
        return definition;
    }
    let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
    definition.column_length = 21;
    definition.decimals = 0;
    set_column_flags(
        &mut definition,
        MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
    );
    definition
}

/// Measured on MySQL 8.4.11: `DAYNAME` and `MONTHNAME` report a VAR_STRING of
/// 36 in utf8mb4 with the not-fixed decimals value — nine characters, the
/// longest name — and `WEEK` a LONGLONG of 3, each nullable whatever it reads.
#[cfg(unix)]
fn calendar_name_or_week_definition(
    name: String,
    function: ScalarFunction,
) -> ColumnDefinitionConfig {
    if function == ScalarFunction::NamesTheDayOrMonth {
        let mut definition = text_call_definition(name, 36, false);
        definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        return definition;
    }
    // Measured on MySQL 8.4.11: `WEEK` reports 3, `YEARWEEK` 7 and `TO_DAYS`
    // 8, each nullable even over a NOT NULL column.
    let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
    definition.column_length = match function {
        ScalarFunction::ReadsTheYearAndWeek => 7,
        ScalarFunction::CountsDaysFromTheYearZero => 8,
        _ => 3,
    };
    definition.decimals = 0;
    set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
    definition
}

/// Measured on MySQL 8.4.11: a `DATEDIFF` reports a LONGLONG of length 9 and
/// a `TIMESTAMPDIFF` one of 21, whichever unit it counts, and both are
/// nullable whatever they count between.
#[cfg(unix)]
fn counted_between_definition(name: String, function: ScalarFunction) -> ColumnDefinitionConfig {
    let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
    definition.column_length = if function == ScalarFunction::CountsDaysBetween {
        9
    } else {
        21
    };
    definition.decimals = 0;
    set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
    definition
}

/// The characters a `TEXT` holds at most, which MySQL counts a `TEXT` in when
/// a call works out how wide its answer can be.
#[cfg(unix)]
const MYSQL_TEXT_CHARACTERS: u32 = 65535;

/// The column a text call reports when its answer outgrows a VAR_STRING:
/// measured on MySQL 8.4.11, a MEDIUM_BLOB with the text collation and no
/// flags, even over a NOT NULL column. Wider than a MEDIUM_BLOB holds is a
/// LONG_BLOB, which has not been measured.
#[cfg(unix)]
fn medium_blob_text_definition(
    name: String,
    width: u32,
) -> Result<ColumnDefinitionConfig, FrontendErrorKind> {
    if width > MYSQL_MEDIUM_BLOB_LENGTH {
        return Err(FrontendErrorKind::Unsupported);
    }
    let mut definition = column_definition(name, MYSQL_TYPE_MEDIUM_BLOB);
    definition.column_length = width;
    definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    definition.decimals = NOT_FIXED_DECIMALS;
    set_column_flags(&mut definition, 0);
    Ok(definition)
}

/// The most bytes a MEDIUM_BLOB holds.
#[cfg(unix)]
const MYSQL_MEDIUM_BLOB_LENGTH: u32 = 16_777_215;

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

/// The characters `SUBSTRING` can answer out of a column of `length` of them.
#[cfg(unix)]
fn substring_characters(length: u32, from: i32, count: Option<i32>) -> u32 {
    let left = if from < 0 {
        let back = from.unsigned_abs();
        if back > length {
            0
        } else {
            back
        }
    } else {
        length.saturating_sub(from.unsigned_abs().wrapping_sub(1).min(length))
    };
    match count {
        Some(count) if count <= 0 => 0,
        Some(count) => left.min(count.unsigned_abs()),
        None => left,
    }
}

/// The result column a `STR_TO_DATE` reports, which its format names.
///
/// Measured on MySQL 8.4.11: a DATE of 10, a TIME of 10 or a DATETIME of 19,
/// each with the binary collation and flag, as a stored column of that type
/// reports.
#[cfg(unix)]
fn read_moment_definition(name: String, function: ScalarFunction) -> ColumnDefinitionConfig {
    let (column_type, length) = match function {
        ScalarFunction::ReadsADay => (MYSQL_TYPE_DATE, 10),
        ScalarFunction::ReadsAClock => (MYSQL_TYPE_TIME, 10),
        _ => (MYSQL_TYPE_DATETIME, 19),
    };
    let mut definition = column_definition(name, column_type);
    definition.column_length = length;
    definition.character_set = MYSQL_BINARY_COLLATION;
    definition.decimals = 0;
    set_column_flags(&mut definition, MYSQL_BINARY_FLAG);
    definition
}

/// The result column a `DATE_FORMAT` reports.
///
/// Measured on MySQL 8.4.11: a VAR_STRING as wide as the format could make it,
/// with the text collation and no flags at all — not even the binary one every
/// other reading of a moment carries.
#[cfg(unix)]
fn written_moment_definition(name: String, literal_characters: u32) -> ColumnDefinitionConfig {
    let mut definition = column_definition(name, MYSQL_TYPE_VAR_STRING);
    definition.column_length = literal_characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
    definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    definition.decimals = NOT_FIXED_DECIMALS;
    set_column_flags(&mut definition, 0);
    definition
}

/// Reports whether a column's value goes into a built JSON document the way
/// MySQL writes it there.
///
/// Measured on MySQL 8.4.11: words and `ENUM` members as strings, whole
/// numbers, a `DOUBLE`, a `YEAR` and a `DECIMAL` with no places as numbers, a
/// `DATE` and a `DATETIME` as strings, and a `JSON` column as the document it
/// holds. The rest are refused: a `TIME`, a `TIMESTAMP` — written in the
/// session's zone — and a `DECIMAL` with places each by a rule the rendering
/// does not follow, and a `FLOAT`, a `SET`, a `BIT` and binary strings have not
/// been measured.
#[cfg(unix)]
fn writes_into_a_document_the_way_mysql_does(source: &MySqlColumnMetadata) -> bool {
    is_text_column(source)
        || is_whole_number_column(source.type_name())
        || turso_mysql_parser::enum_members(source.type_name()).is_some()
        || matches!(
            source.type_name(),
            "DOUBLE" | "YEAR" | "DATE" | "DATETIME" | "JSON"
        )
        || source.decimal_size().is_some_and(|(_, scale)| scale == 0)
}

/// Reports whether MySQL writes a column's values into a `GROUP_CONCAT` the
/// way the engine does.
///
/// Measured on MySQL 8.4.11: a `DOUBLE` or a `FLOAT` is written the shortest
/// way — `1e20`, `0.1` — where the engine writes `1.0e+20` and
/// `0.100000001490116`, and a `BLOB`, a binary string or a `JSON` column
/// answers a binary result of a width of its own.
#[cfg(unix)]
fn joins_as_the_engine_writes_it(column: &MySqlColumnMetadata) -> bool {
    is_text_column(column)
        || is_whole_number_column(column.type_name())
        || column.decimal_size().is_some()
        || matches!(
            column.type_name(),
            "DATE" | "DATETIME" | "TIMESTAMP" | "TIME" | "YEAR" | "ENUM"
        )
}

#[cfg(unix)]
fn is_text_column(column: &MySqlColumnMetadata) -> bool {
    matches!(
        column.type_name(),
        "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT"
    )
}

/// Reports whether a static projection is one of the checked window calls.
#[cfg(unix)]
fn is_window_call(metadata: &turso_mysql_parser::StaticSelectMetadata) -> bool {
    matches!(
        metadata,
        turso_mysql_parser::StaticSelectMetadata::WindowAggregate { .. }
            | turso_mysql_parser::StaticSelectMetadata::WindowCount
            | turso_mysql_parser::StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::RanksRows
                    | ScalarFunction::RanksFraction
                    | ScalarFunction::ShiftsRow
                    | ScalarFunction::ShiftsRowOrNumber
                    | ScalarFunction::ShiftsRowOrWord,
                ..
            }
    )
}

/// Reports whether a static projection has to read the source table's columns.
#[cfg(unix)]
fn needs_source_columns(metadata: &turso_mysql_parser::StaticSelectMetadata) -> bool {
    match metadata {
        turso_mysql_parser::StaticSelectMetadata::ColumnAggregate { .. }
        | turso_mysql_parser::StaticSelectMetadata::RoundedAggregate { .. }
        | turso_mysql_parser::StaticSelectMetadata::RolledUpKey { .. }
        | turso_mysql_parser::StaticSelectMetadata::WindowAggregate { .. } => true,
        turso_mysql_parser::StaticSelectMetadata::FromARollup(inner) => needs_source_columns(inner),
        turso_mysql_parser::StaticSelectMetadata::ScalarSubquery(inner)
        | turso_mysql_parser::StaticSelectMetadata::DefaultedAggregate(inner)
        | turso_mysql_parser::StaticSelectMetadata::FromTheGroupingTable {
            answer: inner, ..
        } => needs_source_columns(inner),
        turso_mysql_parser::StaticSelectMetadata::Arithmetic(shape) => shape.names_a_column(),
        turso_mysql_parser::StaticSelectMetadata::Branches { branches, .. } => branches
            .iter()
            .any(|branch| matches!(branch, Branch::Column { .. })),
        turso_mysql_parser::StaticSelectMetadata::AggregateOverBranches { branches, .. } => {
            needs_source_columns(branches)
        }
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
        // Measured on MySQL 8.4.11: a windowed aggregate answers the shape its
        // plain form does, apart from the binary flag, which it does not
        // carry, and MIN and MAX, which widen an INT to LONGLONG where the
        // plain form leaves it LONG.
        turso_mysql_parser::StaticSelectMetadata::WindowAggregate { column_name, kind } => {
            let mut definition = source_metadata
                .ok_or(FrontendErrorKind::Unsupported)?
                .aggregate_column_definition(name, column_name, *kind)?;
            if *kind == ColumnAggregateKind::MinMax
                && matches!(
                    definition.column_type,
                    MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
                )
            {
                definition.column_type = MYSQL_TYPE_LONGLONG;
            }
            let flags = definition.flags & !MYSQL_BINARY_FLAG;
            set_column_flags(&mut definition, flags);
            Ok(definition)
        }
        // Measured on MySQL 8.4.11: `IFNULL(SUM(n), 0)` answers the shape
        // `SUM(n)` answers on its own — its length, its scale and its
        // character set — and is never null, which is why it is written. A
        // whole number widens to a BIGINT there: `IFNULL(MAX(s), 0)` over a
        // SMALLINT answers LONGLONG while keeping the SMALLINT's length 6,
        // which is what `IFNULL` over a plain column does too.
        turso_mysql_parser::StaticSelectMetadata::DefaultedAggregate(inner) => {
            let mut definition = match inner.as_ref() {
                turso_mysql_parser::StaticSelectMetadata::Count => {
                    static_column_definition(name, inner).ok_or(FrontendErrorKind::Internal)?
                }
                inner => aggregate_column_definition(source_metadata, name, inner)?,
            };
            if matches!(
                definition.column_type,
                MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
            ) {
                definition.column_type = MYSQL_TYPE_LONGLONG;
            }
            let flags = definition.flags | MYSQL_NOT_NULL_FLAG;
            set_column_flags(&mut definition, flags);
            Ok(definition)
        }
        // Measured on MySQL 8.4.11: a scalar subquery answers the shape its
        // aggregate answers on its own, and is nullable whatever that
        // aggregate is — where a plain COUNT is NOT NULL.
        turso_mysql_parser::StaticSelectMetadata::ScalarSubquery(inner) => {
            let mut definition = match inner.as_ref() {
                turso_mysql_parser::StaticSelectMetadata::Count => {
                    static_column_definition(name, inner).ok_or(FrontendErrorKind::Internal)?
                }
                inner => aggregate_column_definition(source_metadata, name, inner)?,
            };
            let flags = definition.flags & !MYSQL_NOT_NULL_FLAG;
            set_column_flags(&mut definition, flags);
            Ok(definition)
        }
        turso_mysql_parser::StaticSelectMetadata::RoundedAggregate {
            column_name,
            kind,
            places,
        } => source_metadata
            .ok_or(FrontendErrorKind::Unsupported)?
            .rounded_aggregate_column_definition(name, column_name, *kind, *places),
        // Measured on MySQL 8.4.11: a column of a `WITH RECURSIVE` sequence
        // counting through whole numbers is a nullable LONGLONG as wide as its
        // first value's digits and one more, with no flags, naming the
        // sequence and itself and no database or original table.
        turso_mysql_parser::StaticSelectMetadata::CountedColumn {
            table,
            column,
            length,
        } => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = *length;
            definition.decimals = 0;
            definition.table.clone_from(table);
            definition.original_name.clone_from(column);
            set_column_flags(&mut definition, 0);
            Ok(definition)
        }
        turso_mysql_parser::StaticSelectMetadata::RolledUpKey { column_name } => source_metadata
            .ok_or(FrontendErrorKind::Unsupported)?
            .rolled_up_key_definition(name, column_name),
        // Measured on MySQL 8.4.11: an aggregate of a statement grouping `WITH
        // ROLLUP` answers the shape it answers without one, apart from a
        // largest or smallest moment, which answers words of 76 there.
        turso_mysql_parser::StaticSelectMetadata::FromARollup(answer) => {
            let mut definition = match static_column_definition(name.clone(), answer) {
                Some(definition) => definition,
                None => aggregate_column_definition(source_metadata, name, answer)?,
            };
            if matches!(
                definition.column_type,
                MYSQL_TYPE_DATE
                    | MYSQL_TYPE_DATETIME
                    | MYSQL_TYPE_TIMESTAMP
                    | MYSQL_TYPE_TIME
                    | MYSQL_TYPE_YEAR
            ) {
                return Err(FrontendErrorKind::Unsupported);
            }
            // Words carry the 31 decimals a call's words do.
            if matches!(
                definition.column_type,
                MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING
            ) {
                definition.decimals = NOT_FIXED_DECIMALS;
            }
            Ok(definition)
        }
        turso_mysql_parser::StaticSelectMetadata::FromTheGroupingTable { answer, key } => {
            let mut definition = match static_column_definition(name.clone(), answer) {
                Some(definition) => definition,
                None => aggregate_column_definition(source_metadata, name, answer)?,
            };
            read_out_of_the_grouping_table(&mut definition, answer)?;
            if *key {
                definition.flags |= MYSQL_GROUP_FLAG;
            }
            Ok(definition)
        }
        // Measured: the same shape a plain COUNT answers, without the binary
        // flag.
        turso_mysql_parser::StaticSelectMetadata::WindowCount => {
            let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
            definition.column_length = 21;
            definition.decimals = 0;
            set_column_flags(&mut definition, MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG);
            Ok(definition)
        }
        // `SELECT 1+1` reads no table at all, so this one may have none.
        turso_mysql_parser::StaticSelectMetadata::Arithmetic(shape) => {
            TableResultMetadata::arithmetic_column_definition(source_metadata, name, shape)
        }
        turso_mysql_parser::StaticSelectMetadata::Branches {
            branches,
            may_be_null,
            falls_back,
        } => TableResultMetadata::branches_column_definition(
            source_metadata,
            name,
            branches,
            *may_be_null,
            *falls_back,
        ),
        turso_mysql_parser::StaticSelectMetadata::AggregateOverBranches { kind, branches } => {
            TableResultMetadata::aggregate_over_branches_definition(
                source_metadata,
                name,
                *kind,
                branches,
            )
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

/// Gives an answer a derived table's body worked out the shape MySQL reports
/// for it once the body has been written out into a table of its own.
///
/// Measured on MySQL 8.4.11: every number that table stores loses the binary
/// flag — a count, a total, a largest, and an average too, which a grouping
/// table works out afterwards and this one stores — and a count stays NOT
/// NULL. Words lose the 31 decimals a call's words carry. A largest or
/// smallest moment and a day keep the binary flag a moment carries. Any other
/// answer has not been measured there and is refused.
#[cfg(unix)]
fn stored_in_a_derived_table(
    definition: &mut ColumnDefinitionConfig,
    answer: &turso_mysql_parser::StaticSelectMetadata,
) -> Result<(), FrontendErrorKind> {
    use turso_mysql_parser::StaticSelectMetadata;
    if !matches!(
        answer,
        StaticSelectMetadata::Count
            | StaticSelectMetadata::ColumnAggregate {
                kind: ColumnAggregateKind::MinMax
                    | ColumnAggregateKind::Sum
                    | ColumnAggregateKind::Avg,
                ..
            }
            | StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::CastsToDay,
                ..
            }
    ) {
        return Err(FrontendErrorKind::Unsupported);
    }
    match definition.column_type {
        MYSQL_TYPE_TINY
        | MYSQL_TYPE_SHORT
        | MYSQL_TYPE_INT24
        | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_NEWDECIMAL => {
            let flags = definition.flags & !MYSQL_BINARY_FLAG;
            set_column_flags(definition, flags);
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => definition.decimals = 0,
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => {
            let flags = definition.flags | MYSQL_BINARY_FLAG;
            set_column_flags(definition, flags);
        }
        _ => return Err(FrontendErrorKind::Unsupported),
    }
    Ok(())
}

/// Gives a column of a derived table's or a CTE's table the shape MySQL
/// reports for it through the derived table.
///
/// Measured on MySQL 8.4.11. The column goes by the name the body gave it and
/// names the table the body read under the name the body read it under — its
/// alias, when it gave one. A body that aggregates is written out into a table
/// of its own, which keeps a column's NOT NULL, default and sign but none of
/// its keys and no auto-increment. A body read straight through keeps every
/// flag, and reports a day, a moment and a time of day in the connection's
/// character set, four bytes to each character it spells — a `DATETIME` 76
/// where on its own it reports 19 — and a JSON column in it too, at the widest
/// length a document has in words.
#[cfg(unix)]
fn read_through_a_derived_table(
    definition: &mut ColumnDefinitionConfig,
    derived: &MySqlDerivedColumns,
    ordinal: usize,
) {
    if let Some(name) = derived.names().get(ordinal) {
        definition.original_name.clone_from(name);
    }
    derived
        .inner_reference()
        .clone_into(&mut definition.original_table);
    if derived.materialized() {
        definition.flags &= !(MYSQL_PRI_KEY_FLAG
            | MYSQL_UNIQUE_KEY_FLAG
            | MYSQL_MULTIPLE_KEY_FLAG
            | MYSQL_PART_KEY_FLAG
            | MYSQL_AUTO_INCREMENT_FLAG);
        return;
    }
    match definition.column_type {
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIME => {
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            definition.column_length = definition
                .column_length
                .saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER);
        }
        MYSQL_TYPE_JSON => {
            definition.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            definition.column_length = u32::MAX - 3;
        }
        _ => {}
    }
}

/// Gives an answer the shape MySQL reports for it once it has been read out of
/// the temporary table a statement grouping by an expression groups in.
///
/// Measured on MySQL 8.4.11. That table stores each grouping key, each call
/// over the grouped columns, and each `COUNT`, `SUM`, `MIN` and `MAX`, and a
/// stored answer reports the table's column. A whole number is stored as a
/// `LONG` when it is eleven characters or fewer, so `YEAR(created_at)` answers
/// a `LONG` of 4 where on its own it answers a `YEAR`, and `MONTH` a `LONG` of
/// 3 where on its own it answers a `LONGLONG`. A stored number loses the
/// binary flag, a `COUNT` included. Words lose their 31 decimals, which is
/// what MySQL writes for a call's words on their own. A day keeps its shape.
/// An `AVG` is worked out after the grouping from a sum and a count and keeps
/// the shape it has on its own.
///
/// Anything else has not been measured there and is refused.
#[cfg(unix)]
fn read_out_of_the_grouping_table(
    definition: &mut ColumnDefinitionConfig,
    answer: &turso_mysql_parser::StaticSelectMetadata,
) -> Result<(), FrontendErrorKind> {
    use turso_mysql_parser::StaticSelectMetadata;
    let stored = match answer {
        StaticSelectMetadata::Count => true,
        // Measured: a written word or whole number is not stored, and keeps
        // the shape it has on its own.
        StaticSelectMetadata::RoundedAggregate { .. }
        | StaticSelectMetadata::Integer { .. }
        | StaticSelectMetadata::WrittenValue(turso_mysql_parser::WrittenValue::Word { .. }) => {
            false
        }
        StaticSelectMetadata::ColumnAggregate { kind, .. } => match kind {
            ColumnAggregateKind::MinMax | ColumnAggregateKind::Sum => true,
            ColumnAggregateKind::Avg => false,
            ColumnAggregateKind::Concatenated
            | ColumnAggregateKind::DeviatesBySample
            | ColumnAggregateKind::CollectsIntoJson => return Err(FrontendErrorKind::Unsupported),
        },
        StaticSelectMetadata::ScalarCall {
            function:
                ScalarFunction::CastsToDay
                | ScalarFunction::ReadsTheYear
                | ScalarFunction::ReadsAMonthOrDay
                | ScalarFunction::ReadsTheHour
                | ScalarFunction::ReadsAMinuteOrSecond
                | ScalarFunction::ReadsTheQuarter
                | ScalarFunction::ReadsADayOfTheWeek
                | ScalarFunction::ReadsTheDayOfTheYear
                | ScalarFunction::ReadsTheLastDay
                | ScalarFunction::ReadsTheYearAsANumber
                | ScalarFunction::NamesTheDayOrMonth
                | ScalarFunction::WritesAMoment
                | ScalarFunction::KeepsTextShape,
            ..
        } => true,
        _ => return Err(FrontendErrorKind::Unsupported),
    };
    if !stored {
        return Ok(());
    }
    match definition.column_type {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
            if definition.column_length <= 11 {
                definition.column_type = MYSQL_TYPE_LONG;
            }
            let flags = definition.flags & (MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG);
            set_column_flags(definition, flags);
        }
        MYSQL_TYPE_NEWDECIMAL => {
            let flags = definition.flags & !MYSQL_BINARY_FLAG;
            set_column_flags(definition, flags);
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => definition.decimals = 0,
        MYSQL_TYPE_DATE => {}
        _ => return Err(FrontendErrorKind::Unsupported),
    }
    Ok(())
}

/// The columns one `information_schema` table reports.
///
/// These are the shapes MySQL reports for them, which the catalog result
/// builder already holds against a pinned 8.4.11 golden.
#[cfg(unix)]
fn catalog_table_columns(catalog: MySqlCatalogTable) -> Vec<ColumnDefinitionConfig> {
    match catalog {
        MySqlCatalogTable::Tables => catalog_results::information_schema_tables_columns(&[
            MySqlInformationSchemaTablesColumn::TableSchema,
            MySqlInformationSchemaTablesColumn::TableName,
            MySqlInformationSchemaTablesColumn::TableType,
            MySqlInformationSchemaTablesColumn::Engine,
            MySqlInformationSchemaTablesColumn::DataLength,
            MySqlInformationSchemaTablesColumn::IndexLength,
            MySqlInformationSchemaTablesColumn::TableCollation,
            MySqlInformationSchemaTablesColumn::TableComment,
        ]),
        MySqlCatalogTable::Views => catalog_results::information_schema_views_columns(),
        MySqlCatalogTable::Statistics => catalog_results::information_schema_statistics_columns(),
        MySqlCatalogTable::KeyColumnUsage => {
            catalog_results::information_schema_key_column_usage_columns()
        }
        MySqlCatalogTable::TableConstraints => {
            catalog_results::information_schema_table_constraints_columns()
        }
        MySqlCatalogTable::ReferentialConstraints => {
            catalog_results::information_schema_referential_constraints_columns()
        }
        MySqlCatalogTable::Routines => catalog_results::information_schema_routines_columns(),
        MySqlCatalogTable::Columns => catalog_results::information_schema_columns_every_column(),
        MySqlCatalogTable::Schemata => catalog_results::information_schema_schemata_columns(),
        MySqlCatalogTable::CheckConstraints => {
            catalog_results::information_schema_check_constraints_columns()
        }
    }
}

#[cfg(unix)]
fn prepared_table_result_metadata(
    connection: &MySqlConnection,
    type_metadata: &[MySqlPreparedResultColumnTypeMetadata],
    selected_database: Option<&str>,
    source_tables: &[MySqlSelectSource],
) -> Result<Option<TableResultMetadata>, FrontendErrorKind> {
    let windowed = type_metadata
        .iter()
        .filter_map(MySqlPreparedResultColumnTypeMetadata::static_metadata)
        .any(is_window_call);
    let needs_source_columns = windowed
        || source_tables.iter().any(|source| source.branch() > 0)
        || type_metadata
            .iter()
            .filter_map(MySqlPreparedResultColumnTypeMetadata::static_metadata)
            .any(needs_source_columns);
    let source_references = type_metadata
        .iter()
        .filter(|metadata| !windowed && metadata.static_metadata().is_none())
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
        // An `information_schema` table is not in the catalog listing: the
        // engine scans it, and its columns report the shapes MySQL reports
        // for them.
        if let Some(catalog) = source.catalog() {
            tables.push(SourceTableColumns {
                source_table: source.table().as_str().to_owned(),
                table_reference: source.reference().to_owned(),
                branch: source.branch(),
                subquery: source.subquery(),
                columns: Vec::new(),
                catalog_columns: catalog_table_columns(catalog),
                view_columns: Vec::new(),
                outer: source.outer(),
                projected_columns: source.projected_columns().to_vec(),
                derived: source.derived().cloned(),
            });
            continue;
        }
        let table_kind = listed
            .iter()
            .find(|table| table.name().eq_ignore_ascii_case(source.table().as_str()))
            .map(|table| table.kind())
            .ok_or(FrontendErrorKind::MissingObject)?;
        let mut view_columns = Vec::new();
        let columns = if table_kind == MySqlTableKind::BaseTable {
            connection
                .list_columns(source.table())
                .map_err(column_metadata_error_kind)?
        } else if let Some(grouped) =
            grouped_view_columns(connection, selected_database, source.table())?
        {
            view_columns = grouped;
            Vec::new()
        } else {
            // A view projecting one table's columns reports each the way the
            // table does, under the view's own name. Any other view stays on
            // the generic path, its wire fields not measured.
            match connection.columns_a_view_reads(source.table()) {
                Ok(columns) => columns,
                Err(_) => return Ok(None),
            }
        };
        tables.push(SourceTableColumns {
            source_table: source.table().as_str().to_owned(),
            table_reference: source.reference().to_owned(),
            branch: source.branch(),
            subquery: source.subquery(),
            columns,
            catalog_columns: Vec::new(),
            view_columns,
            outer: source.outer(),
            projected_columns: source.projected_columns().to_vec(),
            derived: source.derived().cloned(),
        });
    }
    let metadata = TableResultMetadata {
        database: selected_database.to_owned(),
        tables,
        union: source_tables.iter().any(|source| source.branch() > 0),
        group_concat_max_len: connection.group_concat_max_len(),
    };
    for (reference, ordinal) in source_references {
        let Some(table) = metadata.table_for(reference) else {
            return Err(FrontendErrorKind::Unsupported);
        };
        if table.answer(*ordinal).is_none() {
            table.column_ordinal(*ordinal)?;
        }
    }
    Ok(Some(metadata))
}

/// Works out the columns of a view grouping its rows, or nothing for any
/// other view.
///
/// Measured on MySQL 8.4.11, such a view is read out of a table MySQL gathers
/// the groups into, and each column reports that table's shape: a grouped
/// column keeps its table as its original table and its type, length and
/// nullability, and loses its key flags — a primary-key `id` reports
/// `NOT_NULL UNSIGNED` — and an aggregate names the view as its original
/// table. A count is a NOT NULL `LONGLONG` of 21 without the binary flag a
/// `COUNT` read from a table carries, and the least or greatest of a column is
/// that column's shape, nullable and without its keys.
#[cfg(unix)]
fn grouped_view_columns(
    connection: &MySqlConnection,
    database: &str,
    view: &MySqlTableName,
) -> Result<Option<Vec<ColumnDefinitionConfig>>, FrontendErrorKind> {
    use turso_mysql_parser::MySqlViewColumnReading;

    let Some((written, columns)) = connection
        .grouped_view_readings(view)
        .map_err(column_metadata_error_kind)?
    else {
        return Ok(None);
    };
    let base = written.table().as_str().to_owned();
    let base_metadata = TableResultMetadata {
        database: database.to_owned(),
        tables: vec![SourceTableColumns {
            source_table: base.clone(),
            table_reference: base,
            branch: 0,
            subquery: false,
            columns,
            catalog_columns: Vec::new(),
            view_columns: Vec::new(),
            projected_columns: Vec::new(),
            outer: false,
            derived: None,
        }],
        union: false,
        group_concat_max_len: connection.group_concat_max_len(),
    };
    let key_flags = MYSQL_PRI_KEY_FLAG
        | MYSQL_UNIQUE_KEY_FLAG
        | MYSQL_PART_KEY_FLAG
        | MYSQL_AUTO_INCREMENT_FLAG;
    let base_column = |name: &str, column: &str| {
        let (table, ordinal) = base_metadata.column_named(column)?;
        base_metadata.column_definition_for_reference(
            Some((table.table_reference.clone(), ordinal)),
            name.to_owned(),
            None,
        )
    };
    written
        .columns()
        .iter()
        .map(|(name, reading)| {
            let mut definition = match reading {
                MySqlViewColumnReading::Column(column) => {
                    let mut definition = base_column(name, column)?;
                    definition.flags &= !key_flags;
                    return Ok(definition);
                }
                MySqlViewColumnReading::Count => {
                    let mut definition = column_definition(name.clone(), MYSQL_TYPE_LONGLONG);
                    definition.column_length = 21;
                    set_column_flags(&mut definition, MYSQL_NOT_NULL_FLAG);
                    definition
                }
                MySqlViewColumnReading::Least(column)
                | MySqlViewColumnReading::Greatest(column) => {
                    let mut definition = base_column(name, column)?;
                    definition.flags &= !(key_flags | MYSQL_NOT_NULL_FLAG);
                    definition
                }
                MySqlViewColumnReading::Sum(_) | MySqlViewColumnReading::Average(_) => {
                    return Err(FrontendErrorKind::Unsupported);
                }
            };
            database.clone_into(&mut definition.schema);
            view.as_str().clone_into(&mut definition.original_table);
            definition.original_name.clone_from(name);
            Ok(definition)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
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
    if matches!(
        column.type_name(),
        "TEXT"
            | "TINYTEXT"
            | "MEDIUMTEXT"
            | "LONGTEXT"
            | "BLOB"
            | "TINYBLOB"
            | "MEDIUMBLOB"
            | "LONGBLOB"
    ) {
        flags |= MYSQL_BLOB_FLAG;
    }
    if matches!(
        column.type_name(),
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB"
    ) {
        flags |= MYSQL_BINARY_FLAG;
    }
    // Measured on MySQL 8.4.11: a JSON column carries both, as a BLOB does.
    if column.type_name() == "JSON" {
        flags |= MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;
    }
    // Measured on MySQL 8.4.11: a VARBINARY carries the binary flag, as a BLOB
    // does, and not the blob one.
    if column.type_name() == "VARBINARY" {
        flags |= MYSQL_BINARY_FLAG;
    }
    if is_unsigned_integer_type(column.type_name())
        || matches!(
            column.type_name(),
            "DOUBLE UNSIGNED" | "FLOAT UNSIGNED" | "DECIMAL UNSIGNED"
        )
    {
        flags |= MYSQL_UNSIGNED_FLAG;
    }
    flags
}

/// Answers whether a declared type name is one of the unsigned integers.
///
/// The name is kept whole in the stored DDL — `INT UNSIGNED`, not `INT` with a
/// flag beside it — so this is the only place the sign is read back from.
fn is_unsigned_integer_type(name: &str) -> bool {
    [
        "TINYINT UNSIGNED",
        "SMALLINT UNSIGNED",
        "MEDIUMINT UNSIGNED",
        "INT UNSIGNED",
        "INTEGER UNSIGNED",
        "BIGINT UNSIGNED",
    ]
    .iter()
    .any(|unsigned| name.eq_ignore_ascii_case(unsigned))
}

/// Returns the display width MySQL reports for an unsigned integer column.
///
/// Measured on MySQL 8.4.11: 3, 5, 8 and 10, each one narrower than the signed
/// counterpart's 4, 6, 9 and 11, because an unsigned column spends no character
/// on the sign.
fn unsigned_integer_column_length(name: &str) -> Option<u32> {
    for (unsigned, length) in [
        ("TINYINT UNSIGNED", 3),
        ("SMALLINT UNSIGNED", 5),
        ("MEDIUMINT UNSIGNED", 8),
        ("INT UNSIGNED", 10),
        ("INTEGER UNSIGNED", 10),
        // Measured: a BIGINT UNSIGNED reports 20, the same as the signed one,
        // because its top value is as many digits as the signed type's sign
        // and digits together.
        ("BIGINT UNSIGNED", 20),
    ] {
        if name.eq_ignore_ascii_case(unsigned) {
            return Some(length);
        }
    }
    None
}

/// MySQL refuses a declared `DECIMAL` or arithmetic result wider than this.
/// `SUM` and `AVG` still report widths above it for a wide source column.
const MYSQL_MAX_DECIMAL_PRECISION: u32 = 65;
/// MySQL keeps aggregate result scale within the DECIMAL column limit.
const MYSQL_MAX_DECIMAL_SCALE: u32 = 30;

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
const MYSQL_TYPE_JSON: u8 = 0xf5;
const MYSQL_TYPE_BLOB: u8 = 0xfc;
const MYSQL_TYPE_MEDIUM_BLOB: u8 = 0xfa;
const MYSQL_TYPE_LONG_BLOB: u8 = 0xfb;
const MYSQL_TYPE_DATETIME: u8 = 0x0c;
const MYSQL_TYPE_DATE: u8 = 0x0a;
const MYSQL_TYPE_TIME: u8 = 0x0b;
const MYSQL_TYPE_YEAR: u8 = 0x0d;
const MYSQL_TYPE_TIMESTAMP: u8 = 0x07;
const MYSQL_TYPE_NEWDECIMAL: u8 = 0xf6;
pub(crate) const MYSQL_NOT_NULL_FLAG: u16 = 1;
#[cfg(unix)]
const MYSQL_PRI_KEY_FLAG: u16 = 2;
#[cfg(unix)]
const MYSQL_UNIQUE_KEY_FLAG: u16 = 4;
#[cfg(unix)]
const MYSQL_MULTIPLE_KEY_FLAG: u16 = 8;
#[cfg(unix)]
const MYSQL_PART_KEY_FLAG: u16 = 16_384;
const MYSQL_BLOB_FLAG: u16 = 16;
pub(crate) const MYSQL_UNSIGNED_FLAG: u16 = 32;
const MYSQL_ZEROFILL_FLAG: u16 = 64;
pub(crate) const MYSQL_NUM_FLAG: u16 = 32_768;
/// MySQL marks a grouping key with the bit it also sends as the numeric flag,
/// so a client reads a word that is a key as flagged numeric.
#[cfg(unix)]
const MYSQL_GROUP_FLAG: u16 = 32_768;
pub(crate) const MYSQL_BINARY_FLAG: u16 = 128;
const MYSQL_ENUM_FLAG: u16 = 256;
const MYSQL_SET_FLAG: u16 = 2048;
#[cfg(unix)]
const MYSQL_AUTO_INCREMENT_FLAG: u16 = 512;
pub(crate) const MYSQL_NO_DEFAULT_VALUE_FLAG: u16 = 4096;
pub(crate) const MYSQL_BINARY_COLLATION: u16 = 63;

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
        keyword.eq_ignore_ascii_case("CREATE")
            || keyword.eq_ignore_ascii_case("ALTER")
            // MySQL spells dropping an index as a statement of its own as well
            // as an `ALTER TABLE`, and the reader below answers both. A `DROP
            // TABLE` and a `DROP VIEW` have their own readers ahead of this.
            || keyword.eq_ignore_ascii_case("DROP")
    })
}

fn statement_keyword(sql: &str) -> Option<&str> {
    let mut sql = strip_leading_sql_comments(sql);
    while let Some(rest) = sql.strip_prefix('(') {
        sql = strip_leading_sql_comments(rest);
    }
    let end = sql
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .unwrap_or(sql.len());
    (!sql[..end].is_empty()).then_some(&sql[..end])
}

fn strip_leading_sql_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if sql.starts_with("/*!") {
            return sql;
        }
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

#[cfg(test)]
#[test]
fn a_mysqldump_drop_view_comment_remains_executable() {
    let sql = "/* regular */ /*!50001 DROP VIEW IF EXISTS `dump_names`*/";
    let executable = strip_leading_sql_comments(sql);
    assert_eq!(
        turso_mysql_parser::parse_optional_mysqldump_drop_view(
            executable,
            SessionSqlMode::default()
        )
        .unwrap()
        .unwrap()
        .as_str(),
        "dump_names"
    );
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
    // Measured on MySQL 8.4.11: a VARBINARY reports VAR_STRING like a VARCHAR
    // and differs in its collation, its flags, and that its length is the byte
    // count rather than four bytes for each character.
    if name.eq_ignore_ascii_case("VARBINARY") {
        return Some(MYSQL_TYPE_VAR_STRING);
    }
    // Measured on MySQL 8.4.11: a CHAR column reports 254, not 253.
    if name.eq_ignore_ascii_case("CHAR") {
        return Some(MYSQL_TYPE_STRING);
    }
    // Measured on MySQL 8.4.11: an unsigned DOUBLE or FLOAT reports the same
    // type and the same length its signed form does, and the sign is a flag.
    if name.eq_ignore_ascii_case("DOUBLE") || name.eq_ignore_ascii_case("DOUBLE UNSIGNED") {
        return Some(MYSQL_TYPE_DOUBLE);
    }
    if name.eq_ignore_ascii_case("FLOAT") || name.eq_ignore_ascii_case("FLOAT UNSIGNED") {
        return Some(MYSQL_TYPE_FLOAT);
    }
    if name.eq_ignore_ascii_case("BOOLEAN") {
        return Some(MYSQL_TYPE_TINY);
    }
    if name.eq_ignore_ascii_case("DATETIME") {
        return Some(MYSQL_TYPE_DATETIME);
    }
    if name.eq_ignore_ascii_case("DATE") {
        return Some(MYSQL_TYPE_DATE);
    }
    if name.eq_ignore_ascii_case("TIME") {
        return Some(MYSQL_TYPE_TIME);
    }
    if name.eq_ignore_ascii_case("YEAR") {
        return Some(MYSQL_TYPE_YEAR);
    }
    if name.eq_ignore_ascii_case("BIT") {
        return Some(MYSQL_TYPE_BIT);
    }
    if name.eq_ignore_ascii_case("JSON") {
        return Some(MYSQL_TYPE_JSON);
    }
    // Measured on MySQL 8.4.11: an ENUM column reports the fixed-width
    // string type, as a CHAR does, and says which it is with a flag.
    if turso_mysql_parser::enum_members(name).is_some()
        || turso_mysql_parser::set_members(name).is_some()
    {
        return Some(MYSQL_TYPE_STRING);
    }
    if name.eq_ignore_ascii_case("TIMESTAMP") {
        return Some(MYSQL_TYPE_TIMESTAMP);
    }
    // Both word orders reach this: the MySQL one from a column's stored type
    // name, and the engine's own from a statement's declared type, which takes
    // the sign before the arguments.
    if name.eq_ignore_ascii_case("DECIMAL")
        || name.eq_ignore_ascii_case("DECIMAL UNSIGNED")
        || name.eq_ignore_ascii_case("UNSIGNED DECIMAL")
    {
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
    // Measured on MySQL 8.4.11: an unsigned column reports the same wire type
    // its signed counterpart does. What differs is the length, one digit
    // narrower because no digit is spent on the sign, and the UNSIGNED flag.
    if name.eq_ignore_ascii_case("TINYINT UNSIGNED") {
        return Some(MYSQL_TYPE_TINY);
    }
    if name.eq_ignore_ascii_case("SMALLINT UNSIGNED") {
        return Some(MYSQL_TYPE_SHORT);
    }
    if name.eq_ignore_ascii_case("MEDIUMINT UNSIGNED") {
        return Some(MYSQL_TYPE_INT24);
    }
    if name.eq_ignore_ascii_case("INT UNSIGNED") || name.eq_ignore_ascii_case("INTEGER UNSIGNED") {
        return Some(MYSQL_TYPE_LONG);
    }
    if name.eq_ignore_ascii_case("BIGINT UNSIGNED") {
        return Some(MYSQL_TYPE_LONGLONG);
    }
    if name.eq_ignore_ascii_case("REAL") {
        return Some(MYSQL_TYPE_DOUBLE);
    }
    // Measured on MySQL 8.4.11: a TEXT column reports BLOB, and differs from a
    // BLOB column only in its collation and length.
    if name.eq_ignore_ascii_case("TEXT")
        || name.eq_ignore_ascii_case("TINYTEXT")
        || name.eq_ignore_ascii_case("MEDIUMTEXT")
        || name.eq_ignore_ascii_case("LONGTEXT")
    {
        return Some(MYSQL_TYPE_BLOB);
    }
    if name.eq_ignore_ascii_case("BLOB")
        || name.eq_ignore_ascii_case("TINYBLOB")
        || name.eq_ignore_ascii_case("MEDIUMBLOB")
        || name.eq_ignore_ascii_case("LONGBLOB")
    {
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
    /// The note MySQL raises for `CREATE TABLE IF NOT EXISTS` naming a table
    /// that is already there.
    ///
    /// Measured on MySQL 8.4.11: `Note`, code 1050, and a message naming the
    /// table on its own — where 1051 names it with its database.
    fn table_exists(table: &str) -> Self {
        Self {
            level: "Note",
            code: 1050,
            message: format!("Table '{table}' already exists"),
        }
    }

    /// The warning MySQL raises for a `GROUP_CONCAT` longer than the
    /// session's `group_concat_max_len`, naming the row the cut fell at.
    ///
    /// Measured on MySQL 8.4.11: `Warning`, code 1260, and this message.
    fn cut_by_group_concat(row: u64) -> Self {
        Self {
            level: "Warning",
            code: 1260,
            message: format!("Row {row} was cut by GROUP_CONCAT()"),
        }
    }

    /// The warning MySQL raises for `WITH CONSISTENT SNAPSHOT` at a level
    /// that keeps no snapshot. Measured on MySQL 8.4.11 under `READ
    /// COMMITTED`: `Warning`, code 138, and this message.
    fn consistent_snapshot_ignored() -> Self {
        Self {
            level: "Warning",
            code: 138,
            message: "InnoDB: WITH CONSISTENT SNAPSHOT was ignored because this phrase can only be used with REPEATABLE READ isolation level.".to_owned(),
        }
    }

    /// The note MySQL raises for `ALTER TABLE t DISABLE KEYS` and `ENABLE
    /// KEYS` over an InnoDB table.
    ///
    /// Measured on MySQL 8.4.11: `Note`, code 1031, and this message.
    fn keys_have_no_switch(table: &str) -> Self {
        Self {
            level: "Note",
            code: 1031,
            message: format!("Table storage engine for '{table}' doesn't have this option"),
        }
    }

    /// The note MySQL raises for `CREATE DATABASE IF NOT EXISTS` naming a
    /// database that is already there.
    ///
    /// Measured on MySQL 8.4.11: `Note`, code 1007, and this message.
    fn database_exists(database: &str) -> Self {
        Self {
            level: "Note",
            code: 1007,
            message: format!("Can't create database '{database}'; database exists"),
        }
    }

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

    /// The note MySQL raises for `DROP VIEW IF EXISTS` naming a table.
    ///
    /// Measured on MySQL 8.4.11: `Note`, code 1347, `'probe.plain' is not
    /// VIEW`.
    fn not_a_view(database: Option<&str>, table: &str) -> Self {
        let qualified = match database {
            Some(database) => format!("{database}.{table}"),
            None => table.to_owned(),
        };
        Self {
            level: "Note",
            code: 1347,
            message: format!("'{qualified}' is not VIEW"),
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

/// Answers `SHOW ENGINES` with the one storage engine this server has.
///
/// MySQL 8.4.11 lists eleven, most of them unavailable; naming MyISAM or CSV
/// here would claim engines that do not exist. The one row describes what is
/// actually on offer, under the name `SHOW CREATE TABLE` already reports.
///
/// The last three columns are answered about this server rather than copied
/// from MySQL's InnoDB row, which says YES to all three. Transactions work;
/// `XA` and `SAVEPOINT` do not, and a client that reads those columns before
/// using either is better served by the truth. The column shapes are measured:
/// six VAR_STRING columns of length 256, 32, 320, 12, 12 and 12, carrying the
/// connection's utf8mb4 collation, with the first three NOT NULL.
fn show_engines_result(status_flags: u16) -> CommandExecutionResult {
    let column = |name: &str, length: u32, not_null: bool| {
        let mut column = column_definition(name.to_owned(), MYSQL_TYPE_VAR_STRING);
        column.column_length = length;
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.decimals = 0;
        set_column_flags(&mut column, if not_null { MYSQL_NOT_NULL_FLAG } else { 0 });
        column
    };
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![
            column("Engine", 256, true),
            column("Support", 32, true),
            column("Comment", 320, true),
            column("Transactions", 12, false),
            column("XA", 12, false),
            column("Savepoints", 12, false),
        ],
        rows: vec![vec![
            Some(b"InnoDB".to_vec()),
            Some(b"DEFAULT".to_vec()),
            Some(b"Supports transactions and row-level locking".to_vec()),
            Some(b"YES".to_vec()),
            Some(b"NO".to_vec()),
            Some(b"NO".to_vec()),
        ]],
        warnings: 0,
        status_flags,
    })
}

/// Answers `SHOW COLLATION` and `SHOW CHARACTER SET` with what this server
/// has.
///
/// This server speaks utf8mb4 alone, so it lists the collations a column, a
/// table or the connection may be declared with — `utf8mb4_0900_ai_ci`,
/// `utf8mb4_bin`, `utf8mb4_unicode_ci`, and `utf8mb4_general_ci`, which the
/// handshake names — and the binary one every `BLOB` and `VARBINARY` holds,
/// where MySQL lists every one it has. Each row, and each column's shape, was
/// measured on MySQL 8.4.11, which lists both in name order. A `LIKE` and a
/// word compared in a `WHERE` match without regard to case and to trailing
/// spaces, as MySQL matches them there.
fn show_character_sets_result(
    command: &MySqlShowCharacterSetsCommand,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let keyed = MYSQL_UNIQUE_KEY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG;
    let (columns, rows, filter): (_, &[&[&str]], _) = match command {
        MySqlShowCharacterSetsCommand::Collations(filter) => (
            show_listing_columns(
                "COLLATIONS",
                &[
                    (
                        "Collation",
                        "COLLATIONS",
                        MYSQL_TYPE_VAR_STRING,
                        256,
                        MYSQL_NO_DEFAULT_VALUE_FLAG,
                    ),
                    (
                        "Charset",
                        "COLLATIONS",
                        MYSQL_TYPE_VAR_STRING,
                        256,
                        MYSQL_NO_DEFAULT_VALUE_FLAG,
                    ),
                    (
                        "Id",
                        "COLLATIONS",
                        MYSQL_TYPE_LONGLONG,
                        20,
                        MYSQL_UNSIGNED_FLAG,
                    ),
                    ("Default", "COLLATIONS", MYSQL_TYPE_VAR_STRING, 12, 0),
                    ("Compiled", "COLLATIONS", MYSQL_TYPE_VAR_STRING, 12, 0),
                    (
                        "Sortlen",
                        "COLLATIONS",
                        MYSQL_TYPE_LONG,
                        10,
                        MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                    ),
                    (
                        "Pad_attribute",
                        "COLLATIONS",
                        MYSQL_TYPE_STRING,
                        36,
                        MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                    ),
                ],
            ),
            &[
                &["binary", "binary", "63", "Yes", "Yes", "1", "NO PAD"],
                &[
                    "utf8mb4_0900_ai_ci",
                    "utf8mb4",
                    "255",
                    "Yes",
                    "Yes",
                    "0",
                    "NO PAD",
                ],
                &["utf8mb4_bin", "utf8mb4", "46", "", "Yes", "1", "PAD SPACE"],
                &[
                    "utf8mb4_general_ci",
                    "utf8mb4",
                    "45",
                    "",
                    "Yes",
                    "1",
                    "PAD SPACE",
                ],
                &[
                    "utf8mb4_unicode_ci",
                    "utf8mb4",
                    "224",
                    "",
                    "Yes",
                    "8",
                    "PAD SPACE",
                ],
            ],
            filter,
        ),
        MySqlShowCharacterSetsCommand::CharacterSets(filter) => (
            show_listing_columns(
                "CHARACTER_SETS",
                &[
                    ("Charset", "cs", MYSQL_TYPE_VAR_STRING, 256, keyed),
                    (
                        "Description",
                        "cs",
                        MYSQL_TYPE_VAR_STRING,
                        8192,
                        MYSQL_NO_DEFAULT_VALUE_FLAG,
                    ),
                    (
                        "Default collation",
                        "col",
                        MYSQL_TYPE_VAR_STRING,
                        256,
                        keyed,
                    ),
                    (
                        "Maxlen",
                        "cs",
                        MYSQL_TYPE_LONG,
                        10,
                        MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                    ),
                ],
            ),
            &[
                &["binary", "Binary pseudo charset", "binary", "1"],
                &["utf8mb4", "UTF-8 Unicode", "utf8mb4_0900_ai_ci", "4"],
            ],
            filter,
        ),
    };
    let mut kept = Vec::new();
    for row in rows {
        if show_listing_keeps(filter, &columns, row)? {
            kept.push(
                row.iter()
                    .map(|value| Some(value.as_bytes().to_vec()))
                    .collect(),
            );
        }
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows: kept,
        warnings: 0,
        status_flags,
    }))
}

/// The columns of a `SHOW` listing that reads one `information_schema`
/// table, every one of them NOT NULL.
fn show_listing_columns(
    table: &str,
    columns: &[(&str, &str, u8, u32, u16)],
) -> Vec<ColumnDefinitionConfig> {
    columns
        .iter()
        .map(|(name, original_table, column_type, length, flags)| {
            let mut column = column_definition((*name).to_owned(), *column_type);
            "information_schema".clone_into(&mut column.schema);
            table.clone_into(&mut column.table);
            (*original_table).clone_into(&mut column.original_table);
            column.original_name = column.name.clone();
            column.column_length = *length;
            column.decimals = 0;
            set_column_flags(&mut column, MYSQL_NOT_NULL_FLAG | flags);
            column
        })
        .collect()
}

/// Reports whether a `SHOW` listing keeps one of its rows.
///
/// A `LIKE` reads the first column. A `WHERE` names the columns by name,
/// without regard to case, and one the listing does not have is refused, as
/// MySQL answers 1054 for it.
fn show_listing_keeps(
    filter: &MySqlShowListingFilter,
    columns: &[ColumnDefinitionConfig],
    row: &[&str],
) -> Result<bool, FrontendErrorKind> {
    match filter {
        MySqlShowListingFilter::Everything => Ok(true),
        MySqlShowListingFilter::Like(pattern) => Ok(pattern.matches(row[0])),
        MySqlShowListingFilter::Where(tests) => {
            for test in tests {
                let position = columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(test.column()))
                    .ok_or(FrontendErrorKind::UnknownColumn)?;
                let value = row[position];
                let holds = match test.test() {
                    MySqlShowValueTest::EqualsWord(word) => value
                        .trim_end_matches(' ')
                        .eq_ignore_ascii_case(word.trim_end_matches(' ')),
                    // A number compared with a column of words reads each
                    // word as a number, which has not been measured here.
                    MySqlShowValueTest::EqualsNumber(number) => {
                        value
                            .parse::<u64>()
                            .map_err(|_| FrontendErrorKind::Unsupported)?
                            == *number
                    }
                    MySqlShowValueTest::Like(pattern) => pattern.matches(value),
                };
                if !holds {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
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

/// Answers `SHOW COUNT(*) WARNINGS` for what the last statement raised.
fn show_warnings_count_result(
    warnings: &[MySqlWarning],
    status_flags: u16,
) -> CommandExecutionResult {
    show_diagnostics_count_result(
        "@@session.warning_count",
        warnings.len() as u64,
        status_flags,
    )
}

/// Answers `SHOW COUNT(*) ERRORS` for what the last statement raised.
fn show_errors_count_result(
    warnings: &[MySqlWarning],
    status_flags: u16,
) -> CommandExecutionResult {
    let count = warnings
        .iter()
        .filter(|warning| warning.level.eq_ignore_ascii_case("Error"))
        .count() as u64;
    show_diagnostics_count_result("@@session.error_count", count, status_flags)
}

/// Answers `SHOW COUNT(*) WARNINGS` or `SHOW COUNT(*) ERRORS`.
///
/// Measured on MySQL 8.4.11: the column is a `LONGLONG` of length 21 carrying
/// the unsigned, binary and numeric flags without `NOT_NULL`, reporting 0
/// decimals.
fn show_diagnostics_count_result(
    column_name: &str,
    count: u64,
    status_flags: u16,
) -> CommandExecutionResult {
    let mut column = column_definition(column_name.to_owned(), MYSQL_TYPE_LONGLONG);
    column.column_length = 21;
    column.decimals = 0;
    set_column_flags(&mut column, MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG);
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![column],
        rows: vec![vec![Some(count.to_string().into_bytes())]],
        warnings: 0,
        status_flags,
    })
}

/// Returns the flag a column carries because of its type alone.
///
/// Supplies `NUM` in expression fallback metadata. Direct table columns and
/// integer literals use their own measured flags.
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
            | MYSQL_TYPE_YEAR
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

fn last_insert_id_column_definition(name: String) -> ColumnDefinitionConfig {
    let mut definition = column_definition(name, MYSQL_TYPE_LONGLONG);
    definition.column_length = 21;
    set_column_flags(
        &mut definition,
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG,
    );
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
    /// A `DOUBLE`, which MySQL writes in a form of its own.
    Binary64,
    /// A `DECIMAL`, which MySQL renders at the scale the column declared, so a
    /// `DECIMAL(10,2)` holding 1.5 reads back as `1.50`.
    Scaled(u8),
    /// A stored DECIMAL is decoded to text by the engine, with every digit and
    /// the declared scale intact.
    ExactDecimal,
    /// An integer, which the engine answers as a float only when an arithmetic
    /// result left the range an integer can hold.
    Integer,
    /// A `YEAR`, which MySQL writes in four digits: measured on 8.4.11, the
    /// zero year reads back as `0000`.
    Year,
    /// A `BIT(1)`, which MySQL sends as the byte that holds its bit rather
    /// than as a number: measured on 8.4.11, `b'1'` crosses as the one byte
    /// 0x01.
    Bit,
}

impl TextValueRendering {
    fn for_column(column: &ColumnDefinitionConfig) -> Self {
        match column.column_type {
            MYSQL_TYPE_FLOAT => Self::Binary32,
            MYSQL_TYPE_DOUBLE => Self::Binary64,
            MYSQL_TYPE_NEWDECIMAL if is_exact_decimal_column(column) => Self::ExactDecimal,
            MYSQL_TYPE_NEWDECIMAL => Self::Scaled(column.decimals),
            MYSQL_TYPE_YEAR => Self::Year,
            MYSQL_TYPE_BIT => Self::Bit,
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG => Self::Integer,
            _ => Self::Engine,
        }
    }
}

fn is_exact_decimal_column(column: &ColumnDefinitionConfig) -> bool {
    column.column_type == MYSQL_TYPE_NEWDECIMAL && !column.original_table.is_empty()
}

/// Where MySQL stops writing a `DOUBLE` out in full and starts writing an
/// exponent, in digits before the point.
///
/// Measured on MySQL 8.4.11: `1e14` reads back as `100000000000000` and `1e15`
/// as `1e15`; `1e-15` reads back as `0.000000000000001` and `1e-16` as `1e-16`.
/// `123456789012345.6` is written out in full at sixteen digits, so the switch
/// is on where the point falls rather than on how many digits there are.
const MYSQL_DOUBLE_PLAIN_DIGITS: i32 = 15;

/// Renders a `DOUBLE` the way MySQL writes one.
///
/// MySQL writes the shortest digits that read back as the same double, which is
/// what Rust writes too, and then chooses between writing the number out in
/// full and writing an exponent. Measured on 8.4.11: `1` for a whole number
/// rather than `1.0`, `0.3333333333333333` at sixteen digits, `-0` for a
/// negative zero, and `1e20`, `1.5e-300` and `1.2345678901234568e16` with no
/// sign or padding on the exponent.
fn mysql_double_text(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let scientific = format!("{value:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return value.to_string();
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return value.to_string();
    };
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let digits = mantissa
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    // Where the decimal point falls, counting from the left of the digits.
    let point = exponent + 1;
    if point > MYSQL_DOUBLE_PLAIN_DIGITS || point <= -MYSQL_DOUBLE_PLAIN_DIGITS {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        return format!("{sign}{first}{fraction}e{exponent}");
    }
    if point <= 0 {
        return format!("{sign}0.{}{digits}", "0".repeat(-point as usize));
    }
    let point = point as usize;
    if point >= digits.len() {
        return format!("{sign}{digits}{}", "0".repeat(point - digits.len()));
    }
    let (whole, fraction) = digits.split_at(point);
    format!("{sign}{whole}.{fraction}")
}

/// Renders one result value the way the text protocol sends it.
fn value_to_text_ref(
    value: &Value,
    rendering: TextValueRendering,
) -> Result<Option<Vec<u8>>, LimboError> {
    if rendering == TextValueRendering::ExactDecimal
        && !matches!(value, Value::Null | Value::Text(_))
    {
        return Err(LimboError::InternalError(
            "stored DECIMAL was not decoded to exact text".to_owned(),
        ));
    }
    match value {
        Value::Null => Ok(None),
        Value::Numeric(Numeric::Float(float)) if rendering == TextValueRendering::Binary32 => {
            Ok(Some((f64::from(*float) as f32).to_string().into_bytes()))
        }
        Value::Numeric(Numeric::Float(float)) if rendering == TextValueRendering::Binary64 => {
            Ok(Some(mysql_double_text(f64::from(*float)).into_bytes()))
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
                    format_mysql_scaled_integer(*integer, scale).into_bytes(),
                ));
            }
            if rendering == TextValueRendering::Year {
                return Ok(Some(format!("{integer:04}").into_bytes()));
            }
            if rendering == TextValueRendering::Bit {
                return Ok(Some(vec![bit_byte(*integer)?]));
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

/// The byte a `BIT(1)` crosses the wire as. The column takes nothing but 0
/// and 1, so any other number read out of one is a stored value this frontend
/// never wrote.
fn bit_byte(value: i64) -> Result<u8, LimboError> {
    match value {
        0 => Ok(0),
        1 => Ok(1),
        _ => Err(LimboError::Corrupt(format!(
            "a BIT(1) column holds {value}, which is not a bit"
        ))),
    }
}

fn format_mysql_scaled_integer(value: i64, scale: u8) -> String {
    let mut rendered = value.to_string();
    if scale != 0 {
        rendered.push('.');
        rendered.push_str(&"0".repeat(usize::from(scale)));
    }
    rendered
}

fn frontend_error_kind(error: LimboError) -> FrontendErrorKind {
    match error {
        LimboError::NotNullConstraint { .. } => FrontendErrorKind::NotNullViolation,
        LimboError::NoSuchColumn { .. } => FrontendErrorKind::UnknownColumn,
        LimboError::AmbiguousColumn { .. } => FrontendErrorKind::AmbiguousColumn,
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
            if matches!(*error, turso_core::AssignmentError::OutOfRange { .. }) =>
        {
            FrontendErrorKind::OutOfRange
        }
        LimboError::Assignment(error)
            if matches!(
                *error,
                turso_core::AssignmentError::IncorrectTemporal { .. }
            ) =>
        {
            FrontendErrorKind::IncorrectTemporalValue
        }
        LimboError::Assignment(error)
            if matches!(*error, turso_core::AssignmentError::NotAMember { .. }) =>
        {
            FrontendErrorKind::NotAMember
        }
        LimboError::Assignment(error)
            if matches!(*error, turso_core::AssignmentError::NotADocument { .. }) =>
        {
            FrontendErrorKind::InvalidJsonText
        }
        // The engine holds one write lock over the database, so a session that
        // cannot take it is a session waiting on a lock. MySQL answers 1205
        // for that, which is what this reports.
        LimboError::Busy => FrontendErrorKind::DatabaseBusy,
        // A transaction whose snapshot went stale can never write, however
        // long it waits. The caller rolls it back and answers what MySQL
        // answers for a transaction it has to give up on.
        LimboError::BusySnapshot => FrontendErrorKind::SerializationFailure,
        LimboError::ForeignKeyConstraint(_) => FrontendErrorKind::ForeignKeyViolation,
        LimboError::IntegerOverflow => FrontendErrorKind::NumericOverflow,
        LimboError::InvalidArgument(message)
            if message.starts_with(turso_mysql::GROUP_CONCAT_CUT_ERROR) =>
        {
            FrontendErrorKind::GroupConcatCut
        }
        // Measured on MySQL 8.4.11: a row breaking a `CHECK` is 3819, where one
        // breaking a key is 1062.
        LimboError::Constraint(message) if message.starts_with("CHECK constraint failed") => {
            FrontendErrorKind::CheckConstraintViolated
        }
        LimboError::Constraint(_) | LimboError::Raise(..) | LimboError::NullValue => {
            FrontendErrorKind::ConstraintViolation
        }
        _ => FrontendErrorKind::Unsupported,
    }
}

fn frontend_prepare_error(error: MySqlQueryError) -> FrontendErrorKind {
    match error {
        MySqlQueryError::MissingRequiredDefault(_) => FrontendErrorKind::MissingRequiredDefault,
        MySqlQueryError::DuplicateColumn(_) => FrontendErrorKind::DuplicateColumn,
        MySqlQueryError::DuplicateIndex => FrontendErrorKind::DuplicateKeyName,
        MySqlQueryError::MissingIndex => FrontendErrorKind::CantDropKey,
        MySqlQueryError::RequiredByForeignKey => FrontendErrorKind::RequiredForeignKeyIndex,
        MySqlQueryError::MissingTable => FrontendErrorKind::UnknownTable,
        MySqlQueryError::JsonIndex => FrontendErrorKind::JsonIndex,
        MySqlQueryError::JsonLiteralDefault => FrontendErrorKind::JsonLiteralDefault,
        MySqlQueryError::ReadOnlyTransaction => FrontendErrorKind::ReadOnlyTransaction,
        MySqlQueryError::NoSuchSavepoint => FrontendErrorKind::NoSuchSavepoint,
        MySqlQueryError::NoSuchCheck(_) => FrontendErrorKind::NoSuchCheck,
        MySqlQueryError::DuplicateCheckName(_) => FrontendErrorKind::DuplicateCheckName,
        MySqlQueryError::Syntax(_) => FrontendErrorKind::Syntax,
        MySqlQueryError::Unsupported(_) => FrontendErrorKind::Unsupported,
        MySqlQueryError::Engine(error) => frontend_error_kind(error),
    }
}

#[cfg(unix)]
fn admin_error_kind(error: MySqlAdminCommandError) -> FrontendErrorKind {
    match error {
        MySqlAdminCommandError::Syntax => FrontendErrorKind::Syntax,
        MySqlAdminCommandError::Unsupported => FrontendErrorKind::Unsupported,
        MySqlAdminCommandError::UnknownCollation => FrontendErrorKind::UnknownCollation,
        MySqlAdminCommandError::UnknownCharacterSet => FrontendErrorKind::UnknownCharacterSet,
        MySqlAdminCommandError::CollationOfAnotherCharacterSet => {
            FrontendErrorKind::CollationOfAnotherCharacterSet
        }
        MySqlAdminCommandError::ConflictingCharacterSets => {
            FrontendErrorKind::ConflictingCharacterSets
        }
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
