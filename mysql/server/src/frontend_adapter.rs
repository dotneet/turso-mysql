//! Adapter from the bounded MySQL frontend to the transport-neutral server.
//!
//! The dependency points from the protocol crate to the frontend crate.  The
//! frontend does not depend on this crate, so this keeps the execution boundary
//! one-way while allowing a server owner to opt into the checked SELECT slice.

use turso_core::{LimboError, Numeric, Value};
use turso_mysql::{MySqlConnection, MySqlQueryError};
#[cfg(unix)]
use turso_mysql::{MySqlDatabaseError, MySqlDatabaseSession};

use crate::{
    ColumnDefinitionConfig, CommandExecutionResult, CommandExecutor, CommandOkResult,
    FrontendErrorKind, InitialDatabaseSelector, TextResultSet, DEFAULT_UTF8MB4_COLLATION,
    MAX_DISPATCH_RESULT_ROWS, MAX_RESPONSE_PACKET_PAYLOAD_LENGTH, MAX_RESULT_COLUMNS,
    MAX_TEXT_ROW_VALUE_LENGTH,
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

    /// Returns the wrapped frontend connection without changing its ownership.
    pub fn connection(&self) -> &MySqlConnection {
        &self.connection
    }

    /// Returns the wrapped frontend connection.
    pub fn into_connection(self) -> MySqlConnection {
        self.connection
    }
}

impl CommandExecutor for MySqlCommandAdapter {
    fn execute_init_db(
        &mut self,
        _database: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        Err(FrontendErrorKind::Unsupported)
    }

    fn execute_query(&mut self, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind> {
        execute_checked_select(&self.connection, sql)
    }
}

/// Executes classic commands against one registry-backed MySQL session.
///
/// This adapter is Unix-only while the trusted catalog backend depends on
/// directory-descriptor operations. A successful database switch replaces the
/// selected connection; a failed switch leaves the old connection selected.
#[cfg(unix)]
pub struct MySqlDatabaseCommandAdapter {
    session: MySqlDatabaseSession,
}

#[cfg(unix)]
impl MySqlDatabaseCommandAdapter {
    /// Creates an adapter that owns one logical-database session.
    pub fn new(session: MySqlDatabaseSession) -> Self {
        Self { session }
    }

    /// Returns the session without changing its ownership.
    pub fn session(&self) -> &MySqlDatabaseSession {
        &self.session
    }

    /// Returns the owned session.
    pub fn into_session(self) -> MySqlDatabaseSession {
        self.session
    }
}

#[cfg(unix)]
impl CommandExecutor for MySqlDatabaseCommandAdapter {
    fn execute_init_db(
        &mut self,
        database: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind> {
        self.session
            .select_database(database)
            .map_err(database_error_kind)?;
        Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
    }

    fn execute_query(&mut self, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind> {
        let connection = self.session.connection().map_err(database_error_kind)?;
        execute_checked_select(connection, sql)
    }
}

#[cfg(unix)]
impl InitialDatabaseSelector for MySqlDatabaseCommandAdapter {
    fn select_initial_database(&mut self, database: &str) -> Result<(), FrontendErrorKind> {
        self.session
            .select_database(database)
            .map_err(database_error_kind)
    }
}

fn execute_checked_select(
    connection: &MySqlConnection,
    sql: &str,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
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
        .map_err(frontend_error_kind)?;

    let columns = (0..column_count)
        .map(|index| {
            column_definition(
                statement.get_column_name(index).into_owned(),
                column_types[index].unwrap_or(MYSQL_TYPE_NULL),
            )
        })
        .collect();

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows,
        warnings: 0,
        status_flags: 0x0002,
    }))
}

const MYSQL_TYPE_DOUBLE: u8 = 0x05;
const MYSQL_TYPE_NULL: u8 = 0x06;
const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
const MYSQL_TYPE_BLOB: u8 = 0xfc;
const MYSQL_BINARY_COLLATION: u16 = 63;
const MAX_FRONTEND_ADAPTER_RESULT_BYTES: usize = 8 * 1024 * 1024;

fn is_select_statement(sql: &str) -> bool {
    let sql = sql.trim_start();
    let Some(keyword) = sql.get(..6) else {
        return false;
    };
    keyword.eq_ignore_ascii_case("SELECT")
        && sql[6..].chars().next().is_none_or(char::is_whitespace)
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
        MySqlQueryError::Engine(error) => frontend_error_kind(error),
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;
    use crate::{
        dispatch_command_frame, AuthenticationResponse, ClassicConnection,
        ClientHandshakeResponseConfig, ConnectionState, InitialAuthenticationResult,
        InitialHandshakeSettings, PacketCodec, TextRowValue, TransportSecurity,
        CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_CONNECT_WITH_DB, COMMAND_SEQUENCE_ID, COM_INIT_DB,
        COM_QUERY, DEFAULT_UTF8MB4_COLLATION, REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
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

    #[cfg(unix)]
    fn catalog_adapter() -> (
        tempfile::TempDir,
        Arc<MySqlDatabaseCatalog>,
        MySqlDatabaseCommandAdapter,
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
        let adapter = MySqlDatabaseCommandAdapter::new(catalog.new_session(binary_context()));
        (directory, catalog, adapter)
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
    fn catalog_adapter_selects_with_init_db_and_requires_a_selection_for_query() {
        let (_directory, _catalog, mut adapter) = catalog_adapter();
        assert_eq!(
            adapter.execute_query("SELECT id FROM records"),
            Err(FrontendErrorKind::NoDatabaseSelected)
        );
        assert_eq!(
            adapter.execute_init_db("REPORTS"),
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        );
        assert_eq!(adapter.session().selected_database(), Some("reports"));

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
    }

    #[cfg(unix)]
    #[test]
    fn failed_init_db_keeps_the_previous_database_selected() {
        let (_directory, _catalog, mut adapter) = catalog_adapter();
        adapter.execute_init_db("reports").unwrap();

        assert_eq!(
            adapter.execute_init_db("missing"),
            Err(FrontendErrorKind::UnknownDatabase)
        );
        assert_eq!(adapter.session().selected_database(), Some("reports"));
        assert!(matches!(
            adapter.execute_query("SELECT id FROM records"),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
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
        let (_directory, _catalog, mut adapter) = catalog_adapter();
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
        assert_eq!(adapter.session().selected_database(), Some("reports"));
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[cfg(unix)]
    #[test]
    fn catalog_adapter_selects_the_handshake_database_before_authentication_ok() {
        let (_directory, _catalog, mut adapter) = catalog_adapter();
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
        assert_eq!(adapter.session().selected_database(), Some("reports"));
        assert_eq!(connection.state(), ConnectionState::Ready);
    }
}
