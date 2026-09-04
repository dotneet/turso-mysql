//! Transport-neutral dispatch from ready classic commands to server packets.
//!
//! This module owns neither a socket nor a SQL engine. A caller supplies an
//! execution port, while this layer keeps command gating, response sequencing,
//! and bounded protocol encoding in one place.

use std::{error::Error, fmt};

use crate::{
    map_frontend_error, BinaryRowPacket, BinaryRowValue, ClassicCommand, ClassicConnection,
    ColumnCountPacket, ColumnDefinitionConfig, CommandPacketError, ConnectionStateError, EofPacket,
    FrontendErrorKind, OkPacketConfig, PacketCodec, PacketSequence, ResponsePacketError,
    ResultTerminatorPacket, StmtPrepareOkPacketConfig, TextRowPacket, TextRowValue,
    CLIENT_DEPRECATE_EOF, CLIENT_FOUND_ROWS, COMMAND_SEQUENCE_ID,
};

/// The first packet sequence number used by a server response to a command.
pub const SERVER_RESPONSE_SEQUENCE_ID: u8 = COMMAND_SEQUENCE_ID.wrapping_add(1);
/// A transaction is active on the connection.
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
/// The session has autocommit enabled.
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
/// Maximum rows retained in one dispatcher result set.
pub const MAX_DISPATCH_RESULT_ROWS: usize = 4096;

/// A successful command response returned by an execution port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOkResult {
    /// Number of rows affected by the command.
    pub affected_rows: u64,
    /// Last inserted identifier, normally zero for non-insert commands.
    pub last_insert_id: u64,
    /// Server status flags for the response.
    pub status_flags: u16,
    /// Server warning count for the response.
    pub warnings: u16,
    /// Opaque informational bytes following the OK packet fields.
    pub info: Vec<u8>,
}

impl Default for CommandOkResult {
    fn default() -> Self {
        Self {
            affected_rows: 0,
            last_insert_id: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
            warnings: 0,
            info: Vec::new(),
        }
    }
}

/// A binary-safe text result set returned by an execution port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextResultSet {
    /// Column definitions, in their wire order.
    pub columns: Vec<ColumnDefinitionConfig>,
    /// Rows with one optional binary value for each column.
    pub rows: Vec<TextResultRow>,
    /// Server warning count for the result terminator.
    pub warnings: u16,
    /// Server status flags for the result terminator.
    pub status_flags: u16,
}

/// One binary-safe text result row. `None` is SQL NULL.
pub type TextResultRow = Vec<Option<Vec<u8>>>;

/// A binary-protocol result set returned by prepared statement execution.
#[derive(Debug, Clone, PartialEq)]
pub struct BinaryResultSet {
    /// Column definitions, in wire order.
    pub columns: Vec<ColumnDefinitionConfig>,
    /// Typed rows with one value per result column.
    pub rows: Vec<Vec<BinaryResultValue>>,
    /// Server warning count for the result terminator.
    pub warnings: u16,
    /// Server status flags for the result terminator.
    pub status_flags: u16,
}

/// One owned value in a prepared-statement binary result row.
#[derive(Debug, Clone, PartialEq)]
pub enum BinaryResultValue {
    /// SQL NULL.
    Null,
    /// Signed 64-bit integer.
    Integer(i64),
    /// IEEE-754 double.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Opaque bytes.
    Blob(Vec<u8>),
}

/// The successful result returned by a prepared statement execution port.
#[derive(Debug, Clone, PartialEq)]
pub enum PreparedStatementExecutionResult {
    /// A prepared statement produced binary-protocol rows.
    ResultSet(BinaryResultSet),
    /// A prepared statement completed with one OK packet.
    Ok(CommandOkResult),
}

/// The successful result returned by a command execution port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandExecutionResult {
    /// A command completed with one OK packet.
    Ok(CommandOkResult),
    /// A command produced a conservative text result set.
    ResultSet(TextResultSet),
}

/// Metadata returned after one statement is prepared and retained by an executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedStatementResult {
    /// Connection-local statement identifier.
    pub statement_id: u32,
    /// One definition for each positional parameter.
    pub parameters: Vec<ColumnDefinitionConfig>,
    /// Result columns in source order.
    pub columns: Vec<ColumnDefinitionConfig>,
    /// Warning count reported by the prepare operation.
    pub warnings: u16,
    /// Current server status flags for metadata terminators.
    pub status_flags: u16,
}

/// Compatibility alias for the command execution result.
pub type CommandResult = CommandExecutionResult;

/// Immutable execution options selected during the client handshake.
///
/// The options are derived from the server/client capability intersection once
/// and then passed to the authenticated executor factory. Keeping this as a
/// value object prevents a query from changing protocol semantics by mutating
/// handshake state after authentication.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandExecutionOptions {
    client_found_rows: bool,
}

impl CommandExecutionOptions {
    /// Creates execution options from negotiated classic-protocol capabilities.
    pub const fn from_capability_flags(capability_flags: u32) -> Self {
        Self {
            client_found_rows: capability_flags & CLIENT_FOUND_ROWS != 0,
        }
    }

    /// Returns whether affected-row counts must include matched unchanged rows.
    pub const fn client_found_rows(self) -> bool {
        self.client_found_rows
    }
}

/// Injection point for query and default-database execution.
pub trait CommandExecutor {
    /// Returns the current MySQL server status flags for connection-level responses.
    fn status_flags(&self) -> u16 {
        SERVER_STATUS_AUTOCOMMIT
    }

    /// Executes `COM_INIT_DB` without owning the borrowed database text.
    fn execute_init_db(
        &mut self,
        database: &str,
    ) -> Result<CommandExecutionResult, FrontendErrorKind>;

    /// Executes `COM_QUERY` without owning the borrowed query text.
    fn execute_query(&mut self, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind>;

    /// Resets the authenticated connection session.
    fn execute_reset_connection(&mut self) -> Result<(), FrontendErrorKind> {
        Err(FrontendErrorKind::Unsupported)
    }

    /// Prepares and retains one checked statement on this connection.
    fn execute_stmt_prepare(
        &mut self,
        _sql: &str,
    ) -> Result<PreparedStatementResult, FrontendErrorKind> {
        Err(FrontendErrorKind::Unsupported)
    }

    /// Closes one retained statement. `COM_STMT_CLOSE` has no response packet.
    fn execute_stmt_close(&mut self, _statement_id: u32) {}

    /// Removes a statement when its prepare response cannot be encoded.
    fn rollback_stmt_prepare(&mut self, statement_id: u32) {
        self.execute_stmt_close(statement_id);
    }

    /// Resets one retained statement and clears its parameter bindings.
    fn execute_stmt_reset(&mut self, _statement_id: u32) -> Result<(), FrontendErrorKind> {
        Err(FrontendErrorKind::Unsupported)
    }

    /// Appends one long-data chunk to a retained prepared-statement parameter.
    fn execute_stmt_send_long_data(
        &mut self,
        _statement_id: u32,
        _parameter_id: u16,
        _data: &[u8],
    ) {
    }

    /// Executes one retained prepared statement from its binary parameter payload.
    fn execute_stmt_execute(
        &mut self,
        _statement_id: u32,
        _parameter_payload: &[u8],
    ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
        Err(FrontendErrorKind::Unsupported)
    }
}

/// Compatibility name for the command execution port.
pub use CommandExecutor as CommandExecutionPort;

/// A stateless dispatcher for one ready classic connection.
#[derive(Debug, Default, Clone, Copy)]
pub struct CommandDispatcher;

impl CommandDispatcher {
    /// Creates a transport-neutral command dispatcher.
    pub const fn new() -> Self {
        Self
    }

    /// Decodes and dispatches one command packet.
    ///
    /// A successful return contains zero frames for commands that have no
    /// protocol response. Other successful commands return at least one server
    /// frame beginning at sequence ID one.
    pub fn dispatch<E: CommandExecutor + ?Sized>(
        &self,
        connection: &mut ClassicConnection,
        executor: &mut E,
        frame: &[u8],
    ) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
        let command = match connection.receive_command_frame(frame) {
            Ok(command) => command,
            Err(ConnectionStateError::Command(error)) if is_unsupported_command(&error) => {
                let capabilities = negotiated_capabilities(connection)?;
                return close_on_response_error(
                    connection,
                    encode_frontend_error(
                        connection.response_packet_codec(),
                        capabilities,
                        FrontendErrorKind::Unsupported,
                    ),
                );
            }
            Err(ConnectionStateError::Command(error)) if is_recoverable_command_error(&error) => {
                let capabilities = negotiated_capabilities(connection)?;
                return close_on_response_error(
                    connection,
                    encode_frontend_error(
                        connection.response_packet_codec(),
                        capabilities,
                        FrontendErrorKind::Syntax,
                    ),
                );
            }
            Err(ConnectionStateError::Command(error)) => {
                connection.begin_close()?;
                return Err(CommandDispatcherError::Connection(
                    ConnectionStateError::Command(error),
                ));
            }
            Err(error) => return Err(CommandDispatcherError::Connection(error)),
        };

        match command.command {
            ClassicCommand::Ping => {
                let capabilities = negotiated_capabilities(connection)?;
                close_on_response_error(
                    connection,
                    encode_ok(
                        connection.response_packet_codec(),
                        capabilities,
                        CommandOkResult {
                            status_flags: executor.status_flags(),
                            ..CommandOkResult::default()
                        },
                    ),
                )
            }
            ClassicCommand::Quit => Ok(Vec::new()),
            ClassicCommand::InitDb { database } => {
                let capabilities = negotiated_capabilities(connection)?;
                close_on_response_error(
                    connection,
                    encode_execution_result(
                        connection.response_packet_codec(),
                        capabilities,
                        executor.execute_init_db(database),
                    ),
                )
            }
            ClassicCommand::Query { sql } => {
                let capabilities = negotiated_capabilities(connection)?;
                close_on_response_error(
                    connection,
                    encode_execution_result(
                        connection.response_packet_codec(),
                        capabilities,
                        executor.execute_query(sql),
                    ),
                )
            }
            ClassicCommand::ResetConnection => {
                let capabilities = negotiated_capabilities(connection)?;
                let result = executor.execute_reset_connection().map(|()| {
                    CommandExecutionResult::Ok(CommandOkResult {
                        status_flags: executor.status_flags(),
                        ..CommandOkResult::default()
                    })
                });
                close_on_response_error(
                    connection,
                    encode_execution_result(
                        connection.response_packet_codec(),
                        capabilities,
                        result,
                    ),
                )
            }
            ClassicCommand::StmtPrepare { sql } => {
                let capabilities = negotiated_capabilities(connection)?;
                let prepared = executor.execute_stmt_prepare(sql);
                let statement_id = prepared.as_ref().ok().map(|result| result.statement_id);
                let encoded = encode_prepared_statement(
                    connection.response_packet_codec(),
                    prepared,
                    capabilities,
                );
                if encoded.is_err() {
                    if let Some(statement_id) = statement_id {
                        executor.rollback_stmt_prepare(statement_id);
                    }
                }
                close_on_response_error(connection, encoded)
            }
            ClassicCommand::StmtClose { statement_id } => {
                executor.execute_stmt_close(statement_id);
                Ok(Vec::new())
            }
            ClassicCommand::StmtReset { statement_id } => {
                let capabilities = negotiated_capabilities(connection)?;
                let result = executor.execute_stmt_reset(statement_id).map(|()| {
                    CommandExecutionResult::Ok(CommandOkResult {
                        status_flags: executor.status_flags(),
                        ..CommandOkResult::default()
                    })
                });
                close_on_response_error(
                    connection,
                    encode_execution_result(
                        connection.response_packet_codec(),
                        capabilities,
                        result,
                    ),
                )
            }
            ClassicCommand::StmtSendLongData {
                statement_id,
                parameter_id,
                data,
            } => {
                executor.execute_stmt_send_long_data(statement_id, parameter_id, data);
                Ok(Vec::new())
            }
            ClassicCommand::StmtExecute {
                statement_id,
                parameter_payload,
                ..
            } => {
                let capabilities = negotiated_capabilities(connection)?;
                close_on_response_error(
                    connection,
                    encode_prepared_execution_result(
                        connection.response_packet_codec(),
                        capabilities,
                        executor.execute_stmt_execute(statement_id, parameter_payload),
                    ),
                )
            }
        }
    }
}

/// Dispatches one command with the default stateless dispatcher.
pub fn dispatch_command_frame<E: CommandExecutor + ?Sized>(
    connection: &mut ClassicConnection,
    executor: &mut E,
    frame: &[u8],
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    CommandDispatcher::new().dispatch(connection, executor, frame)
}

/// Errors returned by command dispatch and response encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandDispatcherError {
    /// The connection rejected the command or is not ready.
    Connection(ConnectionStateError),
    /// A response packet could not be encoded.
    Response(ResponsePacketError),
    /// A ready connection did not retain negotiated capabilities.
    NegotiatedCapabilitiesRequired,
    /// The execution port returned more rows than the dispatcher retains.
    ResultSetTooLarge { rows: usize, limit: usize },
    /// A result row did not contain one value for every result column.
    ResultRowShape {
        row: usize,
        expected_columns: usize,
        actual_values: usize,
    },
}

impl From<ConnectionStateError> for CommandDispatcherError {
    fn from(error: ConnectionStateError) -> Self {
        Self::Connection(error)
    }
}

impl From<ResponsePacketError> for CommandDispatcherError {
    fn from(error: ResponsePacketError) -> Self {
        Self::Response(error)
    }
}

impl fmt::Display for CommandDispatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(error) => write!(f, "connection rejected command: {error}"),
            Self::Response(error) => write!(f, "response encoding failed: {error}"),
            Self::NegotiatedCapabilitiesRequired => {
                f.write_str("ready connection has no negotiated capabilities")
            }
            Self::ResultSetTooLarge { rows, limit } => {
                write!(f, "result set has {rows} rows, limit is {limit}")
            }
            Self::ResultRowShape {
                row,
                expected_columns,
                actual_values,
            } => write!(
                f,
                "result row {row} has {actual_values} values, expected {expected_columns}"
            ),
        }
    }
}

impl Error for CommandDispatcherError {}

fn is_unsupported_command(error: &CommandPacketError) -> bool {
    matches!(error, CommandPacketError::UnsupportedCommand { .. })
}

fn is_recoverable_command_error(error: &CommandPacketError) -> bool {
    matches!(
        error,
        CommandPacketError::EmptyPayload
            | CommandPacketError::PayloadTooLarge { .. }
            | CommandPacketError::InvalidPayloadLength { .. }
            | CommandPacketError::EmptyText { .. }
            | CommandPacketError::EmbeddedNul { .. }
            | CommandPacketError::InvalidUtf8 { .. }
    )
}

fn negotiated_capabilities(connection: &ClassicConnection) -> Result<u32, CommandDispatcherError> {
    connection
        .negotiated_capabilities()
        .ok_or(CommandDispatcherError::NegotiatedCapabilitiesRequired)
}

fn close_on_response_error(
    connection: &mut ClassicConnection,
    result: Result<Vec<Vec<u8>>, CommandDispatcherError>,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    match result {
        Ok(frames) => Ok(frames),
        Err(error) => {
            if let Err(close_error) = connection.begin_close() {
                return Err(CommandDispatcherError::Connection(close_error));
            }
            Err(error)
        }
    }
}

fn encode_result(
    codec: PacketCodec,
    capability_flags: u32,
    result: CommandExecutionResult,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    match result {
        CommandExecutionResult::Ok(result) => encode_ok(codec, capability_flags, result),
        CommandExecutionResult::ResultSet(result) => {
            encode_result_set(codec, capability_flags, result)
        }
    }
}

fn encode_execution_result(
    codec: PacketCodec,
    capability_flags: u32,
    result: Result<CommandExecutionResult, FrontendErrorKind>,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    match result {
        Ok(result) => encode_result(codec, capability_flags, result),
        Err(kind) => encode_frontend_error(codec, capability_flags, kind),
    }
}

fn encode_prepared_execution_result(
    codec: PacketCodec,
    capability_flags: u32,
    result: Result<PreparedStatementExecutionResult, FrontendErrorKind>,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    match result {
        Ok(PreparedStatementExecutionResult::ResultSet(result)) => {
            encode_binary_result_set(codec, capability_flags, result)
        }
        Ok(PreparedStatementExecutionResult::Ok(result)) => {
            encode_ok(codec, capability_flags, result)
        }
        Err(kind) => encode_frontend_error(codec, capability_flags, kind),
    }
}

fn encode_prepared_statement(
    codec: PacketCodec,
    result: Result<PreparedStatementResult, FrontendErrorKind>,
    capability_flags: u32,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    let result = match result {
        Ok(result) => result,
        Err(kind) => return encode_frontend_error(codec, capability_flags, kind),
    };
    let num_columns = u16::try_from(result.columns.len()).map_err(|_| {
        CommandDispatcherError::ResultSetTooLarge {
            rows: result.columns.len(),
            limit: u16::MAX as usize,
        }
    })?;
    let num_params = u16::try_from(result.parameters.len()).map_err(|_| {
        CommandDispatcherError::ResultSetTooLarge {
            rows: result.parameters.len(),
            limit: u16::MAX as usize,
        }
    })?;
    let mut sequence = PacketSequence::new(SERVER_RESPONSE_SEQUENCE_ID);
    let mut frames = Vec::with_capacity(
        1 + result.parameters.len()
            + usize::from(num_params > 0)
            + result.columns.len()
            + usize::from(num_columns > 0),
    );
    frames.push(
        StmtPrepareOkPacketConfig::new(
            result.statement_id,
            num_columns,
            num_params,
            result.warnings,
        )
        .encode(codec, sequence.next_sequence_id())?,
    );
    for parameter in &result.parameters {
        frames.push(parameter.encode(codec, sequence.next_sequence_id())?);
    }
    if num_params > 0 && capability_flags & CLIENT_DEPRECATE_EOF == 0 {
        frames.push(EofPacket::encode(
            codec,
            sequence.next_sequence_id(),
            result.warnings,
            result.status_flags,
        )?);
    }
    for column in &result.columns {
        frames.push(column.encode(codec, sequence.next_sequence_id())?);
    }
    if num_columns > 0 && capability_flags & CLIENT_DEPRECATE_EOF == 0 {
        frames.push(EofPacket::encode(
            codec,
            sequence.next_sequence_id(),
            result.warnings,
            result.status_flags,
        )?);
    }
    Ok(frames)
}

fn encode_ok(
    codec: PacketCodec,
    _capability_flags: u32,
    result: CommandOkResult,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    let mut config = OkPacketConfig::new(
        result.affected_rows,
        result.last_insert_id,
        result.status_flags,
        result.warnings,
    );
    config.info = result.info;
    let mut sequence = PacketSequence::new(SERVER_RESPONSE_SEQUENCE_ID);
    Ok(vec![config.encode(codec, sequence.next_sequence_id())?])
}

fn encode_frontend_error(
    codec: PacketCodec,
    capability_flags: u32,
    kind: FrontendErrorKind,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    let mut sequence = PacketSequence::new(SERVER_RESPONSE_SEQUENCE_ID);
    let config = map_frontend_error(kind);
    Ok(vec![config.encode(
        codec,
        sequence.next_sequence_id(),
        capability_flags,
    )?])
}

fn encode_result_set(
    codec: PacketCodec,
    capability_flags: u32,
    result: TextResultSet,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    let TextResultSet {
        columns,
        rows,
        warnings,
        status_flags,
    } = result;
    if rows.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(CommandDispatcherError::ResultSetTooLarge {
            rows: rows.len(),
            limit: MAX_DISPATCH_RESULT_ROWS,
        });
    }
    for (row, values) in rows.iter().enumerate() {
        if values.len() != columns.len() {
            return Err(CommandDispatcherError::ResultRowShape {
                row,
                expected_columns: columns.len(),
                actual_values: values.len(),
            });
        }
    }

    let mut sequence = PacketSequence::new(SERVER_RESPONSE_SEQUENCE_ID);
    let mut frames = Vec::with_capacity(2 + columns.len() + rows.len());
    frames.push(ColumnCountPacket::encode(
        codec,
        sequence.next_sequence_id(),
        columns.len(),
    )?);
    for column in &columns {
        frames.push(column.encode(codec, sequence.next_sequence_id())?);
    }

    if capability_flags & CLIENT_DEPRECATE_EOF == 0 {
        frames.push(ResultTerminatorPacket::encode(
            codec,
            sequence.next_sequence_id(),
            capability_flags,
            warnings,
            status_flags,
        )?);
    }

    for row in rows {
        let values = row
            .iter()
            .map(|value| match value {
                Some(bytes) => TextRowValue::Bytes(bytes),
                None => TextRowValue::Null,
            })
            .collect::<Vec<_>>();
        frames.push(TextRowPacket::encode(
            codec,
            sequence.next_sequence_id(),
            &values,
        )?);
    }

    frames.push(ResultTerminatorPacket::encode(
        codec,
        sequence.next_sequence_id(),
        capability_flags,
        warnings,
        status_flags,
    )?);
    Ok(frames)
}

fn encode_binary_result_set(
    codec: PacketCodec,
    capability_flags: u32,
    result: BinaryResultSet,
) -> Result<Vec<Vec<u8>>, CommandDispatcherError> {
    let BinaryResultSet {
        columns,
        rows,
        warnings,
        status_flags,
    } = result;
    if rows.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(CommandDispatcherError::ResultSetTooLarge {
            rows: rows.len(),
            limit: MAX_DISPATCH_RESULT_ROWS,
        });
    }
    for (row, values) in rows.iter().enumerate() {
        if values.len() != columns.len() {
            return Err(CommandDispatcherError::ResultRowShape {
                row,
                expected_columns: columns.len(),
                actual_values: values.len(),
            });
        }
    }
    let mut sequence = PacketSequence::new(SERVER_RESPONSE_SEQUENCE_ID);
    let mut frames = Vec::with_capacity(2 + columns.len() + rows.len());
    frames.push(ColumnCountPacket::encode(
        codec,
        sequence.next_sequence_id(),
        columns.len(),
    )?);
    for column in &columns {
        frames.push(column.encode(codec, sequence.next_sequence_id())?);
    }
    if capability_flags & CLIENT_DEPRECATE_EOF == 0 {
        frames.push(EofPacket::encode(
            codec,
            sequence.next_sequence_id(),
            warnings,
            status_flags,
        )?);
    }
    for row in &rows {
        let values = row
            .iter()
            .map(|value| match value {
                BinaryResultValue::Null => BinaryRowValue::Null,
                BinaryResultValue::Integer(value) => BinaryRowValue::Int64(*value),
                BinaryResultValue::Real(value) => BinaryRowValue::Float64(*value),
                BinaryResultValue::Text(value) => BinaryRowValue::String(value),
                BinaryResultValue::Blob(value) => BinaryRowValue::Bytes(value),
            })
            .collect::<Vec<_>>();
        frames.push(BinaryRowPacket::encode(
            codec,
            sequence.next_sequence_id(),
            &values,
        )?);
    }
    frames.push(ResultTerminatorPacket::encode(
        codec,
        sequence.next_sequence_id(),
        capability_flags,
        warnings,
        status_flags,
    )?);
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AuthOkPacket, ClientHandshakeResponseConfig, ConnectionState, InitialHandshakeSettings,
        PacketCodec, ResultTerminatorPacket, TransportSecurity, CACHING_SHA2_PASSWORD_PLUGIN,
        CLIENT_DEPRECATE_EOF, CLIENT_FOUND_ROWS, REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES,
    };

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: 4096,
    };

    #[derive(Debug, Default)]
    struct TestExecutor {
        status_flags: u16,
        init_db_calls: Vec<String>,
        query_calls: Vec<String>,
        reset_connection_calls: usize,
        prepare_calls: Vec<String>,
        close_calls: Vec<u32>,
        reset_calls: Vec<u32>,
        long_data_calls: Vec<(u32, u16, Vec<u8>)>,
        init_db_result: Option<Result<CommandExecutionResult, FrontendErrorKind>>,
        query_result: Option<Result<CommandExecutionResult, FrontendErrorKind>>,
        reset_connection_result: Option<Result<(), FrontendErrorKind>>,
        prepare_result: Option<Result<PreparedStatementResult, FrontendErrorKind>>,
        reset_result: Option<Result<(), FrontendErrorKind>>,
        execute_result: Option<Result<PreparedStatementExecutionResult, FrontendErrorKind>>,
        execute_calls: Vec<(u32, Vec<u8>)>,
    }

    impl CommandExecutor for TestExecutor {
        fn status_flags(&self) -> u16 {
            if self.status_flags == 0 {
                SERVER_STATUS_AUTOCOMMIT
            } else {
                self.status_flags
            }
        }

        fn execute_init_db(
            &mut self,
            database: &str,
        ) -> Result<CommandExecutionResult, FrontendErrorKind> {
            self.init_db_calls.push(database.to_owned());
            self.init_db_result
                .take()
                .unwrap_or_else(|| Ok(CommandExecutionResult::Ok(CommandOkResult::default())))
        }

        fn execute_query(
            &mut self,
            sql: &str,
        ) -> Result<CommandExecutionResult, FrontendErrorKind> {
            self.query_calls.push(sql.to_owned());
            self.query_result
                .take()
                .unwrap_or_else(|| Ok(CommandExecutionResult::Ok(CommandOkResult::default())))
        }

        fn execute_reset_connection(&mut self) -> Result<(), FrontendErrorKind> {
            self.reset_connection_calls += 1;
            let result = self.reset_connection_result.take().unwrap_or(Ok(()));
            if result.is_ok() {
                self.status_flags = SERVER_STATUS_AUTOCOMMIT;
            }
            result
        }

        fn execute_stmt_prepare(
            &mut self,
            sql: &str,
        ) -> Result<PreparedStatementResult, FrontendErrorKind> {
            self.prepare_calls.push(sql.to_owned());
            self.prepare_result
                .take()
                .unwrap_or(Err(FrontendErrorKind::Unsupported))
        }

        fn execute_stmt_close(&mut self, statement_id: u32) {
            self.close_calls.push(statement_id);
        }

        fn execute_stmt_reset(&mut self, statement_id: u32) -> Result<(), FrontendErrorKind> {
            self.reset_calls.push(statement_id);
            self.reset_result.take().unwrap_or(Ok(()))
        }

        fn execute_stmt_send_long_data(
            &mut self,
            statement_id: u32,
            parameter_id: u16,
            data: &[u8],
        ) {
            self.long_data_calls
                .push((statement_id, parameter_id, data.to_vec()));
        }

        fn execute_stmt_execute(
            &mut self,
            statement_id: u32,
            parameter_payload: &[u8],
        ) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
            self.execute_calls
                .push((statement_id, parameter_payload.to_vec()));
            self.execute_result
                .take()
                .unwrap_or(Err(FrontendErrorKind::Unsupported))
        }
    }

    fn server_config(capability_flags: u32) -> InitialHandshakeSettings {
        InitialHandshakeSettings {
            capability_flags,
            ..InitialHandshakeSettings::default()
        }
    }

    #[test]
    fn execution_options_follow_the_negotiated_found_rows_capability() {
        let without_found_rows = CommandExecutionOptions::from_capability_flags(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        );
        assert!(!without_found_rows.client_found_rows());

        let with_found_rows = CommandExecutionOptions::from_capability_flags(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_FOUND_ROWS,
        );
        assert!(with_found_rows.client_found_rows());
    }

    fn ready_connection(capability_flags: u32) -> ClassicConnection {
        ready_connection_with_max_packet_size(capability_flags, 0)
    }

    fn ready_connection_with_max_packet_size(
        capability_flags: u32,
        max_packet_size: u32,
    ) -> ClassicConnection {
        let mut connection = ClassicConnection::with_test_nonce(
            server_config(capability_flags),
            CODEC,
            TransportSecurity::Secure,
            [0xa5; crate::AUTH_PLUGIN_DATA_LENGTH],
        )
        .unwrap();
        connection.send_initial_handshake().unwrap();
        let response = ClientHandshakeResponseConfig::new(
            capability_flags,
            max_packet_size,
            crate::DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(CODEC, 1)
        .unwrap();
        connection
            .receive_client_handshake_frame(&response)
            .unwrap();
        connection
            .apply_initial_authentication_result(
                crate::InitialAuthenticationResult::FastAuthSuccess,
            )
            .unwrap();
        connection.send_authentication_ok().unwrap();
        connection
    }

    fn command(command: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(1 + body.len());
        payload.push(command);
        payload.extend_from_slice(body);
        CODEC.encode(COMMAND_SEQUENCE_ID, &payload).unwrap()
    }

    #[test]
    fn ping_returns_ok_and_quit_closes_without_a_response() {
        let mut connection = ready_connection_with_max_packet_size(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            crate::MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
        );
        let mut executor = TestExecutor::default();
        let dispatcher = CommandDispatcher::new();

        let ping = dispatcher
            .dispatch(
                &mut connection,
                &mut executor,
                &command(crate::COM_PING, &[]),
            )
            .unwrap();
        assert_eq!(ping.len(), 1);
        assert!(
            CODEC.decode(&ping[0]).unwrap().payload.len()
                <= crate::MIN_SERVER_RESPONSE_PAYLOAD_LENGTH as usize
        );
        assert_eq!(
            AuthOkPacket::decode(CODEC, &ping[0]).unwrap().sequence_id,
            1
        );
        assert!(executor.init_db_calls.is_empty());
        assert!(executor.query_calls.is_empty());

        let quit = dispatcher
            .dispatch(
                &mut connection,
                &mut executor,
                &command(crate::COM_QUIT, &[]),
            )
            .unwrap();
        assert!(quit.is_empty());
        assert_eq!(connection.state(), ConnectionState::Closing);
    }

    #[test]
    fn ping_reports_the_executors_current_transaction_state() {
        let mut connection = ready_connection(REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES);
        let mut executor = TestExecutor {
            status_flags: SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT,
            ..TestExecutor::default()
        };

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_PING, &[]),
        )
        .unwrap();

        assert_eq!(
            AuthOkPacket::decode(CODEC, &frames[0])
                .unwrap()
                .status_flags,
            SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
        );
    }

    #[test]
    fn init_db_uses_executor_and_returns_typed_ok() {
        let mut connection = ready_connection(REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES);
        let mut executor = TestExecutor {
            init_db_result: Some(Ok(CommandExecutionResult::Ok(CommandOkResult {
                affected_rows: 3,
                ..CommandOkResult::default()
            }))),
            ..TestExecutor::default()
        };
        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_INIT_DB, b"analytics"),
        )
        .unwrap();
        let ok = AuthOkPacket::decode(CODEC, &frames[0]).unwrap();
        assert_eq!(ok.sequence_id, 1);
        assert_eq!(ok.affected_rows, 3);
        assert_eq!(executor.init_db_calls, vec![String::from("analytics")]);
    }

    #[test]
    fn query_result_set_sequences_rows_and_uses_deprecated_eof_rules() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_DEPRECATE_EOF;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            query_result: Some(Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![
                    ColumnDefinitionConfig::new("value", 0xfd),
                    ColumnDefinitionConfig::new("optional", 0xfd),
                ],
                rows: vec![vec![Some(vec![0xff, 0]), None]],
                warnings: 0,
                status_flags: 2,
            }))),
            ..TestExecutor::default()
        };
        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_QUERY, b"select value"),
        )
        .unwrap();
        assert_eq!(executor.query_calls, vec![String::from("select value")]);
        assert_eq!(frames.len(), 5);
        let sequence_ids = frames
            .iter()
            .map(|frame| CODEC.decode(frame).unwrap().sequence_id)
            .collect::<Vec<_>>();
        assert_eq!(sequence_ids, [1, 2, 3, 4, 5]);
        assert_eq!(
            ColumnCountPacket::decode(CODEC, &frames[0])
                .unwrap()
                .column_count,
            2
        );
        let row = TextRowPacket::decode(CODEC, &frames[3], 2).unwrap();
        assert_eq!(
            row.values,
            [TextRowValue::Bytes(&[0xff, 0]), TextRowValue::Null]
        );
        assert!(matches!(
            ResultTerminatorPacket::decode(CODEC, &frames[4], capabilities).unwrap(),
            ResultTerminatorPacket::Ok(packet)
                if packet.header == crate::RESPONSE_OK_TERMINATOR_HEADER
        ));
    }

    #[test]
    fn statement_prepare_omits_metadata_eof_when_deprecated() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_DEPRECATE_EOF;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            prepare_result: Some(Ok(PreparedStatementResult {
                statement_id: 7,
                parameters: vec![ColumnDefinitionConfig::new("?", 0x06)],
                columns: vec![ColumnDefinitionConfig::new("value", 0x08)],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            })),
            ..TestExecutor::default()
        };

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_PREPARE, b"SELECT ? AS value"),
        )
        .unwrap();

        assert_eq!(executor.prepare_calls, ["SELECT ? AS value"]);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames
                .iter()
                .map(|frame| CODEC.decode(frame).unwrap().sequence_id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let header = crate::StmtPrepareOkPacket::decode(CODEC, &frames[0]).unwrap();
        assert_eq!(header.statement_id, 7);
        assert_eq!((header.num_params, header.num_columns), (1, 1));
        assert_eq!(
            crate::ColumnDefinitionPacket::decode(CODEC, &frames[1])
                .unwrap()
                .name,
            "?"
        );
        assert_eq!(
            crate::ColumnDefinitionPacket::decode(CODEC, &frames[2])
                .unwrap()
                .name,
            "value"
        );
    }

    #[test]
    fn statement_prepare_uses_metadata_eof_for_legacy_clients() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            prepare_result: Some(Ok(PreparedStatementResult {
                statement_id: 7,
                parameters: vec![ColumnDefinitionConfig::new("?", 0x06)],
                columns: vec![ColumnDefinitionConfig::new("value", 0x08)],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            })),
            ..TestExecutor::default()
        };

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_PREPARE, b"SELECT ? AS value"),
        )
        .unwrap();

        assert_eq!(frames.len(), 5);
        assert_eq!(
            crate::EofPacket::decode(CODEC, &frames[2])
                .unwrap()
                .sequence_id,
            3
        );
        assert_eq!(
            crate::EofPacket::decode(CODEC, &frames[4])
                .unwrap()
                .sequence_id,
            5
        );
    }

    #[test]
    fn failed_prepare_response_removes_the_unreported_statement() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            prepare_result: Some(Ok(PreparedStatementResult {
                statement_id: 19,
                parameters: Vec::new(),
                columns: vec![ColumnDefinitionConfig::new(
                    "x".repeat(crate::MAX_COLUMN_TEXT_LENGTH + 1),
                    0x08,
                )],
                warnings: 0,
                status_flags: SERVER_STATUS_AUTOCOMMIT,
            })),
            ..TestExecutor::default()
        };

        let error = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_PREPARE, b"SELECT 1"),
        )
        .unwrap_err();

        assert!(matches!(error, CommandDispatcherError::Response(_)));
        assert_eq!(executor.close_calls, [19]);
        assert_eq!(connection.state(), ConnectionState::Closing);
    }

    #[test]
    fn statement_close_has_no_response_and_reset_returns_ok_or_unknown_id() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor::default();

        let close = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_CLOSE, &7u32.to_le_bytes()),
        )
        .unwrap();
        assert!(close.is_empty());
        assert_eq!(executor.close_calls, [7]);

        let reset = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_RESET, &9u32.to_le_bytes()),
        )
        .unwrap();
        assert_eq!(executor.reset_calls, [9]);
        assert_eq!(
            crate::OkPacket::decode(CODEC, &reset[0])
                .unwrap()
                .sequence_id,
            1
        );

        executor.reset_result = Some(Err(FrontendErrorKind::UnknownPreparedStatement));
        let unknown = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_RESET, &11u32.to_le_bytes()),
        )
        .unwrap();
        let error = crate::ErrPacket::decode(CODEC, &unknown[0], capabilities).unwrap();
        assert_eq!(error.error_code, 1243);
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn connection_reset_returns_ok_and_preserves_ready_state() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            status_flags: SERVER_STATUS_IN_TRANS,
            ..TestExecutor::default()
        };

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_RESET_CONNECTION, &[]),
        )
        .unwrap();

        assert_eq!(executor.reset_connection_calls, 1);
        let ok = crate::OkPacket::decode(CODEC, &frames[0]).unwrap();
        assert_eq!(ok.sequence_id, SERVER_RESPONSE_SEQUENCE_ID);
        assert_eq!(ok.status_flags, SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(connection.state(), ConnectionState::Ready);

        executor.reset_connection_result = Some(Err(FrontendErrorKind::Unsupported));
        let error = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_RESET_CONNECTION, &[]),
        )
        .unwrap();
        assert_eq!(executor.reset_connection_calls, 2);
        assert_eq!(
            crate::ErrPacket::decode(CODEC, &error[0], capabilities)
                .unwrap()
                .error_code,
            1235
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn statement_long_data_is_forwarded_as_binary_and_has_no_response() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor::default();
        let mut body = Vec::new();
        body.extend_from_slice(&0x0102_0304u32.to_le_bytes());
        body.extend_from_slice(&0x0506u16.to_le_bytes());
        body.extend_from_slice(&[0, 0xff, 0x41]);

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_SEND_LONG_DATA, &body),
        )
        .unwrap();

        assert!(frames.is_empty());
        assert_eq!(
            executor.long_data_calls,
            [(0x0102_0304, 0x0506, vec![0, 0xff, 0x41])]
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn statement_execute_returns_binary_rows_in_protocol_sequence() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_DEPRECATE_EOF;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            execute_result: Some(Ok(PreparedStatementExecutionResult::ResultSet(
                BinaryResultSet {
                    columns: vec![ColumnDefinitionConfig::new("value", 0x08)],
                    rows: vec![vec![BinaryResultValue::Integer(42)]],
                    warnings: 0,
                    status_flags: SERVER_STATUS_AUTOCOMMIT,
                },
            ))),
            ..TestExecutor::default()
        };
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_le_bytes());
        body.push(crate::CURSOR_TYPE_NO_CURSOR);
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&[0, 1, 0x08, 0, 42, 0, 0, 0, 0, 0, 0, 0]);

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_EXECUTE, &body),
        )
        .unwrap();

        assert_eq!(executor.execute_calls, [(7, body[9..].to_vec())]);
        assert_eq!(frames.len(), 4);
        assert_eq!(
            frames
                .iter()
                .map(|frame| CODEC.decode(frame).unwrap().sequence_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            BinaryRowPacket::decode(CODEC, &frames[2], &[crate::BinaryRowColumnType::Int64])
                .unwrap()
                .values,
            [BinaryRowValue::Int64(42)]
        );
    }

    #[test]
    fn statement_execute_returns_one_ok_packet_for_prepared_writes() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            execute_result: Some(Ok(PreparedStatementExecutionResult::Ok(CommandOkResult {
                affected_rows: 2,
                last_insert_id: 9,
                status_flags: SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT,
                warnings: 3,
                info: b"updated".to_vec(),
            }))),
            ..TestExecutor::default()
        };
        let mut body = Vec::new();
        body.extend_from_slice(&11u32.to_le_bytes());
        body.push(crate::CURSOR_TYPE_NO_CURSOR);
        body.extend_from_slice(&1u32.to_le_bytes());

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_EXECUTE, &body),
        )
        .unwrap();

        assert_eq!(executor.execute_calls, [(11, Vec::new())]);
        assert_eq!(frames.len(), 1);
        let ok = crate::OkPacket::decode(CODEC, &frames[0]).unwrap();
        assert_eq!(ok.sequence_id, SERVER_RESPONSE_SEQUENCE_ID);
        assert_eq!(ok.affected_rows, 2);
        assert_eq!(ok.last_insert_id, 9);
        assert_eq!(
            ok.status_flags,
            SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
        );
        assert_eq!(ok.warnings, 3);
        assert_eq!(ok.info, b"updated");
    }

    #[test]
    fn statement_execute_error_returns_one_err_packet_and_keeps_connection_ready() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            execute_result: Some(Err(FrontendErrorKind::ConstraintViolation)),
            ..TestExecutor::default()
        };
        let mut body = Vec::new();
        body.extend_from_slice(&12u32.to_le_bytes());
        body.push(crate::CURSOR_TYPE_NO_CURSOR);
        body.extend_from_slice(&1u32.to_le_bytes());

        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_STMT_EXECUTE, &body),
        )
        .unwrap();

        assert_eq!(executor.execute_calls, [(12, Vec::new())]);
        assert_eq!(frames.len(), 1);
        let error = crate::ErrPacket::decode(CODEC, &frames[0], capabilities).unwrap();
        assert_eq!(error.sequence_id, SERVER_RESPONSE_SEQUENCE_ID);
        assert_eq!(error.error_code, 1062);
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn query_error_maps_to_typed_err_packet() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            query_result: Some(Err(FrontendErrorKind::ConstraintViolation)),
            ..TestExecutor::default()
        };
        let error = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_QUERY, b"insert"),
        )
        .unwrap();
        let decoded = crate::ErrPacket::decode(CODEC, &error[0], capabilities).unwrap();
        assert_eq!(decoded.error_code, 1062);
        assert_eq!(decoded.sql_state, Some(*b"23000"));

        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn malformed_utf8_query_gets_syntax_err_and_connection_stays_ready() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor::default();
        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_QUERY, &[0xff]),
        )
        .unwrap();
        let decoded = crate::ErrPacket::decode(CODEC, &frames[0], capabilities).unwrap();
        assert_eq!(decoded.error_code, 1064);
        assert_eq!(connection.state(), ConnectionState::Ready);
        assert!(executor.query_calls.is_empty());
    }

    #[test]
    fn minimum_negotiated_packet_limit_still_fits_syntax_errors() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection_with_max_packet_size(
            capabilities,
            crate::MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
        );
        let mut executor = TestExecutor::default();
        let frames = dispatch_command_frame(
            &mut connection,
            &mut executor,
            &command(crate::COM_QUERY, &[0xff]),
        )
        .unwrap();
        let decoded = crate::ErrPacket::decode(CODEC, &frames[0], capabilities).unwrap();
        assert_eq!(decoded.error_code, 1064);
        assert_eq!(connection.state(), ConnectionState::Ready);
        assert!(executor.query_calls.is_empty());
    }

    #[test]
    fn oversized_resultset_response_closes_the_connection() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut oversized_result = ready_connection_with_max_packet_size(
            capabilities,
            crate::MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
        );
        let mut executor = TestExecutor {
            query_result: Some(Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![ColumnDefinitionConfig::new(
                    "x".repeat(crate::MAX_RESPONSE_PACKET_PAYLOAD_LENGTH),
                    0xfd,
                )],
                rows: Vec::new(),
                warnings: 0,
                status_flags: 2,
            }))),
            ..TestExecutor::default()
        };
        assert!(matches!(
            dispatch_command_frame(
                &mut oversized_result,
                &mut executor,
                &command(crate::COM_QUERY, b"select 1"),
            ),
            Err(CommandDispatcherError::Response(_))
        ));
        assert_eq!(oversized_result.state(), ConnectionState::Closing);
    }

    #[test]
    fn unframed_command_error_closes_when_an_err_cannot_be_safely_returned() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor::default();
        let wrong_sequence = CODEC.encode(1, &[crate::COM_PING]).unwrap();
        assert!(matches!(
            dispatch_command_frame(&mut connection, &mut executor, &wrong_sequence),
            Err(CommandDispatcherError::Connection(
                ConnectionStateError::Command(CommandPacketError::UnexpectedSequenceId {
                    expected: 0,
                    actual: 1,
                })
            ))
        ));
        assert_eq!(connection.state(), ConnectionState::Closing);
    }

    #[test]
    fn dispatcher_rejects_commands_before_ready_without_calling_executor() {
        let mut connection =
            ClassicConnection::new(server_config(REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES)).unwrap();
        let mut executor = TestExecutor::default();
        let error = CommandDispatcher::new().dispatch(
            &mut connection,
            &mut executor,
            &command(crate::COM_QUERY, b"select 1"),
        );
        assert!(matches!(
            error,
            Err(CommandDispatcherError::Connection(
                ConnectionStateError::CommandBeforeReady {
                    state: ConnectionState::SendInitialHandshake
                }
            ))
        ));
        assert!(executor.query_calls.is_empty());
    }

    #[test]
    fn malformed_result_rows_do_not_reach_the_packet_codec() {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ready_connection(capabilities);
        let mut executor = TestExecutor {
            query_result: Some(Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![ColumnDefinitionConfig::new("value", 0xfd)],
                rows: vec![Vec::new()],
                warnings: 0,
                status_flags: 2,
            }))),
            ..TestExecutor::default()
        };
        assert!(matches!(
            dispatch_command_frame(
                &mut connection,
                &mut executor,
                &command(crate::COM_QUERY, b"select value")
            ),
            Err(CommandDispatcherError::ResultRowShape {
                row: 0,
                expected_columns: 1,
                actual_values: 0,
            })
        ));
        assert_eq!(connection.state(), ConnectionState::Closing);
    }
}
