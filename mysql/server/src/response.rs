//! Bounded classic-protocol server responses and text result sets.

use std::{error::Error, fmt, str};

use crate::{
    AuthOkPacket, AuthOkPacketConfig, AuthPacketError, PacketCodec, PacketCodecError,
    CLIENT_DEPRECATE_EOF, CLIENT_PROTOCOL_41,
};

/// The normal protocol OK packet header.
pub const RESPONSE_OK_HEADER: u8 = 0x00;
/// The OK packet header used for a result-set terminator when EOF is deprecated.
pub const RESPONSE_OK_TERMINATOR_HEADER: u8 = 0xfe;

/// Values used to encode a protocol OK packet response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseOkPacketConfig {
    /// Packet header. Only `0x00` and `0xfe` are valid OK headers.
    pub header: u8,
    /// Number of rows affected by the operation.
    pub affected_rows: u64,
    /// Last inserted identifier.
    pub last_insert_id: u64,
    /// Server status flags.
    pub status_flags: u16,
    /// Server warning count.
    pub warnings: u16,
    /// Opaque informational bytes following the fixed fields.
    pub info: Vec<u8>,
}

impl ResponseOkPacketConfig {
    /// Creates a normal response OK packet with header `0x00`.
    pub fn new(affected_rows: u64, last_insert_id: u64, status_flags: u16, warnings: u16) -> Self {
        Self {
            header: RESPONSE_OK_HEADER,
            affected_rows,
            last_insert_id,
            status_flags,
            warnings,
            info: Vec::new(),
        }
    }

    /// Creates an OK packet with an explicitly selected valid header.
    pub fn new_with_header(
        header: u8,
        affected_rows: u64,
        last_insert_id: u64,
        status_flags: u16,
        warnings: u16,
    ) -> Self {
        Self {
            header,
            affected_rows,
            last_insert_id,
            status_flags,
            warnings,
            info: Vec::new(),
        }
    }

    /// Checks the header and bounded OK-packet fields.
    pub fn validate(&self) -> Result<(), ResponsePacketError> {
        validate_ok_header(self.header)?;
        self.as_auth_config()
            .validate()
            .map_err(ResponsePacketError::from)
    }

    /// Encodes one response OK packet.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        self.validate()?;
        self.as_auth_config()
            .encode_with_header(codec, sequence_id, self.header)
            .map_err(ResponsePacketError::from)
    }

    fn as_auth_config(&self) -> AuthOkPacketConfig {
        AuthOkPacketConfig {
            affected_rows: self.affected_rows,
            last_insert_id: self.last_insert_id,
            status_flags: self.status_flags,
            warnings: self.warnings,
            info: self.info.clone(),
        }
    }
}

impl Default for ResponseOkPacketConfig {
    fn default() -> Self {
        Self::new(0, 0, 0x0002, 0)
    }
}

/// A decoded protocol OK packet response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseOkPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Packet header, either `0x00` or `0xfe`.
    pub header: u8,
    /// Number of rows affected by the operation.
    pub affected_rows: u64,
    /// Last inserted identifier.
    pub last_insert_id: u64,
    /// Server status flags.
    pub status_flags: u16,
    /// Server warning count.
    pub warnings: u16,
    /// Opaque informational bytes.
    pub info: Vec<u8>,
}

impl ResponseOkPacket {
    /// Decodes an OK packet with either official OK header.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, ResponsePacketError> {
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        let header =
            packet
                .payload
                .first()
                .copied()
                .ok_or(ResponsePacketError::InvalidPayloadLength {
                    actual: 0,
                    expected: 1,
                })?;
        validate_ok_header(header)?;
        let decoded = AuthOkPacket::decode_with_header(codec, frame, header)
            .map_err(ResponsePacketError::from)?;
        Ok(Self {
            sequence_id: decoded.sequence_id,
            header,
            affected_rows: decoded.affected_rows,
            last_insert_id: decoded.last_insert_id,
            status_flags: decoded.status_flags,
            warnings: decoded.warnings,
            info: decoded.info,
        })
    }

    /// Encodes this response OK packet with a new sequence number.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        ResponseOkPacketConfig {
            header: self.header,
            affected_rows: self.affected_rows,
            last_insert_id: self.last_insert_id,
            status_flags: self.status_flags,
            warnings: self.warnings,
            info: self.info.clone(),
        }
        .encode(codec, sequence_id)
    }
}

/// Header byte for the first packet in a successful `COM_STMT_PREPARE`
/// response.
pub const STMT_PREPARE_OK_HEADER: u8 = 0x00;
/// Payload length of the fixed `COM_STMT_PREPARE_OK` response packet.
pub const STMT_PREPARE_OK_PAYLOAD_LENGTH: usize = 12;

/// Values used to encode a fixed `COM_STMT_PREPARE_OK` response packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StmtPrepareOkPacketConfig {
    /// Connection-local identifier assigned to the prepared statement.
    pub statement_id: u32,
    /// Number of result columns described by the prepared statement.
    pub num_columns: u16,
    /// Number of parameters accepted by the prepared statement.
    pub num_params: u16,
    /// Number of warnings generated while preparing the statement.
    pub warning_count: u16,
}

impl StmtPrepareOkPacketConfig {
    /// Creates a fixed `COM_STMT_PREPARE_OK` response configuration.
    pub const fn new(
        statement_id: u32,
        num_columns: u16,
        num_params: u16,
        warning_count: u16,
    ) -> Self {
        Self {
            statement_id,
            num_columns,
            num_params,
            warning_count,
        }
    }

    /// Encodes the fixed first packet of a successful prepared statement.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        let mut payload = Vec::with_capacity(STMT_PREPARE_OK_PAYLOAD_LENGTH);
        payload.push(STMT_PREPARE_OK_HEADER);
        payload.extend_from_slice(&self.statement_id.to_le_bytes());
        payload.extend_from_slice(&self.num_columns.to_le_bytes());
        payload.extend_from_slice(&self.num_params.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&self.warning_count.to_le_bytes());
        debug_assert_eq!(payload.len(), STMT_PREPARE_OK_PAYLOAD_LENGTH);
        codec
            .encode(sequence_id, &payload)
            .map_err(ResponsePacketError::from)
    }
}

/// A decoded fixed `COM_STMT_PREPARE_OK` response packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StmtPrepareOkPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Connection-local identifier assigned to the prepared statement.
    pub statement_id: u32,
    /// Number of result columns described by the prepared statement.
    pub num_columns: u16,
    /// Number of parameters accepted by the prepared statement.
    pub num_params: u16,
    /// Number of warnings generated while preparing the statement.
    pub warning_count: u16,
}

impl StmtPrepareOkPacket {
    /// Decodes the fixed first packet of a successful prepared statement.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, ResponsePacketError> {
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        if packet.payload.len() != STMT_PREPARE_OK_PAYLOAD_LENGTH {
            return Err(ResponsePacketError::InvalidPayloadLength {
                actual: packet.payload.len(),
                expected: STMT_PREPARE_OK_PAYLOAD_LENGTH,
            });
        }
        if packet.payload[0] != STMT_PREPARE_OK_HEADER {
            return Err(ResponsePacketError::UnexpectedMarker {
                actual: packet.payload[0],
                expected: STMT_PREPARE_OK_HEADER,
            });
        }
        if packet.payload[9] != 0 {
            return Err(ResponsePacketError::NonZeroFiller);
        }
        Ok(Self {
            sequence_id: packet.sequence_id,
            statement_id: u32::from_le_bytes([
                packet.payload[1],
                packet.payload[2],
                packet.payload[3],
                packet.payload[4],
            ]),
            num_columns: u16::from_le_bytes([packet.payload[5], packet.payload[6]]),
            num_params: u16::from_le_bytes([packet.payload[7], packet.payload[8]]),
            warning_count: u16::from_le_bytes([packet.payload[10], packet.payload[11]]),
        })
    }

    /// Encodes this response packet with a new sequence number.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        StmtPrepareOkPacketConfig::new(
            self.statement_id,
            self.num_columns,
            self.num_params,
            self.warning_count,
        )
        .encode(codec, sequence_id)
    }
}

/// Compatibility alias for the prepared-statement response configuration.
pub type PrepareOkPacketConfig = StmtPrepareOkPacketConfig;
/// Compatibility alias for a decoded prepared-statement response packet.
pub type PrepareOkPacket = StmtPrepareOkPacket;

/// Compatibility alias for a decoded response OK packet.
pub type OkPacket = ResponseOkPacket;
/// Compatibility alias for a response OK packet configuration.
pub type OkPacketConfig = ResponseOkPacketConfig;

/// Maximum payload accepted by response packet models.
pub const MAX_RESPONSE_PACKET_PAYLOAD_LENGTH: usize = 4096;
/// Maximum number of columns accepted in one conservative text result set.
pub const MAX_RESULT_COLUMNS: usize = 256;
/// Maximum length of one textual column-definition field.
pub const MAX_COLUMN_TEXT_LENGTH: usize = 1024;
/// Maximum length of an ERR message under the protocol-4.1 layout.
pub const MAX_ERROR_MESSAGE_LENGTH: usize = MAX_RESPONSE_PACKET_PAYLOAD_LENGTH - 9;
/// Maximum length of one binary-safe text-row value.
pub const MAX_TEXT_ROW_VALUE_LENGTH: usize = MAX_RESPONSE_PACKET_PAYLOAD_LENGTH;
/// Maximum length of one binary-protocol row value.
pub const MAX_BINARY_ROW_VALUE_LENGTH: usize = MAX_RESPONSE_PACKET_PAYLOAD_LENGTH;

/// Maximum packet sequence number before the protocol-defined wrap to zero.
pub const MAX_PACKET_SEQUENCE_ID: u8 = u8::MAX;

/// Typed categories emitted by frontend adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendErrorKind {
    /// SQL syntax or statement-shape failure.
    Syntax,
    /// A requested logical database does not exist or cannot be named safely.
    UnknownDatabase,
    /// A statement needs a selected logical database, but none is selected.
    NoDatabaseSelected,
    /// A logical database already exists.
    DuplicateDatabase,
    /// A logical database cannot be changed while another session retains it.
    DatabaseBusy,
    /// A catalog or storage failure whose details must not reach the client.
    Internal,
    /// A referenced table, column, or other object does not exist.
    MissingObject,
    UnknownView,
    NotView,
    /// A prepared-statement command referenced no statement on this connection.
    UnknownPreparedStatement,
    /// The configured prepared-statement quota rejected a new statement.
    PreparedStatementLimitReached,
    /// An object or key already exists.
    DuplicateObject,
    /// A unique, foreign-key, or other constraint rejected the operation.
    ConstraintViolation,
    /// A required column was omitted by a checked default-row INSERT.
    MissingRequiredDefault,
    /// The configured statement execution deadline elapsed.
    QueryTimeout,
    /// The statement or feature is not implemented.
    Unsupported,
    /// Authentication failed without exposing credential details.
    Authentication,
    /// The authenticated principal is not allowed to use the requested data.
    AccessDenied,
}

/// Maps a typed frontend category to a conservative protocol ERR response.
pub fn map_frontend_error(kind: FrontendErrorKind) -> ErrPacketConfig {
    let (error_code, sql_state, message) = match kind {
        FrontendErrorKind::Syntax => (1064, *b"42000", b"syntax error".as_slice()),
        FrontendErrorKind::UnknownDatabase => (1049, *b"42000", b"unknown database".as_slice()),
        FrontendErrorKind::NoDatabaseSelected => {
            (1046, *b"3D000", b"no database selected".as_slice())
        }
        FrontendErrorKind::DuplicateDatabase => {
            (1007, *b"HY000", b"database already exists".as_slice())
        }
        FrontendErrorKind::DatabaseBusy => (1205, *b"HY000", b"database is busy".as_slice()),
        FrontendErrorKind::Internal => (1105, *b"HY000", b"internal error".as_slice()),
        FrontendErrorKind::MissingObject => (1146, *b"42S02", b"unknown object".as_slice()),
        FrontendErrorKind::UnknownView => (1051, *b"42S02", b"unknown view".as_slice()),
        FrontendErrorKind::NotView => (1347, *b"HY000", b"object is not a view".as_slice()),
        FrontendErrorKind::UnknownPreparedStatement => {
            (1243, *b"HY000", b"unknown prepared statement".as_slice())
        }
        FrontendErrorKind::PreparedStatementLimitReached => (
            1461,
            *b"42000",
            b"maximum prepared statement count reached".as_slice(),
        ),
        FrontendErrorKind::DuplicateObject => {
            (1050, *b"42S01", b"object already exists".as_slice())
        }
        FrontendErrorKind::MissingRequiredDefault => (
            1364,
            *b"HY000",
            b"field doesn't have a default value".as_slice(),
        ),
        FrontendErrorKind::ConstraintViolation => {
            (1062, *b"23000", b"constraint violation".as_slice())
        }
        FrontendErrorKind::QueryTimeout => {
            (3024, *b"HY000", b"query execution time exceeded".as_slice())
        }
        FrontendErrorKind::Unsupported => (1235, *b"42000", b"feature not supported".as_slice()),
        FrontendErrorKind::Authentication => (1045, *b"28000", b"access denied".as_slice()),
        FrontendErrorKind::AccessDenied => (1045, *b"28000", b"access denied".as_slice()),
    };
    ErrPacketConfig {
        error_code,
        sql_state,
        message: message.to_vec(),
    }
}

/// A strict protocol-4.1 ERR packet configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrPacketConfig {
    /// MySQL numeric error code.
    pub error_code: u16,
    /// Five-byte SQLSTATE value.
    pub sql_state: [u8; 5],
    /// Error message bytes. The protocol does not require these to be UTF-8.
    pub message: Vec<u8>,
}

impl ErrPacketConfig {
    /// Checks SQLSTATE and bounded message fields.
    pub fn validate(&self, capability_flags: u32) -> Result<(), ResponsePacketError> {
        validate_sql_state(&self.sql_state)?;
        let overhead = if capability_flags & CLIENT_PROTOCOL_41 != 0 {
            9
        } else {
            3
        };
        if self.message.len() > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH - overhead {
            return Err(ResponsePacketError::FieldTooLong {
                field: "ERR message",
                length: self.message.len(),
                limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH - overhead,
            });
        }
        Ok(())
    }

    /// Encodes one ERR packet according to negotiated protocol capabilities.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
        capability_flags: u32,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        self.validate(capability_flags)?;
        let mut payload = Vec::with_capacity(9 + self.message.len());
        payload.push(0xff);
        payload.extend_from_slice(&self.error_code.to_le_bytes());
        if capability_flags & CLIENT_PROTOCOL_41 != 0 {
            payload.push(b'#');
            payload.extend_from_slice(&self.sql_state);
        }
        payload.extend_from_slice(&self.message);
        codec
            .encode(sequence_id, &payload)
            .map_err(ResponsePacketError::from)
    }
}

impl From<FrontendErrorKind> for ErrPacketConfig {
    fn from(kind: FrontendErrorKind) -> Self {
        map_frontend_error(kind)
    }
}

/// A decoded protocol-4.1 ERR packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// MySQL numeric error code.
    pub error_code: u16,
    /// SQLSTATE when protocol 4.1 was negotiated.
    pub sql_state: Option<[u8; 5]>,
    /// Error message bytes.
    pub message: Vec<u8>,
}

impl ErrPacket {
    /// Decodes one bounded ERR packet according to negotiated capabilities.
    pub fn decode(
        codec: PacketCodec,
        frame: &[u8],
        capability_flags: u32,
    ) -> Result<Self, ResponsePacketError> {
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        check_response_payload_length(packet.payload.len())?;
        let mut reader = ResponseReader::new(packet.payload);
        if reader.read_u8("ERR marker")? != 0xff {
            return Err(ResponsePacketError::UnexpectedMarker {
                actual: packet.payload[0],
                expected: 0xff,
            });
        }
        let error_code = reader.read_u16("ERR code")?;
        let sql_state = if capability_flags & CLIENT_PROTOCOL_41 != 0 {
            if reader.read_u8("SQLSTATE marker")? != b'#' {
                return Err(ResponsePacketError::UnexpectedMarker {
                    actual: packet.payload[3],
                    expected: b'#',
                });
            }
            let bytes = reader.read_exact(5, "SQLSTATE")?;
            let mut state = [0; 5];
            state.copy_from_slice(bytes);
            validate_sql_state(&state)?;
            Some(state)
        } else {
            None
        };
        let message = reader.remaining().to_vec();
        let message_limit = if capability_flags & CLIENT_PROTOCOL_41 != 0 {
            MAX_RESPONSE_PACKET_PAYLOAD_LENGTH - 9
        } else {
            MAX_RESPONSE_PACKET_PAYLOAD_LENGTH - 3
        };
        if message.len() > message_limit {
            return Err(ResponsePacketError::FieldTooLong {
                field: "ERR message",
                length: message.len(),
                limit: message_limit,
            });
        }
        Ok(Self {
            sequence_id: packet.sequence_id,
            error_code,
            sql_state,
            message,
        })
    }

    /// Encodes this ERR packet with a new sequence number.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
        capability_flags: u32,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        let sql_state = self.sql_state.unwrap_or(*b"HY000");
        ErrPacketConfig {
            error_code: self.error_code,
            sql_state,
            message: self.message.clone(),
        }
        .encode(codec, sequence_id, capability_flags)
    }
}

/// One result-set column-count packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnCountPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Number of following column-definition packets.
    pub column_count: usize,
}

impl ColumnCountPacket {
    /// Encodes one bounded column-count packet.
    pub fn encode(
        codec: PacketCodec,
        sequence_id: u8,
        column_count: usize,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        validate_column_count(column_count)?;
        let payload = encode_lenenc_integer(column_count as u64);
        codec
            .encode(sequence_id, &payload)
            .map_err(ResponsePacketError::from)
    }

    /// Decodes one exact column-count packet.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, ResponsePacketError> {
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        check_response_payload_length(packet.payload.len())?;
        let mut reader = ResponseReader::new(packet.payload);
        let count = reader.read_lenenc_integer("column count")?;
        let column_count =
            usize::try_from(count).map_err(|_| ResponsePacketError::ColumnCountOutOfRange {
                count,
                limit: MAX_RESULT_COLUMNS,
            })?;
        validate_column_count(column_count)?;
        reader.finish()?;
        Ok(Self {
            sequence_id: packet.sequence_id,
            column_count,
        })
    }
}

/// Values used to encode one result-set column definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinitionConfig {
    /// Catalog name.
    pub catalog: String,
    /// Schema/database name.
    pub schema: String,
    /// Table name.
    pub table: String,
    /// Original table name.
    pub original_table: String,
    /// Display column name.
    pub name: String,
    /// Original column name.
    pub original_name: String,
    /// Character-set identifier.
    pub character_set: u16,
    /// Maximum display width.
    pub column_length: u32,
    /// MySQL column type identifier.
    pub column_type: u8,
    /// MySQL column flags.
    pub flags: u16,
    /// Display decimals.
    pub decimals: u8,
}

impl ColumnDefinitionConfig {
    /// Creates a definition with the protocol catalog and empty schema/table metadata.
    pub fn new(name: impl Into<String>, column_type: u8) -> Self {
        Self {
            catalog: "def".to_owned(),
            schema: String::new(),
            table: String::new(),
            original_table: String::new(),
            name: name.into(),
            original_name: String::new(),
            character_set: 0,
            column_length: 0,
            column_type,
            flags: 0,
            decimals: 0,
        }
    }

    /// Checks all textual and fixed fields.
    pub fn validate(&self) -> Result<(), ResponsePacketError> {
        validate_column_text(&self.catalog, "catalog", false)?;
        validate_column_text(&self.schema, "schema", false)?;
        validate_column_text(&self.table, "table", false)?;
        validate_column_text(&self.original_table, "original table", false)?;
        validate_column_text(&self.name, "column name", true)?;
        validate_column_text(&self.original_name, "original column name", false)
    }

    /// Encodes this definition as one protocol-4.1 packet.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        self.validate()?;
        let mut payload = Vec::new();
        for value in [
            &self.catalog,
            &self.schema,
            &self.table,
            &self.original_table,
            &self.name,
            &self.original_name,
        ] {
            push_lenenc_bytes(&mut payload, value.as_bytes());
        }
        payload.push(0x0c);
        payload.extend_from_slice(&self.character_set.to_le_bytes());
        payload.extend_from_slice(&self.column_length.to_le_bytes());
        payload.push(self.column_type);
        payload.extend_from_slice(&self.flags.to_le_bytes());
        payload.push(self.decimals);
        payload.extend_from_slice(&[0; 2]);
        if payload.len() > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
            return Err(ResponsePacketError::PayloadTooLarge {
                length: payload.len(),
                limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
            });
        }
        codec
            .encode(sequence_id, &payload)
            .map_err(ResponsePacketError::from)
    }
}

/// A decoded result-set column definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinitionPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Catalog name.
    pub catalog: String,
    /// Schema/database name.
    pub schema: String,
    /// Table name.
    pub table: String,
    /// Original table name.
    pub original_table: String,
    /// Display column name.
    pub name: String,
    /// Original column name.
    pub original_name: String,
    /// Character-set identifier.
    pub character_set: u16,
    /// Maximum display width.
    pub column_length: u32,
    /// MySQL column type identifier.
    pub column_type: u8,
    /// MySQL column flags.
    pub flags: u16,
    /// Display decimals.
    pub decimals: u8,
}

impl ColumnDefinitionPacket {
    /// Decodes one bounded protocol-4.1 column definition.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, ResponsePacketError> {
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        check_response_payload_length(packet.payload.len())?;
        let mut reader = ResponseReader::new(packet.payload);
        let catalog = reader.read_text("catalog", false)?;
        let schema = reader.read_text("schema", false)?;
        let table = reader.read_text("table", false)?;
        let original_table = reader.read_text("original table", false)?;
        let name = reader.read_text("column name", true)?;
        let original_name = reader.read_text("original column name", false)?;
        if reader.read_u8("column definition fixed-length marker")? != 0x0c {
            return Err(ResponsePacketError::InvalidFixedLength);
        }
        let character_set = reader.read_u16("character set")?;
        let column_length = reader.read_u32("column length")?;
        let column_type = reader.read_u8("column type")?;
        let flags = reader.read_u16("column flags")?;
        let decimals = reader.read_u8("decimals")?;
        if reader.read_exact(2, "column definition filler")? != [0, 0] {
            return Err(ResponsePacketError::NonZeroFiller);
        }
        reader.finish()?;
        Ok(Self {
            sequence_id: packet.sequence_id,
            catalog,
            schema,
            table,
            original_table,
            name,
            original_name,
            character_set,
            column_length,
            column_type,
            flags,
            decimals,
        })
    }

    /// Encodes this definition with a new sequence number.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        ColumnDefinitionConfig {
            catalog: self.catalog.clone(),
            schema: self.schema.clone(),
            table: self.table.clone(),
            original_table: self.original_table.clone(),
            name: self.name.clone(),
            original_name: self.original_name.clone(),
            character_set: self.character_set,
            column_length: self.column_length,
            column_type: self.column_type,
            flags: self.flags,
            decimals: self.decimals,
        }
        .encode(codec, sequence_id)
    }
}

/// A binary-safe value in a text-protocol result row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRowValue<'a> {
    /// SQL NULL, represented by the dedicated 0xfb marker.
    Null,
    /// Bytes are returned without UTF-8 conversion.
    Bytes(&'a [u8]),
}

/// Header byte for a binary-protocol result row.
pub const BINARY_ROW_HEADER: u8 = 0x00;

/// A value in a binary-protocol result row.
///
/// Byte and string values borrow their contents so callers can encode a row
/// without copying its variable-width values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryRowValue<'a> {
    /// SQL NULL, represented by the row's NULL bitmap.
    Null,
    /// A signed 8-bit integer in little-endian order.
    Int8(i8),
    /// A signed 16-bit integer in little-endian order.
    Int16(i16),
    /// A signed 24-bit integer in the protocol's four-byte representation.
    Int24(i32),
    /// A signed 32-bit integer in little-endian order.
    Int32(i32),
    /// A signed 64-bit integer in little-endian order.
    Int64(i64),
    /// An IEEE-754 double in little-endian order.
    Float64(f64),
    /// Length-encoded binary bytes.
    Bytes(&'a [u8]),
    /// Length-encoded UTF-8 text.
    String(&'a str),
}

/// The wire representation to use when decoding a non-NULL binary row value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryRowColumnType {
    /// A signed 8-bit integer.
    Int8,
    /// A signed 16-bit integer.
    Int16,
    /// A signed 24-bit integer in the protocol's four-byte representation.
    Int24,
    /// A signed 32-bit integer.
    Int32,
    /// A signed 64-bit integer.
    Int64,
    /// An IEEE-754 double.
    Float64,
    /// Length-encoded binary bytes.
    Bytes,
    /// Length-encoded UTF-8 text.
    String,
}

impl<'a> BinaryRowValue<'a> {
    /// Converts a signed semantic integer to the selected binary-row width.
    ///
    /// The server stores integer results as `i64`; this checked conversion is
    /// the boundary that prevents a narrow MySQL result column from silently
    /// truncating an out-of-range value on the wire.
    pub fn try_from_signed_integer(
        value: i64,
        column_type: BinaryRowColumnType,
    ) -> Result<Self, ResponsePacketError> {
        match column_type {
            BinaryRowColumnType::Int8 => i8::try_from(value)
                .map(Self::Int8)
                .map_err(|_| ResponsePacketError::BinaryIntegerOutOfRange { value, column_type }),
            BinaryRowColumnType::Int16 => i16::try_from(value)
                .map(Self::Int16)
                .map_err(|_| ResponsePacketError::BinaryIntegerOutOfRange { value, column_type }),
            BinaryRowColumnType::Int24 => {
                if !(-8_388_608..=8_388_607).contains(&value) {
                    return Err(ResponsePacketError::BinaryIntegerOutOfRange {
                        value,
                        column_type,
                    });
                }
                let value = i32::try_from(value).map_err(|_| {
                    ResponsePacketError::BinaryIntegerOutOfRange { value, column_type }
                })?;
                Ok(Self::Int24(value))
            }
            BinaryRowColumnType::Int32 => i32::try_from(value)
                .map(Self::Int32)
                .map_err(|_| ResponsePacketError::BinaryIntegerOutOfRange { value, column_type }),
            BinaryRowColumnType::Int64 => Ok(Self::Int64(value)),
            _ => Err(ResponsePacketError::BinaryIntegerTypeMismatch { column_type }),
        }
    }
}

/// One decoded binary-protocol result row.
#[derive(Debug, Clone, PartialEq)]
pub struct BinaryRowPacket<'a> {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Exactly one value for each result column.
    pub values: Vec<BinaryRowValue<'a>>,
}

impl<'a> BinaryRowPacket<'a> {
    /// Encodes one bounded binary-protocol row.
    pub fn encode(
        codec: PacketCodec,
        sequence_id: u8,
        values: &[BinaryRowValue<'a>],
    ) -> Result<Vec<u8>, ResponsePacketError> {
        validate_column_count(values.len())?;
        let null_bitmap_length = binary_row_null_bitmap_len(values.len());
        let mut payload_length =
            1usize
                .checked_add(null_bitmap_length)
                .ok_or(ResponsePacketError::PayloadTooLarge {
                    length: usize::MAX,
                    limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
                })?;
        for value in values {
            let length = binary_row_value_encoded_len(*value)?;
            payload_length =
                payload_length
                    .checked_add(length)
                    .ok_or(ResponsePacketError::PayloadTooLarge {
                        length: usize::MAX,
                        limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
                    })?;
        }
        check_response_payload_length(payload_length)?;

        let mut payload = Vec::with_capacity(payload_length);
        payload.push(BINARY_ROW_HEADER);
        let null_bitmap_offset = payload.len();
        payload.resize(null_bitmap_offset + null_bitmap_length, 0);
        for (column, value) in values.iter().enumerate() {
            match value {
                BinaryRowValue::Null => {
                    set_binary_row_null(&mut payload, null_bitmap_offset, column)
                }
                BinaryRowValue::Int8(value) => payload.extend_from_slice(&value.to_le_bytes()),
                BinaryRowValue::Int16(value) => payload.extend_from_slice(&value.to_le_bytes()),
                BinaryRowValue::Int24(value) => {
                    validate_int24_value(*value)?;
                    payload.extend_from_slice(&value.to_le_bytes());
                }
                BinaryRowValue::Int32(value) => payload.extend_from_slice(&value.to_le_bytes()),
                BinaryRowValue::Int64(value) => payload.extend_from_slice(&value.to_le_bytes()),
                BinaryRowValue::Float64(value) => payload.extend_from_slice(&value.to_le_bytes()),
                BinaryRowValue::Bytes(value) => push_lenenc_bytes(&mut payload, value),
                BinaryRowValue::String(value) => push_lenenc_bytes(&mut payload, value.as_bytes()),
            }
        }
        debug_assert_eq!(payload.len(), payload_length);
        codec
            .encode(sequence_id, &payload)
            .map_err(ResponsePacketError::from)
    }

    /// Decodes one bounded binary row using its result-column types.
    pub fn decode(
        codec: PacketCodec,
        frame: &'a [u8],
        column_types: &[BinaryRowColumnType],
    ) -> Result<Self, ResponsePacketError> {
        validate_column_count(column_types.len())?;
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        check_response_payload_length(packet.payload.len())?;
        let mut reader = ResponseReader::new(packet.payload);
        let header = reader.read_u8("binary-row header")?;
        if header != BINARY_ROW_HEADER {
            return Err(ResponsePacketError::UnexpectedMarker {
                actual: header,
                expected: BINARY_ROW_HEADER,
            });
        }
        let null_bitmap = reader.read_exact(
            binary_row_null_bitmap_len(column_types.len()),
            "binary-row NULL bitmap",
        )?;
        let mut values = Vec::with_capacity(column_types.len());
        for (column, column_type) in column_types.iter().enumerate() {
            if binary_row_is_null(null_bitmap, column) {
                values.push(BinaryRowValue::Null);
                continue;
            }
            let value = match column_type {
                BinaryRowColumnType::Int8 => BinaryRowValue::Int8(reader.read_i8("binary-row i8")?),
                BinaryRowColumnType::Int16 => {
                    BinaryRowValue::Int16(reader.read_i16("binary-row i16")?)
                }
                BinaryRowColumnType::Int24 => {
                    let value = reader.read_i32("binary-row int24")?;
                    validate_int24_value(value)?;
                    BinaryRowValue::Int24(value)
                }
                BinaryRowColumnType::Int32 => {
                    BinaryRowValue::Int32(reader.read_i32("binary-row i32")?)
                }
                BinaryRowColumnType::Int64 => {
                    BinaryRowValue::Int64(reader.read_i64("binary-row i64")?)
                }
                BinaryRowColumnType::Float64 => {
                    BinaryRowValue::Float64(reader.read_f64("binary-row f64")?)
                }
                BinaryRowColumnType::Bytes => BinaryRowValue::Bytes(
                    reader.read_bytes("binary-row bytes", MAX_BINARY_ROW_VALUE_LENGTH)?,
                ),
                BinaryRowColumnType::String => {
                    let bytes =
                        reader.read_bytes("binary-row string", MAX_BINARY_ROW_VALUE_LENGTH)?;
                    let value =
                        str::from_utf8(bytes).map_err(|_| ResponsePacketError::InvalidUtf8 {
                            field: "binary-row string",
                        })?;
                    BinaryRowValue::String(value)
                }
            };
            values.push(value);
        }
        reader.finish()?;
        Ok(Self {
            sequence_id: packet.sequence_id,
            values,
        })
    }
}

/// One decoded text-protocol result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRowPacket<'a> {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Exactly one value for each result column.
    pub values: Vec<TextRowValue<'a>>,
}

impl<'a> TextRowPacket<'a> {
    /// Encodes one binary-safe text row.
    pub fn encode(
        codec: PacketCodec,
        sequence_id: u8,
        values: &[TextRowValue<'a>],
    ) -> Result<Vec<u8>, ResponsePacketError> {
        validate_column_count(values.len())?;
        let mut payload_length = 0usize;
        for value in values {
            let length = match value {
                TextRowValue::Null => 1,
                TextRowValue::Bytes(bytes) => {
                    if bytes.len() > MAX_TEXT_ROW_VALUE_LENGTH {
                        return Err(ResponsePacketError::FieldTooLong {
                            field: "text-row value",
                            length: bytes.len(),
                            limit: MAX_TEXT_ROW_VALUE_LENGTH,
                        });
                    }
                    lenenc_integer_len(bytes.len() as u64)
                        .checked_add(bytes.len())
                        .ok_or(ResponsePacketError::PayloadTooLarge {
                            length: usize::MAX,
                            limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
                        })?
                }
            };
            payload_length =
                payload_length
                    .checked_add(length)
                    .ok_or(ResponsePacketError::PayloadTooLarge {
                        length: usize::MAX,
                        limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
                    })?;
        }
        if payload_length > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
            return Err(ResponsePacketError::PayloadTooLarge {
                length: payload_length,
                limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
            });
        }
        let mut payload = Vec::with_capacity(payload_length);
        for value in values {
            match value {
                TextRowValue::Null => payload.push(0xfb),
                TextRowValue::Bytes(bytes) => {
                    push_lenenc_bytes(&mut payload, bytes);
                }
            }
        }
        codec
            .encode(sequence_id, &payload)
            .map_err(ResponsePacketError::from)
    }

    /// Decodes one row with exactly `column_count` values.
    pub fn decode(
        codec: PacketCodec,
        frame: &'a [u8],
        column_count: usize,
    ) -> Result<Self, ResponsePacketError> {
        validate_column_count(column_count)?;
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        check_response_payload_length(packet.payload.len())?;
        let mut reader = ResponseReader::new(packet.payload);
        let mut values = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            if reader.peek() == Some(0xfb) {
                reader.read_u8("NULL marker")?;
                values.push(TextRowValue::Null);
            } else {
                values.push(TextRowValue::Bytes(
                    reader.read_bytes("text-row value", MAX_TEXT_ROW_VALUE_LENGTH)?,
                ));
            }
        }
        reader.finish()?;
        Ok(Self {
            sequence_id: packet.sequence_id,
            values,
        })
    }
}

/// The classic result-set terminator used when EOF is not deprecated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EofPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Server warning count.
    pub warnings: u16,
    /// Server status flags.
    pub status_flags: u16,
}

impl EofPacket {
    /// Encodes one exact EOF result terminator.
    pub fn encode(
        codec: PacketCodec,
        sequence_id: u8,
        warnings: u16,
        status_flags: u16,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        codec
            .encode(
                sequence_id,
                &[
                    0xfe,
                    warnings as u8,
                    (warnings >> 8) as u8,
                    status_flags as u8,
                    (status_flags >> 8) as u8,
                ],
            )
            .map_err(ResponsePacketError::from)
    }

    /// Decodes one exact EOF result terminator.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, ResponsePacketError> {
        let packet = codec.decode(frame).map_err(ResponsePacketError::from)?;
        if packet.payload.len() != 5 {
            return Err(ResponsePacketError::InvalidPayloadLength {
                actual: packet.payload.len(),
                expected: 5,
            });
        }
        if packet.payload[0] != 0xfe {
            return Err(ResponsePacketError::UnexpectedMarker {
                actual: packet.payload[0],
                expected: 0xfe,
            });
        }
        Ok(Self {
            sequence_id: packet.sequence_id,
            warnings: u16::from_le_bytes([packet.payload[1], packet.payload[2]]),
            status_flags: u16::from_le_bytes([packet.payload[3], packet.payload[4]]),
        })
    }
}

/// A result-set terminator selected by `CLIENT_DEPRECATE_EOF`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultTerminatorPacket {
    /// Legacy EOF packet.
    Eof(EofPacket),
    /// OK packet used when EOF is deprecated.
    Ok(OkPacket),
}

impl ResultTerminatorPacket {
    /// Encodes the negotiated result-set terminator.
    pub fn encode(
        codec: PacketCodec,
        sequence_id: u8,
        capability_flags: u32,
        warnings: u16,
        status_flags: u16,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        if capability_flags & CLIENT_DEPRECATE_EOF != 0 {
            if capability_flags & CLIENT_PROTOCOL_41 == 0 {
                return Err(ResponsePacketError::MissingCapability {
                    capability: CLIENT_PROTOCOL_41,
                });
            }
            OkPacketConfig::new_with_header(
                RESPONSE_OK_TERMINATOR_HEADER,
                0,
                0,
                status_flags,
                warnings,
            )
            .encode(codec, sequence_id)
        } else {
            EofPacket::encode(codec, sequence_id, warnings, status_flags)
        }
    }

    /// Decodes the negotiated result-set terminator.
    pub fn decode(
        codec: PacketCodec,
        frame: &[u8],
        capability_flags: u32,
    ) -> Result<Self, ResponsePacketError> {
        if capability_flags & CLIENT_DEPRECATE_EOF != 0 {
            if capability_flags & CLIENT_PROTOCOL_41 == 0 {
                return Err(ResponsePacketError::MissingCapability {
                    capability: CLIENT_PROTOCOL_41,
                });
            }
            let packet = OkPacket::decode(codec, frame)?;
            if packet.header != RESPONSE_OK_TERMINATOR_HEADER {
                return Err(ResponsePacketError::UnexpectedMarker {
                    actual: packet.header,
                    expected: RESPONSE_OK_TERMINATOR_HEADER,
                });
            }
            Ok(Self::Ok(packet))
        } else {
            Ok(Self::Eof(EofPacket::decode(codec, frame)?))
        }
    }
}

impl PacketCodec {
    /// Encodes the fixed first packet of a successful `COM_STMT_PREPARE`.
    pub fn encode_stmt_prepare_ok(
        self,
        sequence_id: u8,
        config: &StmtPrepareOkPacketConfig,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        config.encode(self, sequence_id)
    }

    /// Decodes the fixed first packet of a successful `COM_STMT_PREPARE`.
    pub fn decode_stmt_prepare_ok(
        self,
        frame: &[u8],
    ) -> Result<StmtPrepareOkPacket, ResponsePacketError> {
        StmtPrepareOkPacket::decode(self, frame)
    }

    /// Encodes one ERR packet according to negotiated capabilities.
    pub fn encode_err_packet(
        self,
        sequence_id: u8,
        config: &ErrPacketConfig,
        capability_flags: u32,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        config.encode(self, sequence_id, capability_flags)
    }

    /// Decodes one ERR packet according to negotiated capabilities.
    pub fn decode_err_packet(
        self,
        frame: &[u8],
        capability_flags: u32,
    ) -> Result<ErrPacket, ResponsePacketError> {
        ErrPacket::decode(self, frame, capability_flags)
    }

    /// Encodes one protocol OK packet.
    pub fn encode_ok_packet(
        self,
        sequence_id: u8,
        config: &OkPacketConfig,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        config.encode(self, sequence_id)
    }

    /// Decodes one protocol OK packet.
    pub fn decode_ok_packet(self, frame: &[u8]) -> Result<OkPacket, ResponsePacketError> {
        OkPacket::decode(self, frame)
    }

    /// Encodes one result-set column-count packet.
    pub fn encode_column_count(
        self,
        sequence_id: u8,
        column_count: usize,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        ColumnCountPacket::encode(self, sequence_id, column_count)
    }

    /// Decodes one result-set column-count packet.
    pub fn decode_column_count(
        self,
        frame: &[u8],
    ) -> Result<ColumnCountPacket, ResponsePacketError> {
        ColumnCountPacket::decode(self, frame)
    }

    /// Encodes one result-set column definition.
    pub fn encode_column_definition(
        self,
        sequence_id: u8,
        config: &ColumnDefinitionConfig,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        config.encode(self, sequence_id)
    }

    /// Decodes one result-set column definition.
    pub fn decode_column_definition(
        self,
        frame: &[u8],
    ) -> Result<ColumnDefinitionPacket, ResponsePacketError> {
        ColumnDefinitionPacket::decode(self, frame)
    }

    /// Encodes one binary-safe text-protocol row.
    pub fn encode_text_row<'a>(
        self,
        sequence_id: u8,
        values: &[TextRowValue<'a>],
    ) -> Result<Vec<u8>, ResponsePacketError> {
        TextRowPacket::encode(self, sequence_id, values)
    }

    /// Decodes one binary-safe text-protocol row.
    pub fn decode_text_row<'a>(
        self,
        frame: &'a [u8],
        column_count: usize,
    ) -> Result<TextRowPacket<'a>, ResponsePacketError> {
        TextRowPacket::decode(self, frame, column_count)
    }

    /// Encodes one binary-protocol result row.
    pub fn encode_binary_row<'a>(
        self,
        sequence_id: u8,
        values: &[BinaryRowValue<'a>],
    ) -> Result<Vec<u8>, ResponsePacketError> {
        BinaryRowPacket::encode(self, sequence_id, values)
    }

    /// Decodes one binary-protocol result row using its result-column types.
    pub fn decode_binary_row<'a>(
        self,
        frame: &'a [u8],
        column_types: &[BinaryRowColumnType],
    ) -> Result<BinaryRowPacket<'a>, ResponsePacketError> {
        BinaryRowPacket::decode(self, frame, column_types)
    }

    /// Encodes the negotiated EOF or OK result terminator.
    pub fn encode_result_terminator(
        self,
        sequence_id: u8,
        capability_flags: u32,
        warnings: u16,
        status_flags: u16,
    ) -> Result<Vec<u8>, ResponsePacketError> {
        ResultTerminatorPacket::encode(self, sequence_id, capability_flags, warnings, status_flags)
    }

    /// Decodes the negotiated EOF or OK result terminator.
    pub fn decode_result_terminator(
        self,
        frame: &[u8],
        capability_flags: u32,
    ) -> Result<ResultTerminatorPacket, ResponsePacketError> {
        ResultTerminatorPacket::decode(self, frame, capability_flags)
    }
}

/// Tracks classic packet sequence IDs with the protocol's explicit modulo-256 policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketSequence {
    expected: u8,
}

impl PacketSequence {
    /// Starts a sequence at the supplied packet ID.
    pub const fn new(first: u8) -> Self {
        Self { expected: first }
    }

    /// Returns the next expected packet ID.
    pub const fn expected(self) -> u8 {
        self.expected
    }

    /// Accepts one packet and advances, wrapping 255 to zero as the wire does.
    pub fn accept(&mut self, actual: u8) -> Result<(), ResponsePacketError> {
        if actual != self.expected {
            return Err(ResponsePacketError::UnexpectedSequenceId {
                expected: self.expected,
                actual,
            });
        }
        self.expected = self.expected.wrapping_add(1);
        Ok(())
    }

    /// Returns the current ID and advances with the same modulo-256 policy.
    pub fn next_sequence_id(&mut self) -> u8 {
        let sequence_id = self.expected;
        self.expected = self.expected.wrapping_add(1);
        sequence_id
    }
}

/// Errors returned by bounded server response models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponsePacketError {
    /// Packet framing rejected the frame.
    PacketCodec(PacketCodecError),
    /// The reused authentication OK packet rejected the payload.
    AuthPacket(AuthPacketError),
    /// A fixed packet did not have the expected payload length.
    InvalidPayloadLength { actual: usize, expected: usize },
    /// A packet marker did not match its model.
    UnexpectedMarker { actual: u8, expected: u8 },
    /// An OK packet header was not one of the two protocol-defined values.
    InvalidOkHeader { actual: u8 },
    /// A fixed-length column definition marker was not 0x0c.
    InvalidFixedLength,
    /// A reserved filler field was not zero.
    NonZeroFiller,
    /// A required field was truncated.
    TruncatedField { field: &'static str },
    /// A field contained trailing bytes after its complete packet structure.
    TrailingBytes { remaining: usize },
    /// A length-encoded integer marker was invalid.
    InvalidLengthEncodedInteger { field: &'static str, marker: u8 },
    /// A length-encoded integer used the NULL marker where an integer is required.
    NullLengthEncodedInteger { field: &'static str },
    /// A length-encoded integer was not in its canonical width.
    NonCanonicalLengthEncodedInteger { field: &'static str, value: u64 },
    /// A length-encoded value cannot fit in the platform's indexing type.
    LengthTooLarge { field: &'static str, length: u64 },
    /// A textual field was not valid UTF-8.
    InvalidUtf8 { field: &'static str },
    /// A textual field contained a NUL byte.
    EmbeddedNul { field: &'static str, offset: usize },
    /// A textual or binary field exceeded its limit.
    FieldTooLong {
        field: &'static str,
        length: usize,
        limit: usize,
    },
    /// A packet exceeded the response parser's bound.
    PayloadTooLarge { length: usize, limit: usize },
    /// SQLSTATE was not exactly five ASCII alphanumeric bytes.
    InvalidSqlState,
    /// The result-set column count exceeds the configured bound.
    ColumnCountOutOfRange { count: u64, limit: usize },
    /// A capability required for a negotiated packet was absent.
    MissingCapability { capability: u32 },
    /// A packet arrived out of sequence.
    UnexpectedSequenceId { expected: u8, actual: u8 },
    /// A signed integer does not fit in the selected binary-row width.
    BinaryIntegerOutOfRange {
        value: i64,
        column_type: BinaryRowColumnType,
    },
    /// A signed integer was requested for a non-integer binary-row type.
    BinaryIntegerTypeMismatch { column_type: BinaryRowColumnType },
}

impl From<PacketCodecError> for ResponsePacketError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl From<AuthPacketError> for ResponsePacketError {
    fn from(error: AuthPacketError) -> Self {
        Self::AuthPacket(error)
    }
}

impl fmt::Display for ResponsePacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::AuthPacket(error) => write!(f, "OK packet error: {error}"),
            Self::InvalidPayloadLength { actual, expected } => {
                write!(f, "response payload is {actual} bytes, expected {expected}")
            }
            Self::UnexpectedMarker { actual, expected } => {
                write!(
                    f,
                    "response marker is 0x{actual:02x}, expected 0x{expected:02x}"
                )
            }
            Self::InvalidOkHeader { actual } => {
                write!(f, "invalid OK packet header 0x{actual:02x}")
            }
            Self::InvalidFixedLength => {
                f.write_str("column definition fixed-length field is invalid")
            }
            Self::NonZeroFiller => f.write_str("reserved filler must be zero"),
            Self::TruncatedField { field } => write!(f, "{field} is truncated"),
            Self::TrailingBytes { remaining } => {
                write!(f, "response has {remaining} trailing bytes")
            }
            Self::InvalidLengthEncodedInteger { field, marker } => {
                write!(
                    f,
                    "{field} has invalid length-encoded marker 0x{marker:02x}"
                )
            }
            Self::NullLengthEncodedInteger { field } => {
                write!(f, "{field} cannot be a NULL length-encoded integer")
            }
            Self::NonCanonicalLengthEncodedInteger { field, value } => {
                write!(f, "{field} uses a non-canonical encoding for {value}")
            }
            Self::LengthTooLarge { field, length } => {
                write!(f, "{field} length {length} cannot be indexed")
            }
            Self::InvalidUtf8 { field } => write!(f, "{field} is not valid UTF-8"),
            Self::EmbeddedNul { field, offset } => {
                write!(f, "{field} contains an embedded NUL at byte {offset}")
            }
            Self::FieldTooLong {
                field,
                length,
                limit,
            } => {
                write!(f, "{field} is {length} bytes, limit is {limit}")
            }
            Self::PayloadTooLarge { length, limit } => {
                write!(f, "response payload {length} exceeds limit {limit}")
            }
            Self::InvalidSqlState => {
                f.write_str("SQLSTATE must contain five ASCII alphanumeric bytes")
            }
            Self::ColumnCountOutOfRange { count, limit } => {
                write!(f, "column count {count} exceeds limit {limit}")
            }
            Self::MissingCapability { capability } => {
                write!(f, "required capability 0x{capability:08x} is absent")
            }
            Self::UnexpectedSequenceId { expected, actual } => {
                write!(f, "response sequence id is {actual}, expected {expected}")
            }
            Self::BinaryIntegerOutOfRange { value, column_type } => write!(
                f,
                "signed integer {value} does not fit binary-row type {column_type:?}"
            ),
            Self::BinaryIntegerTypeMismatch { column_type } => write!(
                f,
                "signed integer cannot use non-integer binary-row type {column_type:?}"
            ),
        }
    }
}

impl Error for ResponsePacketError {}

fn validate_ok_header(header: u8) -> Result<(), ResponsePacketError> {
    if matches!(header, RESPONSE_OK_HEADER | RESPONSE_OK_TERMINATOR_HEADER) {
        Ok(())
    } else {
        Err(ResponsePacketError::InvalidOkHeader { actual: header })
    }
}

fn validate_sql_state(sql_state: &[u8; 5]) -> Result<(), ResponsePacketError> {
    if sql_state.iter().all(|byte| byte.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(ResponsePacketError::InvalidSqlState)
    }
}

fn validate_column_count(column_count: usize) -> Result<(), ResponsePacketError> {
    if column_count == 0 || column_count > MAX_RESULT_COLUMNS {
        return Err(ResponsePacketError::ColumnCountOutOfRange {
            count: column_count as u64,
            limit: MAX_RESULT_COLUMNS,
        });
    }
    Ok(())
}

fn validate_column_text(
    value: &str,
    field: &'static str,
    required: bool,
) -> Result<(), ResponsePacketError> {
    if required && value.is_empty() {
        return Err(ResponsePacketError::TruncatedField { field });
    }
    if value.len() > MAX_COLUMN_TEXT_LENGTH {
        return Err(ResponsePacketError::FieldTooLong {
            field,
            length: value.len(),
            limit: MAX_COLUMN_TEXT_LENGTH,
        });
    }
    if let Some(offset) = value.as_bytes().iter().position(|byte| *byte == 0) {
        return Err(ResponsePacketError::EmbeddedNul { field, offset });
    }
    Ok(())
}

fn check_response_payload_length(length: usize) -> Result<(), ResponsePacketError> {
    if length > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
        return Err(ResponsePacketError::PayloadTooLarge {
            length,
            limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
        });
    }
    Ok(())
}

/// Encodes one canonical length-encoded integer.
pub fn encode_lenenc_integer(value: u64) -> Vec<u8> {
    let mut output = Vec::with_capacity(lenenc_integer_len(value));
    match value {
        0..=250 => output.push(value as u8),
        251..=65_535 => {
            output.push(0xfc);
            output.extend_from_slice(&(value as u16).to_le_bytes());
        }
        65_536..=0x00ff_ffff => {
            output.push(0xfd);
            output.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
        }
        value => {
            output.push(0xfe);
            output.extend_from_slice(&value.to_le_bytes());
        }
    }
    output
}

/// Decodes one canonical length-encoded integer and returns `(value, bytes_used)`.
pub fn decode_lenenc_integer(input: &[u8]) -> Result<(u64, usize), ResponsePacketError> {
    let mut reader = ResponseReader::new(input);
    let value = reader.read_lenenc_integer("length-encoded integer")?;
    Ok((value, reader.offset))
}

fn lenenc_integer_len(value: u64) -> usize {
    match value {
        0..=250 => 1,
        251..=65_535 => 3,
        65_536..=0x00ff_ffff => 4,
        _ => 9,
    }
}

fn push_lenenc_bytes(payload: &mut Vec<u8>, bytes: &[u8]) {
    payload.extend_from_slice(&encode_lenenc_integer(bytes.len() as u64));
    payload.extend_from_slice(bytes);
}

fn binary_row_null_bitmap_len(column_count: usize) -> usize {
    (column_count + 9) / 8
}

fn binary_row_value_encoded_len(value: BinaryRowValue<'_>) -> Result<usize, ResponsePacketError> {
    match value {
        BinaryRowValue::Null => Ok(0),
        BinaryRowValue::Int8(_) => Ok(1),
        BinaryRowValue::Int16(_) => Ok(2),
        BinaryRowValue::Int24(value) => {
            validate_int24_value(value)?;
            Ok(4)
        }
        BinaryRowValue::Int32(_) => Ok(4),
        BinaryRowValue::Int64(_) | BinaryRowValue::Float64(_) => Ok(8),
        BinaryRowValue::Bytes(bytes) => binary_row_lenenc_value_len(bytes.len()),
        BinaryRowValue::String(value) => binary_row_lenenc_value_len(value.len()),
    }
}

fn validate_int24_value(value: i32) -> Result<(), ResponsePacketError> {
    if (-8_388_608..=8_388_607).contains(&value) {
        Ok(())
    } else {
        Err(ResponsePacketError::BinaryIntegerOutOfRange {
            value: i64::from(value),
            column_type: BinaryRowColumnType::Int24,
        })
    }
}

fn binary_row_lenenc_value_len(length: usize) -> Result<usize, ResponsePacketError> {
    if length > MAX_BINARY_ROW_VALUE_LENGTH {
        return Err(ResponsePacketError::FieldTooLong {
            field: "binary-row value",
            length,
            limit: MAX_BINARY_ROW_VALUE_LENGTH,
        });
    }
    lenenc_integer_len(length as u64).checked_add(length).ok_or(
        ResponsePacketError::PayloadTooLarge {
            length: usize::MAX,
            limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
        },
    )
}

fn set_binary_row_null(null_bitmap_payload: &mut [u8], null_bitmap_offset: usize, column: usize) {
    let bit = column + 2;
    null_bitmap_payload[null_bitmap_offset + bit / 8] |= 1 << (bit % 8);
}

fn binary_row_is_null(null_bitmap: &[u8], column: usize) -> bool {
    let bit = column + 2;
    null_bitmap[bit / 8] & (1 << (bit % 8)) != 0
}

struct ResponseReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ResponseReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn read_u8(&mut self, field: &'static str) -> Result<u8, ResponsePacketError> {
        let value = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or(ResponsePacketError::TruncatedField { field })?;
        self.offset += 1;
        Ok(value)
    }

    fn read_u16(&mut self, field: &'static str) -> Result<u16, ResponsePacketError> {
        let bytes = self.read_exact(2, field)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self, field: &'static str) -> Result<u32, ResponsePacketError> {
        let bytes = self.read_exact(4, field)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i8(&mut self, field: &'static str) -> Result<i8, ResponsePacketError> {
        Ok(i8::from_le_bytes([self.read_u8(field)?]))
    }

    fn read_i16(&mut self, field: &'static str) -> Result<i16, ResponsePacketError> {
        let bytes = self.read_exact(2, field)?;
        Ok(i16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_i32(&mut self, field: &'static str) -> Result<i32, ResponsePacketError> {
        let bytes = self.read_exact(4, field)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i64(&mut self, field: &'static str) -> Result<i64, ResponsePacketError> {
        let bytes = self.read_exact(8, field)?;
        Ok(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_f64(&mut self, field: &'static str) -> Result<f64, ResponsePacketError> {
        let bytes = self.read_exact(8, field)?;
        Ok(f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_exact(
        &mut self,
        length: usize,
        field: &'static str,
    ) -> Result<&'a [u8], ResponsePacketError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(ResponsePacketError::TruncatedField { field })?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(ResponsePacketError::TruncatedField { field })?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_lenenc_integer(&mut self, field: &'static str) -> Result<u64, ResponsePacketError> {
        let marker = self.read_u8(field)?;
        match marker {
            value @ 0..=250 => Ok(u64::from(value)),
            0xfb => Err(ResponsePacketError::NullLengthEncodedInteger { field }),
            0xfc => {
                let value = u64::from(self.read_u16(field)?);
                if value < 251 {
                    return Err(ResponsePacketError::NonCanonicalLengthEncodedInteger {
                        field,
                        value,
                    });
                }
                Ok(value)
            }
            0xfd => {
                let bytes = self.read_exact(3, field)?;
                let value =
                    u64::from(bytes[0]) | (u64::from(bytes[1]) << 8) | (u64::from(bytes[2]) << 16);
                if value <= 65_535 {
                    return Err(ResponsePacketError::NonCanonicalLengthEncodedInteger {
                        field,
                        value,
                    });
                }
                Ok(value)
            }
            0xfe => {
                let bytes = self.read_exact(8, field)?;
                let value = u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]);
                if value <= 0x00ff_ffff {
                    return Err(ResponsePacketError::NonCanonicalLengthEncodedInteger {
                        field,
                        value,
                    });
                }
                Ok(value)
            }
            marker => Err(ResponsePacketError::InvalidLengthEncodedInteger { field, marker }),
        }
    }

    fn read_bytes(
        &mut self,
        field: &'static str,
        limit: usize,
    ) -> Result<&'a [u8], ResponsePacketError> {
        let length = self.read_lenenc_integer(field)?;
        let length = usize::try_from(length)
            .map_err(|_| ResponsePacketError::LengthTooLarge { field, length })?;
        if length > limit {
            return Err(ResponsePacketError::FieldTooLong {
                field,
                length,
                limit,
            });
        }
        self.read_exact(length, field)
    }

    fn read_text(
        &mut self,
        field: &'static str,
        required: bool,
    ) -> Result<String, ResponsePacketError> {
        let bytes = self.read_bytes(field, MAX_COLUMN_TEXT_LENGTH)?;
        if required && bytes.is_empty() {
            return Err(ResponsePacketError::TruncatedField { field });
        }
        if let Some(offset) = bytes.iter().position(|byte| *byte == 0) {
            return Err(ResponsePacketError::EmbeddedNul { field, offset });
        }
        str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ResponsePacketError::InvalidUtf8 { field })
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    fn finish(&self) -> Result<(), ResponsePacketError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ResponsePacketError::TrailingBytes {
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PACKET_HEADER_LEN;

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
    };

    #[test]
    fn encodes_exact_stmt_prepare_ok_payload() {
        let config = StmtPrepareOkPacketConfig::new(0x0102_0304, 0x0506, 0x0708, 0x090a);
        let frame = config.encode(CODEC, 11).unwrap();
        assert_eq!(
            frame,
            [
                0x0c, 0x00, 0x00, 0x0b, // packet header
                0x00, // status
                0x04, 0x03, 0x02, 0x01, // statement id
                0x06, 0x05, // number of columns
                0x08, 0x07, // number of parameters
                0x00, // reserved filler
                0x0a, 0x09, // warning count
            ]
        );
        assert_eq!(
            StmtPrepareOkPacket::decode(CODEC, &frame).unwrap(),
            StmtPrepareOkPacket {
                sequence_id: 11,
                statement_id: 0x0102_0304,
                num_columns: 0x0506,
                num_params: 0x0708,
                warning_count: 0x090a,
            }
        );
    }

    #[test]
    fn rejects_malformed_stmt_prepare_ok_packets() {
        let config = StmtPrepareOkPacketConfig::new(1, 2, 3, 4);
        let frame = config.encode(CODEC, 1).unwrap();

        let truncated = &frame[..frame.len() - 1];
        assert_eq!(
            StmtPrepareOkPacket::decode(CODEC, truncated),
            Err(ResponsePacketError::PacketCodec(
                crate::PacketCodecError::TruncatedPayload {
                    declared: STMT_PREPARE_OK_PAYLOAD_LENGTH,
                    actual: STMT_PREPARE_OK_PAYLOAD_LENGTH - 1,
                }
            ))
        );

        let mut nonzero_status = frame.clone();
        nonzero_status[PACKET_HEADER_LEN] = 0xff;
        assert_eq!(
            StmtPrepareOkPacket::decode(CODEC, &nonzero_status),
            Err(ResponsePacketError::UnexpectedMarker {
                actual: 0xff,
                expected: STMT_PREPARE_OK_HEADER,
            })
        );

        let mut nonzero_filler = frame;
        nonzero_filler[PACKET_HEADER_LEN + 9] = 1;
        assert_eq!(
            StmtPrepareOkPacket::decode(CODEC, &nonzero_filler),
            Err(ResponsePacketError::NonZeroFiller)
        );
    }

    #[test]
    fn encodes_and_decodes_protocol_41_ok_and_err_packets() {
        let ok = OkPacketConfig::new(1, 2, 2, 0).encode(CODEC, 7).unwrap();
        assert_eq!(OkPacket::decode(CODEC, &ok).unwrap().sequence_id, 7);

        let config = ErrPacketConfig {
            error_code: 1064,
            sql_state: *b"42000",
            message: b"syntax error".to_vec(),
        };
        let frame = config.encode(CODEC, 8, CLIENT_PROTOCOL_41).unwrap();
        let packet = ErrPacket::decode(CODEC, &frame, CLIENT_PROTOCOL_41).unwrap();
        assert_eq!(packet.sql_state, Some(*b"42000"));
        assert_eq!(packet.message, b"syntax error");
    }

    #[test]
    fn maps_frontend_categories_without_message_matching() {
        let cases = [
            (FrontendErrorKind::Syntax, 1064, *b"42000"),
            (FrontendErrorKind::UnknownDatabase, 1049, *b"42000"),
            (FrontendErrorKind::NoDatabaseSelected, 1046, *b"3D000"),
            (FrontendErrorKind::DuplicateDatabase, 1007, *b"HY000"),
            (FrontendErrorKind::DatabaseBusy, 1205, *b"HY000"),
            (FrontendErrorKind::Internal, 1105, *b"HY000"),
            (FrontendErrorKind::MissingObject, 1146, *b"42S02"),
            (FrontendErrorKind::UnknownView, 1051, *b"42S02"),
            (FrontendErrorKind::NotView, 1347, *b"HY000"),
            (FrontendErrorKind::UnknownPreparedStatement, 1243, *b"HY000"),
            (
                FrontendErrorKind::PreparedStatementLimitReached,
                1461,
                *b"42000",
            ),
            (FrontendErrorKind::DuplicateObject, 1050, *b"42S01"),
            (FrontendErrorKind::ConstraintViolation, 1062, *b"23000"),
            (FrontendErrorKind::MissingRequiredDefault, 1364, *b"HY000"),
            (FrontendErrorKind::QueryTimeout, 3024, *b"HY000"),
            (FrontendErrorKind::Unsupported, 1235, *b"42000"),
            (FrontendErrorKind::Authentication, 1045, *b"28000"),
            (FrontendErrorKind::AccessDenied, 1045, *b"28000"),
        ];
        for (kind, error_code, sql_state) in cases {
            let mapped = map_frontend_error(kind);
            assert_eq!(mapped.error_code, error_code);
            assert_eq!(mapped.sql_state, sql_state);
        }
    }

    #[test]
    fn default_column_definition_is_accepted_by_the_mysql_driver() {
        use mysql_common::{io::ParseBuf, packets::Column, proto::MyDeserialize};

        let definition = ColumnDefinitionConfig::new("@@max_allowed_packet", 8);
        let frame = definition.encode(CODEC, 2).unwrap();
        let payload = CODEC.decode(&frame).unwrap().payload;
        assert_eq!(&payload[..4], b"\x03def");
        let column = Column::deserialize((), &mut ParseBuf(payload)).unwrap();
        assert_eq!(column.name_str(), "@@max_allowed_packet");
    }

    #[test]
    fn result_set_packets_preserve_binary_values_and_nulls() {
        let count = ColumnCountPacket::encode(CODEC, 0, 2).unwrap();
        assert_eq!(
            ColumnCountPacket::decode(CODEC, &count)
                .unwrap()
                .column_count,
            2
        );
        let definition = ColumnDefinitionConfig::new("payload", 0xfd);
        let definition_frame = definition.encode(CODEC, 1).unwrap();
        assert_eq!(
            ColumnDefinitionPacket::decode(CODEC, &definition_frame)
                .unwrap()
                .name,
            "payload"
        );
        let values = [TextRowValue::Bytes(b"\xff\0binary"), TextRowValue::Null];
        let row = TextRowPacket::encode(CODEC, 2, &values).unwrap();
        assert_eq!(
            TextRowPacket::decode(CODEC, &row, 2).unwrap().values,
            values
        );
    }

    #[test]
    fn binary_rows_use_the_two_bit_null_offset_and_round_trip_values() {
        let values = [
            BinaryRowValue::Int64(-2),
            BinaryRowValue::Null,
            BinaryRowValue::Float64(1.5),
            BinaryRowValue::Bytes(b"\xff\0"),
            BinaryRowValue::String("hi"),
        ];
        let frame = BinaryRowPacket::encode(CODEC, 9, &values).unwrap();
        assert_eq!(
            frame,
            [
                0x18, 0x00, 0x00, 0x09, // packet header
                0x00, // binary row header
                0x08, // column 1 uses NULL bitmap bit 1 + 2
                0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // -2i64
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0x3f, // 1.5f64
                0x02, 0xff, 0x00, // bytes
                0x02, b'h', b'i', // string
            ]
        );
        let types = [
            BinaryRowColumnType::Int64,
            BinaryRowColumnType::Int64,
            BinaryRowColumnType::Float64,
            BinaryRowColumnType::Bytes,
            BinaryRowColumnType::String,
        ];
        assert_eq!(
            BinaryRowPacket::decode(CODEC, &frame, &types).unwrap(),
            BinaryRowPacket {
                sequence_id: 9,
                values: values.to_vec(),
            }
        );
    }

    #[test]
    fn binary_rows_encode_signed_i64_extrema_in_little_endian_order() {
        let values = [
            BinaryRowValue::Int64(i64::MIN),
            BinaryRowValue::Int64(i64::MAX),
        ];
        let frame = BinaryRowPacket::encode(CODEC, 9, &values).unwrap();
        assert_eq!(
            CODEC.decode(&frame).unwrap().payload,
            [
                0x00, // binary row header
                0x00, // no NULL values
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, // i64::MIN
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, // i64::MAX
            ]
        );
        assert_eq!(
            BinaryRowPacket::decode(
                CODEC,
                &frame,
                &[BinaryRowColumnType::Int64, BinaryRowColumnType::Int64],
            )
            .unwrap()
            .values,
            values
        );
    }

    #[test]
    fn binary_rows_encode_signed_integer_widths_in_little_endian_order() {
        let values = [
            BinaryRowValue::Int8(-2),
            BinaryRowValue::Int16(0x1234),
            BinaryRowValue::Int24(-2),
            BinaryRowValue::Int32(-2),
            BinaryRowValue::Int64(0x0102_0304_0506_0708),
        ];
        let frame = BinaryRowPacket::encode(CODEC, 9, &values).unwrap();
        assert_eq!(
            CODEC.decode(&frame).unwrap().payload,
            [
                0x00, // binary row header
                0x00, // no NULL values
                0xfe, // -2i8
                0x34, 0x12, // 0x1234i16
                0xfe, 0xff, 0xff, 0xff, // -2i24
                0xfe, 0xff, 0xff, 0xff, // -2i32
                0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // i64
            ]
        );
        let types = [
            BinaryRowColumnType::Int8,
            BinaryRowColumnType::Int16,
            BinaryRowColumnType::Int24,
            BinaryRowColumnType::Int32,
            BinaryRowColumnType::Int64,
        ];
        assert_eq!(
            BinaryRowPacket::decode(CODEC, &frame, &types)
                .unwrap()
                .values,
            values
        );
    }

    #[test]
    fn binary_rows_round_trip_signed_integer_boundaries() {
        let values = [
            BinaryRowValue::Int8(i8::MIN),
            BinaryRowValue::Int8(i8::MAX),
            BinaryRowValue::Int16(i16::MIN),
            BinaryRowValue::Int16(i16::MAX),
            BinaryRowValue::Int32(i32::MIN),
            BinaryRowValue::Int32(i32::MAX),
        ];
        let frame = BinaryRowPacket::encode(CODEC, 9, &values).unwrap();
        let types = [
            BinaryRowColumnType::Int8,
            BinaryRowColumnType::Int8,
            BinaryRowColumnType::Int16,
            BinaryRowColumnType::Int16,
            BinaryRowColumnType::Int32,
            BinaryRowColumnType::Int32,
        ];
        assert_eq!(
            BinaryRowPacket::decode(CODEC, &frame, &types)
                .unwrap()
                .values,
            values
        );
    }

    #[test]
    fn binary_rows_check_int24_range_and_null_bitmap() {
        let values = [
            BinaryRowValue::Int24(-8_388_608),
            BinaryRowValue::Int24(8_388_607),
            BinaryRowValue::Null,
        ];
        let frame = BinaryRowPacket::encode(CODEC, 9, &values).unwrap();
        assert_eq!(
            CODEC.decode(&frame).unwrap().payload,
            [
                0x00, // binary row header
                0x10, // column 2 uses NULL bitmap bit 4
                0x00, 0x00, 0x80, 0xff, // signed INT24 minimum in 4 bytes
                0xff, 0xff, 0x7f, 0x00, // signed INT24 maximum in 4 bytes
            ]
        );
        assert_eq!(
            BinaryRowPacket::decode(
                CODEC,
                &frame,
                &[
                    BinaryRowColumnType::Int24,
                    BinaryRowColumnType::Int24,
                    BinaryRowColumnType::Int24,
                ],
            )
            .unwrap()
            .values,
            values
        );

        for value in [-8_388_609i32, 8_388_608] {
            assert_eq!(
                BinaryRowValue::try_from_signed_integer(
                    i64::from(value),
                    BinaryRowColumnType::Int24,
                ),
                Err(ResponsePacketError::BinaryIntegerOutOfRange {
                    value: i64::from(value),
                    column_type: BinaryRowColumnType::Int24,
                })
            );
            assert_eq!(
                BinaryRowPacket::encode(CODEC, 9, &[BinaryRowValue::Int24(value)]),
                Err(ResponsePacketError::BinaryIntegerOutOfRange {
                    value: i64::from(value),
                    column_type: BinaryRowColumnType::Int24,
                })
            );
        }

        let invalid = CODEC
            .encode(9, &[0x00, 0x00, 0x00, 0x00, 0x80, 0x00])
            .unwrap();
        assert_eq!(
            BinaryRowPacket::decode(CODEC, &invalid, &[BinaryRowColumnType::Int24]),
            Err(ResponsePacketError::BinaryIntegerOutOfRange {
                value: 8_388_608,
                column_type: BinaryRowColumnType::Int24,
            })
        );
    }

    #[test]
    fn binary_rows_preserve_null_bitmap_with_mixed_integer_widths() {
        let values = [
            BinaryRowValue::Int8(1),
            BinaryRowValue::Null,
            BinaryRowValue::Int16(-2),
            BinaryRowValue::Null,
            BinaryRowValue::Int32(3),
            BinaryRowValue::Int64(4),
        ];
        let frame = BinaryRowPacket::encode(CODEC, 9, &values).unwrap();
        assert_eq!(
            CODEC.decode(&frame).unwrap().payload,
            [
                0x00, // binary row header
                0x28, // columns 1 and 3 use NULL bitmap bits 3 and 5
                0x01, // i8
                0xfe, 0xff, // -2i16
                0x03, 0x00, 0x00, 0x00, // i32
                0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // i64
            ]
        );
        let types = [
            BinaryRowColumnType::Int8,
            BinaryRowColumnType::Int16,
            BinaryRowColumnType::Int16,
            BinaryRowColumnType::Int32,
            BinaryRowColumnType::Int32,
            BinaryRowColumnType::Int64,
        ];
        assert_eq!(
            BinaryRowPacket::decode(CODEC, &frame, &types)
                .unwrap()
                .values,
            values
        );
    }

    #[test]
    fn binary_rows_check_signed_integer_narrowing() {
        for (value, column_type, expected) in [
            (
                i8::MIN as i64,
                BinaryRowColumnType::Int8,
                BinaryRowValue::Int8(i8::MIN),
            ),
            (
                i8::MAX as i64,
                BinaryRowColumnType::Int8,
                BinaryRowValue::Int8(i8::MAX),
            ),
            (
                i16::MIN as i64,
                BinaryRowColumnType::Int16,
                BinaryRowValue::Int16(i16::MIN),
            ),
            (
                i16::MAX as i64,
                BinaryRowColumnType::Int16,
                BinaryRowValue::Int16(i16::MAX),
            ),
            (
                i32::MIN as i64,
                BinaryRowColumnType::Int32,
                BinaryRowValue::Int32(i32::MIN),
            ),
            (
                i32::MAX as i64,
                BinaryRowColumnType::Int32,
                BinaryRowValue::Int32(i32::MAX),
            ),
        ] {
            assert_eq!(
                BinaryRowValue::try_from_signed_integer(value, column_type),
                Ok(expected)
            );
        }

        for (value, column_type) in [
            (-129, BinaryRowColumnType::Int8),
            (128, BinaryRowColumnType::Int8),
            (-32_769, BinaryRowColumnType::Int16),
            (32_768, BinaryRowColumnType::Int16),
            (i64::from(i32::MIN) - 1, BinaryRowColumnType::Int32),
            (i64::from(i32::MAX) + 1, BinaryRowColumnType::Int32),
        ] {
            assert_eq!(
                BinaryRowValue::try_from_signed_integer(value, column_type),
                Err(ResponsePacketError::BinaryIntegerOutOfRange { value, column_type })
            );
        }

        assert_eq!(
            BinaryRowValue::try_from_signed_integer(i64::MIN, BinaryRowColumnType::Int64),
            Ok(BinaryRowValue::Int64(i64::MIN))
        );
        assert_eq!(
            BinaryRowValue::try_from_signed_integer(42, BinaryRowColumnType::Float64),
            Err(ResponsePacketError::BinaryIntegerTypeMismatch {
                column_type: BinaryRowColumnType::Float64,
            })
        );
    }

    #[test]
    fn binary_row_null_bitmap_uses_the_offset_across_bytes() {
        let values = [BinaryRowValue::Null; 7];
        assert_eq!(
            BinaryRowPacket::encode(CODEC, 4, &values).unwrap(),
            [
                0x03, 0x00, 0x00, 0x04, // packet header
                0x00, // binary row header
                0xfc, 0x01, // columns 0 through 6 map to bits 2 through 8
            ]
        );
    }

    #[test]
    fn binary_rows_reject_invalid_column_counts_and_oversized_payloads() {
        assert_eq!(
            BinaryRowPacket::encode(CODEC, 0, &[]),
            Err(ResponsePacketError::ColumnCountOutOfRange {
                count: 0,
                limit: MAX_RESULT_COLUMNS,
            })
        );
        let too_many_values = vec![BinaryRowValue::Null; MAX_RESULT_COLUMNS + 1];
        assert_eq!(
            BinaryRowPacket::encode(CODEC, 0, &too_many_values),
            Err(ResponsePacketError::ColumnCountOutOfRange {
                count: (MAX_RESULT_COLUMNS + 1) as u64,
                limit: MAX_RESULT_COLUMNS,
            })
        );
        let bytes = vec![b'x'; MAX_RESPONSE_PACKET_PAYLOAD_LENGTH - 2];
        assert_eq!(
            BinaryRowPacket::encode(CODEC, 0, &[BinaryRowValue::Bytes(&bytes)]),
            Err(ResponsePacketError::PayloadTooLarge {
                length: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH + 3,
                limit: MAX_RESPONSE_PACKET_PAYLOAD_LENGTH,
            })
        );
        assert_eq!(
            BinaryRowPacket::decode(CODEC, &[0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], &[]),
            Err(ResponsePacketError::ColumnCountOutOfRange {
                count: 0,
                limit: MAX_RESULT_COLUMNS,
            })
        );
    }

    #[test]
    fn selects_eof_or_ok_terminator_from_capabilities() {
        let eof = ResultTerminatorPacket::encode(CODEC, 3, CLIENT_PROTOCOL_41, 1, 2).unwrap();
        assert!(matches!(
            ResultTerminatorPacket::decode(CODEC, &eof, CLIENT_PROTOCOL_41).unwrap(),
            ResultTerminatorPacket::Eof(EofPacket { sequence_id: 3, .. })
        ));
        let ok = ResultTerminatorPacket::encode(
            CODEC,
            4,
            CLIENT_PROTOCOL_41 | CLIENT_DEPRECATE_EOF,
            1,
            2,
        )
        .unwrap();
        assert_eq!(ok[PACKET_HEADER_LEN], RESPONSE_OK_TERMINATOR_HEADER);
        assert!(matches!(
            ResultTerminatorPacket::decode(CODEC, &ok, CLIENT_PROTOCOL_41 | CLIENT_DEPRECATE_EOF)
                .unwrap(),
            ResultTerminatorPacket::Ok(OkPacket {
                sequence_id: 4,
                header: RESPONSE_OK_TERMINATOR_HEADER,
                ..
            })
        ));
        let ordinary_ok = OkPacketConfig::new(0, 0, 2, 1).encode(CODEC, 5).unwrap();
        assert!(matches!(
            ResultTerminatorPacket::decode(
                CODEC,
                &ordinary_ok,
                CLIENT_PROTOCOL_41 | CLIENT_DEPRECATE_EOF
            ),
            Err(ResponsePacketError::UnexpectedMarker {
                actual: RESPONSE_OK_HEADER,
                expected: RESPONSE_OK_TERMINATOR_HEADER,
            })
        ));
    }

    #[test]
    fn rejects_noncanonical_lengths_oversized_rows_and_sequence_errors() {
        assert!(matches!(
            decode_lenenc_integer(&[0xfc, 1, 0]),
            Err(ResponsePacketError::NonCanonicalLengthEncodedInteger { value: 1, .. })
        ));
        let oversized = vec![b'x'; MAX_RESPONSE_PACKET_PAYLOAD_LENGTH + 1];
        let large_codec = PacketCodec::new(MAX_RESPONSE_PACKET_PAYLOAD_LENGTH + 1).unwrap();
        let frame = large_codec.encode(2, &oversized).unwrap();
        assert!(matches!(
            TextRowPacket::decode(large_codec, &frame, 1),
            Err(ResponsePacketError::PayloadTooLarge { length, limit })
                if length == MAX_RESPONSE_PACKET_PAYLOAD_LENGTH + 1
                    && limit == MAX_RESPONSE_PACKET_PAYLOAD_LENGTH
        ));
        let mut sequence = PacketSequence::new(MAX_PACKET_SEQUENCE_ID);
        sequence.accept(MAX_PACKET_SEQUENCE_ID).unwrap();
        assert_eq!(sequence.expected(), 0);
        assert!(matches!(
            sequence.accept(2),
            Err(ResponsePacketError::UnexpectedSequenceId {
                expected: 0,
                actual: 2
            })
        ));
    }
}
