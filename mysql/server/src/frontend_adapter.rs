//! Adapter from the bounded MySQL frontend to the transport-neutral server.
//!
//! The dependency points from the protocol crate to the frontend crate.  The
//! frontend does not depend on this crate, so this keeps the execution boundary
//! one-way while allowing a server owner to opt into the checked SELECT slice.

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
use turso_mysql::{MySqlAffectedRowsMode, MySqlConnection, MySqlQueryError};
use turso_mysql::{MySqlPreparedStatementError, MySqlPreparedStatementMetadata};

#[cfg(unix)]
use crate::{
    authorization_frontend_error, AuthenticatedCommandExecutor, AuthenticatedExecutorFactory,
    AuthenticatedPrincipal, AuthorizationError, DatabaseAction, DatabaseAuthorizer,
};
use crate::{
    ColumnDefinitionConfig, CommandExecutionOptions, CommandExecutionResult, CommandExecutor,
    CommandOkResult, FrontendErrorKind, InitialDatabaseSelector, PreparedStatementResult,
    TextResultSet,
    DEFAULT_UTF8MB4_COLLATION, MAX_DISPATCH_RESULT_ROWS, MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
    MAX_RESULT_COLUMNS, MAX_TEXT_ROW_VALUE_LENGTH, SERVER_STATUS_AUTOCOMMIT,
    SERVER_STATUS_IN_TRANS,
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
}

impl MySqlCommandAdapter {
    /// Creates an adapter around a checked MySQL frontend connection.
    pub fn new(connection: MySqlConnection) -> Self {
        Self { connection }
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
        if self
            .session
            .connection()
            .is_ok_and(|connection| !connection.is_auto_commit())
        {
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
                if self
                    .session
                    .connection()
                    .is_ok_and(|connection| !connection.is_auto_commit())
                {
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
        let connection = self.session.connection().map_err(database_error_kind)?;
        prepare_checked_statement(connection, sql)
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
        MySqlPreparedStatementError::UnknownStatement { .. } => FrontendErrorKind::MissingObject,
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
        assert_eq!(
            adapter.execute_stmt_prepare("DELETE FROM result_values"),
            Err(FrontendErrorKind::Unsupported)
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
        let prepared = adapter
            .execute_stmt_prepare("SELECT ? AS value")
            .unwrap();
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
