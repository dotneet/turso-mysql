//! Adapter from the bounded MySQL frontend to the transport-neutral server.
//!
//! The dependency points from the protocol crate to the frontend crate.  The
//! frontend does not depend on this crate, so this keeps the execution boundary
//! one-way while allowing a server owner to opt into the checked SELECT slice.

use std::collections::HashMap;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

use turso_core::{LimboError, Numeric, Value};
#[cfg(unix)]
use turso_mysql::{
    canonicalize_database_name, MySqlDatabaseCatalog, MySqlDatabaseError, MySqlDatabaseSession,
};
#[cfg(unix)]
use turso_mysql::{MySqlAdminCommand, MySqlAdminCommandError, MySqlAdminCommandResult};
use turso_mysql::{
    MySqlAffectedRowsMode, MySqlConnection, MySqlPreparedExecutionResult, MySqlQueryError,
};
use turso_mysql::{
    MySqlPreparedStatementError, MySqlPreparedStatementMetadata, MySqlPreparedValue,
};

#[cfg(unix)]
use crate::{
    authorization_frontend_error, AuthenticatedCommandExecutor, AuthenticatedExecutorFactory,
    AuthenticatedPrincipal, AuthorizationError, DatabaseAction, DatabaseAuthorizer,
};
use crate::{
    decode_statement_execute_parameters_with_long_data, BinaryResultSet, BinaryResultValue,
    ColumnDefinitionConfig, CommandExecutionOptions, CommandExecutionResult, CommandExecutor,
    CommandOkResult, FrontendErrorKind, InitialDatabaseSelector, PreparedStatementExecutionResult,
    PreparedStatementResult, StatementExecuteDecodeError, StatementParameterType,
    StatementParameterValue, TextResultSet, DEFAULT_UTF8MB4_COLLATION, MAX_DISPATCH_RESULT_ROWS,
    MAX_RESPONSE_PACKET_PAYLOAD_LENGTH, MAX_RESULT_COLUMNS, MAX_TEXT_ROW_VALUE_LENGTH,
    SERVER_STATUS_AUTOCOMMIT, SERVER_STATUS_IN_TRANS,
};

/// Executes the frontend's checked MySQL SELECT subset for classic commands.
///
/// This adapter owns one [`MySqlConnection`].  It deliberately accepts only
/// SELECT text in `COM_QUERY`; schema writes and every other statement remain
/// outside the classic command slice until their protocol semantics are wired.
/// `COM_INIT_DB` is denied because a directly supplied connection has no
/// logical-database catalog.
pub struct MySqlCommandAdapter {
    connection: MySqlConnection,
    prepared_types: HashMap<u32, Vec<StatementParameterType>>,
    pending_long_data: PendingLongData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingLongDataError {
    UnknownStatement,
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
            prepared_types: HashMap::new(),
            pending_long_data: PendingLongData::default(),
        }
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
        execute_checked_query(&self.connection, sql, None, MySqlAffectedRowsMode::Changed)
    }

    fn execute_stmt_prepare(
        &mut self,
        sql: &str,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        prepare_checked_statement(&self.connection, sql)
    }

    fn execute_stmt_close(&mut self, statement_id: u32) {
        self.connection.remove_prepared_statement(statement_id);
        self.prepared_types.remove(&statement_id);
        self.pending_long_data.clear_statement(statement_id);
    }

    fn execute_stmt_reset(&mut self, statement_id: u32) -> Result<(), FrontendErrorKind> {
        let result = self.connection
            .reset_prepared_statement(statement_id)
            .map_err(prepared_statement_error);
        if result.is_ok() {
            self.pending_long_data.clear_statement(statement_id);
        }
        result
    }

    fn execute_stmt_send_long_data(
        &mut self,
        statement_id: u32,
        parameter_id: u16,
        data: &[u8],
    ) {
        let parameter_count = self
            .connection
            .prepared_statement_metadata(statement_id)
            .map(|metadata| metadata.parameter_count);
        self.pending_long_data.append(
            statement_id,
            parameter_id,
            data,
            parameter_count,
        );
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
    query_timeout: Option<Duration>,
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
            query_timeout: None,
        }
    }

    /// Applies the runtime's validated timeout to each checked SELECT.
    pub(crate) fn with_query_timeout(mut self, timeout: Duration) -> Self {
        assert!(!timeout.is_zero(), "query timeout must be non-zero");
        self.query_timeout = Some(timeout);
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
            session: self.catalog.new_session(self.schema_context),
            principal,
            authorizer: self.authorizer,
            query_timeout: self.query_timeout,
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
    command_options: CommandExecutionOptions,
    prepared_statements: DatabasePreparedStatementRegistry,
    pending_long_data: PendingLongData,
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
        if let Some(command) = self
            .session
            .parse_admin_command(sql)
            .map_err(admin_error_kind)?
        {
            return self.execute_admin_command(command);
        }

        let selected_database = self
            .session
            .selected_database()
            .ok_or(FrontendErrorKind::NoDatabaseSelected)?
            .to_owned();
        self.authorize(DatabaseAction::Query {
            database: &selected_database,
        })?;
        let connection = self.session.connection().map_err(database_error_kind)?;
        let affected_rows_mode = if self.command_options.client_found_rows() {
            MySqlAffectedRowsMode::Matched
        } else {
            MySqlAffectedRowsMode::Changed
        };
        execute_checked_query(connection, sql, self.query_timeout, affected_rows_mode)
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
        self.authorize(DatabaseAction::Query {
            database: &selected_database,
        })?;
        let connection = self
            .session
            .connection()
            .map_err(database_error_kind)?
            .clone();
        let metadata = connection
            .prepare_checked_statement(sql)
            .map_err(prepared_statement_error)?;
        let connection_statement_id = metadata.statement_id;
        let statement_id = self
            .prepared_statements
            .next_statement_id
            .ok_or(FrontendErrorKind::Internal)?;
        let result = prepared_statement_result(
            &connection,
            MySqlPreparedStatementMetadata {
                statement_id,
                ..metadata
            },
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

    fn execute_stmt_send_long_data(
        &mut self,
        statement_id: u32,
        parameter_id: u16,
        data: &[u8],
    ) {
        let parameter_count = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .and_then(|statement| {
                statement
                    .connection
                    .prepared_statement_metadata(statement.connection_statement_id)
            })
            .map(|metadata| metadata.parameter_count);
        self.pending_long_data.append(
            statement_id,
            parameter_id,
            data,
            parameter_count,
        );
    }

    fn execute_stmt_execute(
        &mut self,
        statement_id: u32,
        parameter_payload: &[u8],
    ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
        let long_data = self.pending_long_data.take_statement(statement_id);
        let database = self
            .prepared_statements
            .statements
            .get(&statement_id)
            .ok_or(FrontendErrorKind::UnknownPreparedStatement)?
            .database
            .clone();
        self.authorize(DatabaseAction::Query {
            database: &database,
        })?;
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
    query_timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let sql = strip_leading_sql_comments(sql);
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
        connection
            .execute_schema_ddl(sql)
            .map_err(frontend_query_error)?;
        return Ok(CommandExecutionResult::Ok(CommandOkResult {
            status_flags: connection_status_flags(connection),
            ..CommandOkResult::default()
        }));
    }
    if is_select_statement(sql) {
        let mut result = execute_checked_select_with_timeout(connection, sql, query_timeout)?;
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
    prepared_statement_result(connection, metadata)
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
        metadata,
        values,
        timeout,
        affected_rows_mode,
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
        metadata,
        values,
        timeout,
        affected_rows_mode,
    )
}

fn execute_prepared_values(
    connection: &MySqlConnection,
    statement_id: u32,
    metadata: MySqlPreparedStatementMetadata,
    values: Vec<MySqlPreparedValue>,
    timeout: Option<Duration>,
    affected_rows_mode: MySqlAffectedRowsMode,
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
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
                if row.len() != metadata.result_columns.len() {
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
    let column_types = binary_result_column_types(&metadata, &rows)?;
    let columns = metadata
        .result_columns
        .into_iter()
        .zip(&column_types)
        .map(|(column, column_type)| column_definition(column.name, *column_type))
        .collect();
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
    rows: &[Vec<MySqlPreparedValue>],
) -> Result<Vec<u8>, FrontendErrorKind> {
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
            let known_type = column.type_name.as_deref().and_then(mysql_type_for_name);
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
        MySqlPreparedValue::Integer(value) if column_type == MYSQL_TYPE_LONGLONG => {
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
) -> Result<PreparedStatementResult, FrontendErrorKind> {
    let parameters = (0..metadata.parameter_count)
        .map(|index| column_definition(format!("?{}", index + 1), MYSQL_TYPE_NULL))
        .collect();
    let columns = metadata
        .result_columns
        .into_iter()
        .map(|column| {
            let column_type = column
                .type_name
                .as_deref()
                .and_then(mysql_type_for_name)
                .unwrap_or(MYSQL_TYPE_NULL);
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
        MySqlPreparedStatementError::Prepare(error) => frontend_query_error(error),
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

fn execute_checked_select_with_timeout(
    connection: &MySqlConnection,
    sql: &str,
    query_timeout: Option<Duration>,
) -> Result<TextResultSet, FrontendErrorKind> {
    if !is_select_statement(sql) {
        return Err(FrontendErrorKind::Unsupported);
    }
    let mut statement = connection
        .prepare_select(sql)
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

    let columns = (0..column_count)
        .map(|index| {
            column_definition(
                statement.get_column_name(index).into_owned(),
                column_types[index].unwrap_or(MYSQL_TYPE_NULL),
            )
        })
        .collect();

    Ok(TextResultSet {
        columns,
        rows,
        warnings: 0,
        status_flags: 0x0002,
    })
}

const MYSQL_TYPE_DOUBLE: u8 = 0x05;
const MYSQL_TYPE_NULL: u8 = 0x06;
const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
const MYSQL_TYPE_BLOB: u8 = 0xfc;
const MYSQL_BINARY_COLLATION: u16 = 63;
const MAX_FRONTEND_ADAPTER_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PREPARED_LONG_DATA_BYTES: usize = 8 * 1024 * 1024;

impl PendingLongData {
    fn append(
        &mut self,
        statement_id: u32,
        parameter_id: u16,
        data: &[u8],
        parameter_count: Option<u16>,
    ) {
        if self.errors.contains_key(&statement_id) {
            return;
        }
        let Some(parameter_count) = parameter_count else {
            self.errors
                .insert(statement_id, PendingLongDataError::UnknownStatement);
            return;
        };
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
        PendingLongDataError::UnknownStatement => FrontendErrorKind::UnknownPreparedStatement,
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
        "INTEGER" => Some(MYSQL_TYPE_LONGLONG),
        "REAL" => Some(MYSQL_TYPE_DOUBLE),
        "TEXT" => Some(MYSQL_TYPE_VAR_STRING),
        "BLOB" => Some(MYSQL_TYPE_BLOB),
        _ => None,
    }
}

fn column_definition(name: String, column_type: u8) -> ColumnDefinitionConfig {
    let mut definition = ColumnDefinitionConfig::new(name, column_type);
    definition.character_set = if column_type == MYSQL_TYPE_VAR_STRING {
        u16::from(DEFAULT_UTF8MB4_COLLATION)
    } else {
        MYSQL_BINARY_COLLATION
    };
    definition.column_length = match column_type {
        MYSQL_TYPE_LONGLONG => 20,
        MYSQL_TYPE_DOUBLE => 22,
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_BLOB => MAX_TEXT_ROW_VALUE_LENGTH as u32,
        MYSQL_TYPE_NULL => 0,
        _ => 0,
    };
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
        LimboError::Constraint(_)
        | LimboError::ForeignKeyConstraint(_)
        | LimboError::Raise(..)
        | LimboError::NullValue => FrontendErrorKind::ConstraintViolation,
        _ => FrontendErrorKind::Unsupported,
    }
}

fn frontend_prepare_error(error: MySqlQueryError) -> FrontendErrorKind {
    match error {
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
fn database_list_column() -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new("Database", MYSQL_TYPE_VAR_STRING);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 64;
    column
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
    use turso_mysql::MySqlDatabaseCatalog;
    use turso_mysql::{
        schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext},
        MySqlDialect,
    };

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
        assert_eq!(first.columns[0].column_type, MYSQL_TYPE_NULL);
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
        let payload = [
            0,
            1,
            MYSQL_TYPE_VAR_STRING,
            0,
            MYSQL_TYPE_BLOB,
            0,
        ];

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
    }

    #[test]
    fn pending_long_data_limit_fails_without_retaining_the_overflowing_chunk() {
        let mut pending = PendingLongData::default();
        let full = vec![0xaa; MAX_PREPARED_LONG_DATA_BYTES];
        pending.append(1, 0, &full, Some(1));
        pending.append(1, 0, &[0xbb], Some(1));
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
    fn prepared_result_keeps_known_and_all_null_column_types() {
        let mut adapter = adapter();
        let unknown = adapter.execute_stmt_prepare("SELECT ?").unwrap();
        let null_result = adapter
            .execute_stmt_execute(unknown.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
            .unwrap();
        let null_result = prepared_result_set(null_result);
        assert_eq!(null_result.columns[0].column_type, MYSQL_TYPE_NULL);
        assert_eq!(null_result.rows, [vec![BinaryResultValue::Null]]);

        let known = adapter
            .execute_stmt_prepare("SELECT id FROM result_values")
            .unwrap();
        let known_result = adapter
            .execute_stmt_execute(known.statement_id, &[])
            .unwrap();
        let known_result = prepared_result_set(known_result);
        assert_eq!(known_result.columns[0].column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(
            known_result.rows,
            [
                vec![BinaryResultValue::Integer(1)],
                vec![BinaryResultValue::Integer(2)],
            ]
        );
    }

    #[cfg(unix)]
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedDatabaseAction {
        Connect(Option<String>),
        Query(String),
        Create(String),
        Drop(String),
        List,
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct RecordingAuthorizer {
        decisions: Mutex<VecDeque<Result<(), AuthorizationError>>>,
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
            adapter.execute_stmt_prepare("SELECT id FROM records"),
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
        assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONGLONG);
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
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
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
            adapter.execute_query("SHOW TABLES"),
            Err(FrontendErrorKind::Unsupported)
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
        assert_eq!(id_definition.column_type, MYSQL_TYPE_LONGLONG);
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
