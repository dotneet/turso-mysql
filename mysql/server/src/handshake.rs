//! The bounded MySQL 8 initial handshake packet.

use std::{error::Error, fmt, str};

use crate::{PacketCodec, PacketCodecError};

/// MySQL protocol version used by the v10 initial handshake.
pub const PROTOCOL_VERSION_10: u8 = 10;
/// Number of bytes in the authentication plugin data.
pub const AUTH_PLUGIN_DATA_LENGTH: usize = 20;
/// Number of bytes in the first authentication plugin data part.
pub const AUTH_PLUGIN_DATA_PART_1_LENGTH: usize = 8;
/// Number of bytes in the second authentication plugin data part.
pub const AUTH_PLUGIN_DATA_PART_2_LENGTH: usize =
    AUTH_PLUGIN_DATA_LENGTH - AUTH_PLUGIN_DATA_PART_1_LENGTH;
/// Length advertised for 20 authentication bytes and their terminating NUL.
pub const AUTH_PLUGIN_DATA_WIRE_LENGTH: u8 = (AUTH_PLUGIN_DATA_LENGTH + 1) as u8;
/// Maximum server version string accepted by this handshake model.
pub const MAX_SERVER_VERSION_LENGTH: usize = u8::MAX as usize;
/// Maximum authentication plugin name accepted by this handshake model.
pub const MAX_AUTH_PLUGIN_NAME_LENGTH: usize = u8::MAX as usize;
/// Maximum payload accepted by this handshake model.
pub const MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH: usize = 4096;
/// The default utf8mb4 collation used by this UTF-8-only server slice.
pub const DEFAULT_UTF8MB4_COLLATION: u8 = 45;

/// Capability bit for the protocol 4.1 status and capability fields.
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
/// Capability bit requesting matched rather than changed affected-row counts.
///
/// This is a client capability. The server advertises support for it in its
/// initial handshake, and the negotiated bit is retained for the lifetime of
/// the authenticated command executor.
pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
/// Capability bit requesting the classic protocol TLS upgrade.
pub const CLIENT_SSL: u32 = 0x0000_0800;
/// Capability bit for the second authentication data part.
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
/// Capability bit for authentication plugin negotiation.
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
/// Capability bit for length-encoded authentication responses.
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
/// Capability bit for multiple result sets.
pub const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
/// Capability bit for prepared-statement multiple result sets.
pub const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
/// Capability bit selecting OK packets instead of EOF result terminators.
pub const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;
/// Capability bit for the classic zlib stream compression.
pub const CLIENT_COMPRESS: u32 = 0x0000_0020;
/// Capability bit for zstd stream compression.
pub const CLIENT_ZSTD_COMPRESSION_ALGORITHM: u32 = 0x0400_0000;
/// Capabilities required by this complete MySQL 8 handshake layout.
pub const REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES: u32 =
    CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;

/// Failure to obtain a fresh server authentication nonce from the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialHandshakeNonceError {
    /// The operating system random source was unavailable.
    OsRandomUnavailable,
}

impl fmt::Display for InitialHandshakeNonceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OsRandomUnavailable => f.write_str("OS random source unavailable"),
        }
    }
}

impl Error for InitialHandshakeNonceError {}

/// Immutable server settings shared by connections.
///
/// Authentication plugin data is deliberately not part of this type. A
/// [`crate::ClassicConnection`] creates fresh per-connection plugin data when
/// it is constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialHandshakeSettings {
    /// NUL-terminated server version advertised to the client.
    pub server_version: String,
    /// Server connection identifier.
    pub connection_id: u32,
    /// Combined lower and upper capability fields.
    pub capability_flags: u32,
    /// Default character set/collation identifier.
    pub character_set: u8,
    /// Initial server status flags.
    pub status_flags: u16,
    /// NUL-terminated authentication plugin name.
    pub auth_plugin_name: String,
}

impl InitialHandshakeSettings {
    /// Creates immutable settings for future per-connection handshakes.
    pub fn new(
        server_version: impl Into<String>,
        connection_id: u32,
        capability_flags: u32,
        character_set: u8,
        status_flags: u16,
        auth_plugin_name: impl Into<String>,
    ) -> Self {
        Self {
            server_version: server_version.into(),
            connection_id,
            capability_flags,
            character_set,
            status_flags,
            auth_plugin_name: auth_plugin_name.into(),
        }
    }

    pub(crate) fn with_auth_plugin_data(
        self,
        auth_plugin_data: [u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> InitialHandshakeConfig {
        InitialHandshakeConfig {
            server_version: self.server_version,
            connection_id: self.connection_id,
            capability_flags: self.capability_flags,
            character_set: self.character_set,
            status_flags: self.status_flags,
            auth_plugin_data,
            auth_plugin_name: self.auth_plugin_name,
        }
    }
}

impl Default for InitialHandshakeSettings {
    fn default() -> Self {
        Self::new(
            "8.0.0-turso",
            0,
            REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES | CLIENT_FOUND_ROWS,
            DEFAULT_UTF8MB4_COLLATION,
            0x0002,
            "caching_sha2_password",
        )
    }
}

/// The per-connection configuration for an initial handshake v10 packet.
///
/// This type is used by the packet codec. Its authentication plugin data is
/// crate-private so callers cannot supply it to a production connection
/// constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialHandshakeConfig {
    /// NUL-terminated server version advertised to the client.
    pub server_version: String,
    /// Server connection identifier.
    pub connection_id: u32,
    /// Combined lower and upper capability fields.
    pub capability_flags: u32,
    /// Default character set/collation identifier.
    pub character_set: u8,
    /// Initial server status flags.
    pub status_flags: u16,
    /// Twenty bytes of authentication plugin data.
    pub(crate) auth_plugin_data: [u8; AUTH_PLUGIN_DATA_LENGTH],
    /// NUL-terminated authentication plugin name.
    pub auth_plugin_name: String,
}

impl InitialHandshakeConfig {
    /// Returns the lower 16 bits sent in the first capability field.
    pub const fn capability_flags_lower(&self) -> u16 {
        self.capability_flags as u16
    }

    /// Returns the upper 16 bits sent in the second capability field.
    pub const fn capability_flags_upper(&self) -> u16 {
        (self.capability_flags >> 16) as u16
    }

    /// Checks all values that affect the handshake wire layout.
    pub fn validate(&self) -> Result<(), InitialHandshakeError> {
        validate_text(
            &self.server_version,
            "server version",
            MAX_SERVER_VERSION_LENGTH,
        )?;
        validate_character_set(self.character_set)?;
        validate_text(
            &self.auth_plugin_name,
            "authentication plugin name",
            MAX_AUTH_PLUGIN_NAME_LENGTH,
        )?;
        if self.auth_plugin_data.iter().all(|byte| *byte == 0) {
            return Err(InitialHandshakeError::ZeroAuthPluginData);
        }
        validate_capabilities(self.capability_flags)
    }

    /// Encodes this configuration as one framed v10 handshake packet.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, InitialHandshakeError> {
        InitialHandshakeCodec::new(codec).encode(sequence_id, self)
    }
}

/// A source of fresh authentication plugin data for one server connection.
pub(crate) trait HandshakeNonceSource {
    /// Fills one 20-byte nonce from a cryptographically secure source.
    fn fill_nonce(
        &mut self,
        nonce: &mut [u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> Result<(), InitialHandshakeNonceError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct OsHandshakeNonceSource;

impl HandshakeNonceSource for OsHandshakeNonceSource {
    fn fill_nonce(
        &mut self,
        nonce: &mut [u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> Result<(), InitialHandshakeNonceError> {
        getrandom::fill(nonce).map_err(|_| InitialHandshakeNonceError::OsRandomUnavailable)
    }
}

/// A decoded initial handshake v10 packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialHandshake {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Always [`PROTOCOL_VERSION_10`] for this type.
    pub protocol_version: u8,
    /// Server version from the NUL-terminated wire string.
    pub server_version: String,
    /// Server connection identifier.
    pub connection_id: u32,
    /// Lower 16-bit capability field.
    pub capability_flags_lower: u16,
    /// Default character set/collation identifier.
    pub character_set: u8,
    /// Initial server status flags.
    pub status_flags: u16,
    /// Upper 16-bit capability field.
    pub capability_flags_upper: u16,
    /// Twenty bytes of authentication plugin data.
    pub auth_plugin_data: [u8; AUTH_PLUGIN_DATA_LENGTH],
    /// Authentication plugin name from the final NUL-terminated string.
    pub auth_plugin_name: String,
}

impl InitialHandshake {
    /// Decodes one framed v10 handshake packet.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, InitialHandshakeError> {
        InitialHandshakeCodec::new(codec).decode(frame)
    }

    /// Returns both capability fields combined in their wire order.
    pub const fn capability_flags(&self) -> u32 {
        self.capability_flags_lower as u32 | ((self.capability_flags_upper as u32) << 16)
    }

    /// Converts the decoded values to an encoding configuration.
    pub fn to_config(&self) -> InitialHandshakeConfig {
        InitialHandshakeConfig {
            server_version: self.server_version.clone(),
            connection_id: self.connection_id,
            capability_flags: self.capability_flags(),
            character_set: self.character_set,
            status_flags: self.status_flags,
            auth_plugin_data: self.auth_plugin_data,
            auth_plugin_name: self.auth_plugin_name.clone(),
        }
    }

    /// Encodes this handshake's payload as a packet with a new sequence id.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, InitialHandshakeError> {
        let config = self.to_config();
        config.validate()?;
        InitialHandshakeCodec::new(codec).encode(sequence_id, &config)
    }
}

/// Codec for one bounded initial handshake packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitialHandshakeCodec {
    packet_codec: PacketCodec,
}

impl InitialHandshakeCodec {
    /// Creates a handshake codec on top of a packet framing codec.
    pub const fn new(packet_codec: PacketCodec) -> Self {
        Self { packet_codec }
    }

    /// Returns the packet framing codec used by this handshake codec.
    pub const fn packet_codec(self) -> PacketCodec {
        self.packet_codec
    }

    /// Encodes one deterministic v10 handshake packet.
    pub fn encode(
        self,
        sequence_id: u8,
        config: &InitialHandshakeConfig,
    ) -> Result<Vec<u8>, InitialHandshakeError> {
        config.validate()?;
        let payload = encode_payload(config);
        if payload.len() > MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH {
            return Err(InitialHandshakeError::PayloadTooLarge {
                length: payload.len(),
                limit: MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
            });
        }
        self.packet_codec
            .encode(sequence_id, &payload)
            .map_err(InitialHandshakeError::from)
    }

    /// Decodes exactly one framed and complete v10 handshake packet.
    pub fn decode(self, frame: &[u8]) -> Result<InitialHandshake, InitialHandshakeError> {
        let packet = self
            .packet_codec
            .decode(frame)
            .map_err(InitialHandshakeError::from)?;
        if packet.payload.len() > MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH {
            return Err(InitialHandshakeError::PayloadTooLarge {
                length: packet.payload.len(),
                limit: MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
            });
        }
        decode_payload(packet.sequence_id, packet.payload)
    }
}

impl PacketCodec {
    /// Encodes one initial handshake v10 packet using this packet codec.
    pub fn encode_initial_handshake(
        self,
        sequence_id: u8,
        config: &InitialHandshakeConfig,
    ) -> Result<Vec<u8>, InitialHandshakeError> {
        InitialHandshakeCodec::new(self).encode(sequence_id, config)
    }

    /// Decodes one initial handshake v10 packet using this packet codec.
    pub fn decode_initial_handshake(
        self,
        frame: &[u8],
    ) -> Result<InitialHandshake, InitialHandshakeError> {
        InitialHandshakeCodec::new(self).decode(frame)
    }
}

fn encode_payload(config: &InitialHandshakeConfig) -> Vec<u8> {
    let payload_len = 1
        + config.server_version.len()
        + 1
        + 4
        + AUTH_PLUGIN_DATA_PART_1_LENGTH
        + 1
        + 2
        + 1
        + 2
        + 2
        + 1
        + 10
        + AUTH_PLUGIN_DATA_PART_2_LENGTH
        + 1
        + config.auth_plugin_name.len()
        + 1;
    let mut payload = Vec::with_capacity(payload_len);
    payload.push(PROTOCOL_VERSION_10);
    payload.extend_from_slice(config.server_version.as_bytes());
    payload.push(0);
    payload.extend_from_slice(&config.connection_id.to_le_bytes());
    payload.extend_from_slice(&config.auth_plugin_data[..AUTH_PLUGIN_DATA_PART_1_LENGTH]);
    payload.push(0);
    payload.extend_from_slice(&config.capability_flags_lower().to_le_bytes());
    payload.push(config.character_set);
    payload.extend_from_slice(&config.status_flags.to_le_bytes());
    payload.extend_from_slice(&config.capability_flags_upper().to_le_bytes());
    payload.push(AUTH_PLUGIN_DATA_WIRE_LENGTH);
    payload.extend_from_slice(&[0; 10]);
    payload.extend_from_slice(&config.auth_plugin_data[AUTH_PLUGIN_DATA_PART_1_LENGTH..]);
    payload.push(0);
    payload.extend_from_slice(config.auth_plugin_name.as_bytes());
    payload.push(0);
    payload
}

fn decode_payload(
    sequence_id: u8,
    payload: &[u8],
) -> Result<InitialHandshake, InitialHandshakeError> {
    let mut reader = Reader::new(payload);
    let protocol_version = reader.read_u8("protocol version")?;
    if protocol_version != PROTOCOL_VERSION_10 {
        return Err(InitialHandshakeError::InvalidProtocolVersion {
            actual: protocol_version,
        });
    }
    let server_version = reader.read_string("server version", MAX_SERVER_VERSION_LENGTH, false)?;
    let connection_id = reader.read_u32("connection id")?;
    let auth_part_1 = reader.read_exact(AUTH_PLUGIN_DATA_PART_1_LENGTH, "auth plugin data")?;
    let filler = reader.read_u8("filler")?;
    if filler != 0 {
        return Err(InitialHandshakeError::InvalidFiller { actual: filler });
    }
    let capability_flags_lower = reader.read_u16("lower capability flags")?;
    let character_set = reader.read_u8("character set")?;
    validate_character_set(character_set)?;
    let status_flags = reader.read_u16("status flags")?;
    let capability_flags_upper = reader.read_u16("upper capability flags")?;
    let capability_flags =
        u32::from(capability_flags_lower) | (u32::from(capability_flags_upper) << 16);
    validate_capabilities(capability_flags)?;
    let auth_plugin_data_len = reader.read_u8("auth plugin data length")?;
    if auth_plugin_data_len != AUTH_PLUGIN_DATA_WIRE_LENGTH {
        return Err(InitialHandshakeError::InvalidAuthPluginDataLength {
            actual: auth_plugin_data_len,
            expected: AUTH_PLUGIN_DATA_WIRE_LENGTH,
        });
    }
    let reserved = reader.read_exact(10, "reserved bytes")?;
    if reserved.iter().any(|byte| *byte != 0) {
        return Err(InitialHandshakeError::NonZeroReservedBytes);
    }
    let auth_part_2 = reader.read_exact(AUTH_PLUGIN_DATA_PART_2_LENGTH, "auth plugin data")?;
    let auth_terminator = reader.read_u8("auth plugin data terminator")?;
    if auth_terminator != 0 {
        return Err(InitialHandshakeError::MissingTerminator {
            field: "auth plugin data",
        });
    }
    let auth_plugin_name = reader.read_string(
        "authentication plugin name",
        MAX_AUTH_PLUGIN_NAME_LENGTH,
        false,
    )?;
    reader.finish()?;

    let mut auth_plugin_data = [0; AUTH_PLUGIN_DATA_LENGTH];
    auth_plugin_data[..AUTH_PLUGIN_DATA_PART_1_LENGTH].copy_from_slice(auth_part_1);
    auth_plugin_data[AUTH_PLUGIN_DATA_PART_1_LENGTH..].copy_from_slice(auth_part_2);
    Ok(InitialHandshake {
        sequence_id,
        protocol_version,
        server_version,
        connection_id,
        capability_flags_lower,
        character_set,
        status_flags,
        capability_flags_upper,
        auth_plugin_data,
        auth_plugin_name,
    })
}

fn validate_text(
    value: &str,
    field: &'static str,
    limit: usize,
) -> Result<(), InitialHandshakeError> {
    if value.is_empty() {
        return Err(InitialHandshakeError::EmptyField { field });
    }
    if value.len() > limit {
        return Err(InitialHandshakeError::FieldTooLong {
            field,
            length: value.len(),
            limit,
        });
    }
    if let Some(offset) = value.as_bytes().iter().position(|byte| *byte == 0) {
        return Err(InitialHandshakeError::EmbeddedNul { field, offset });
    }
    Ok(())
}

/// Returns whether the collation ID is an explicitly supported utf8mb4 ID.
pub fn is_supported_utf8mb4_collation(character_set: u8) -> bool {
    matches!(character_set, 45 | 46 | 224..=247 | 255)
}

fn validate_character_set(character_set: u8) -> Result<(), InitialHandshakeError> {
    if is_supported_utf8mb4_collation(character_set) {
        Ok(())
    } else {
        Err(InitialHandshakeError::UnsupportedCharacterSet { character_set })
    }
}

fn validate_capabilities(capability_flags: u32) -> Result<(), InitialHandshakeError> {
    let missing = REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES & !capability_flags;
    if missing != 0 {
        return Err(InitialHandshakeError::MissingCapabilities {
            flags: capability_flags,
            missing,
        });
    }
    if capability_flags & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0
        && capability_flags & CLIENT_PLUGIN_AUTH == 0
    {
        return Err(InitialHandshakeError::IncompatibleCapabilities {
            flags: capability_flags,
            capability: CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
            requires: CLIENT_PLUGIN_AUTH,
        });
    }
    if capability_flags & (CLIENT_MULTI_RESULTS | CLIENT_PS_MULTI_RESULTS) != 0
        && capability_flags & CLIENT_PROTOCOL_41 == 0
    {
        return Err(InitialHandshakeError::IncompatibleCapabilities {
            flags: capability_flags,
            capability: CLIENT_MULTI_RESULTS | CLIENT_PS_MULTI_RESULTS,
            requires: CLIENT_PROTOCOL_41,
        });
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_exact(
        &mut self,
        length: usize,
        field: &'static str,
    ) -> Result<&'a [u8], InitialHandshakeError> {
        let remaining = self.bytes.len().saturating_sub(self.offset);
        if remaining < length {
            return Err(InitialHandshakeError::Truncated {
                field,
                needed: length,
                remaining,
            });
        }
        let start = self.offset;
        self.offset += length;
        Ok(&self.bytes[start..self.offset])
    }

    fn read_u8(&mut self, field: &'static str) -> Result<u8, InitialHandshakeError> {
        Ok(self.read_exact(1, field)?[0])
    }

    fn read_u16(&mut self, field: &'static str) -> Result<u16, InitialHandshakeError> {
        let bytes = self.read_exact(2, field)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self, field: &'static str) -> Result<u32, InitialHandshakeError> {
        let bytes = self.read_exact(4, field)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_string(
        &mut self,
        field: &'static str,
        max_length: usize,
        allow_empty: bool,
    ) -> Result<String, InitialHandshakeError> {
        let remaining = &self.bytes[self.offset..];
        let Some(end) = remaining.iter().position(|byte| *byte == 0) else {
            return Err(InitialHandshakeError::MissingTerminator { field });
        };
        if end > max_length {
            return Err(InitialHandshakeError::FieldTooLong {
                field,
                length: end,
                limit: max_length,
            });
        }
        if end == 0 && !allow_empty {
            return Err(InitialHandshakeError::EmptyField { field });
        }
        let value = str::from_utf8(&remaining[..end])
            .map_err(|_| InitialHandshakeError::InvalidUtf8 { field })?
            .to_owned();
        self.offset += end + 1;
        Ok(value)
    }

    fn finish(&self) -> Result<(), InitialHandshakeError> {
        if self.offset != self.bytes.len() {
            return Err(InitialHandshakeError::TrailingBytes {
                remaining: self.bytes.len() - self.offset,
            });
        }
        Ok(())
    }
}

/// Errors returned by the bounded initial handshake codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialHandshakeError {
    /// Packet framing rejected the frame.
    PacketCodec(PacketCodecError),
    /// The payload exceeds this model's independent bound.
    PayloadTooLarge { length: usize, limit: usize },
    /// A required textual field was empty.
    EmptyField { field: &'static str },
    /// A textual field exceeds its protocol-facing bound.
    FieldTooLong {
        field: &'static str,
        length: usize,
        limit: usize,
    },
    /// A configuration string contains a NUL that cannot be represented safely.
    EmbeddedNul { field: &'static str, offset: usize },
    /// A NUL-terminated wire string has no terminator.
    MissingTerminator { field: &'static str },
    /// A wire string is not valid UTF-8.
    InvalidUtf8 { field: &'static str },
    /// A fixed-width field is shorter than its required size.
    Truncated {
        field: &'static str,
        needed: usize,
        remaining: usize,
    },
    /// The packet is not protocol version 10.
    InvalidProtocolVersion { actual: u8 },
    /// The packet filler byte is not zero.
    InvalidFiller { actual: u8 },
    /// The handshake selected a collation outside the supported utf8mb4 set.
    UnsupportedCharacterSet { character_set: u8 },
    /// Required capability bits are absent.
    MissingCapabilities { flags: u32, missing: u32 },
    /// Capability bits have an invalid dependency.
    IncompatibleCapabilities {
        flags: u32,
        capability: u32,
        requires: u32,
    },
    /// The advertised authentication data length is not 21.
    InvalidAuthPluginDataLength { actual: u8, expected: u8 },
    /// Reserved bytes must be zero for this MySQL 8 model.
    NonZeroReservedBytes,
    /// An all-zero authentication nonce would allow challenge replay.
    ZeroAuthPluginData,
    /// The payload has bytes after the final plugin name terminator.
    TrailingBytes { remaining: usize },
}

impl From<PacketCodecError> for InitialHandshakeError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl fmt::Display for InitialHandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::PayloadTooLarge { length, limit } => {
                write!(
                    f,
                    "initial handshake payload {length} exceeds limit {limit}"
                )
            }
            Self::EmptyField { field } => write!(f, "{field} must not be empty"),
            Self::FieldTooLong {
                field,
                length,
                limit,
            } => write!(f, "{field} length {length} exceeds limit {limit}"),
            Self::EmbeddedNul { field, offset } => {
                write!(f, "{field} contains an embedded NUL at byte {offset}")
            }
            Self::MissingTerminator { field } => write!(f, "{field} is missing its NUL terminator"),
            Self::InvalidUtf8 { field } => write!(f, "{field} is not valid UTF-8"),
            Self::Truncated {
                field,
                needed,
                remaining,
            } => write!(
                f,
                "{field} is truncated: need {needed} bytes, got {remaining}"
            ),
            Self::InvalidProtocolVersion { actual } => {
                write!(
                    f,
                    "unsupported MySQL protocol version {actual}, expected {PROTOCOL_VERSION_10}"
                )
            }
            Self::InvalidFiller { actual } => {
                write!(f, "handshake filler must be zero, got {actual}")
            }
            Self::UnsupportedCharacterSet { character_set } => write!(
                f,
                "unsupported handshake character-set/collation id {character_set}"
            ),
            Self::MissingCapabilities { flags, missing } => write!(
                f,
                "capabilities 0x{flags:08x} are missing required bits 0x{missing:08x}"
            ),
            Self::IncompatibleCapabilities {
                flags,
                capability,
                requires,
            } => write!(
                f,
                "capabilities 0x{flags:08x} set 0x{capability:08x} without 0x{requires:08x}"
            ),
            Self::InvalidAuthPluginDataLength { actual, expected } => write!(
                f,
                "authentication plugin data length is {actual}, expected {expected}"
            ),
            Self::NonZeroReservedBytes => f.write_str("handshake reserved bytes must be zero"),
            Self::ZeroAuthPluginData => {
                f.write_str("authentication plugin data must contain fresh non-zero bytes")
            }
            Self::TrailingBytes { remaining } => {
                write!(f, "initial handshake has {remaining} trailing bytes")
            }
        }
    }
}

impl Error for InitialHandshakeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PACKET_HEADER_LEN;

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: 4096,
    };

    fn config() -> InitialHandshakeConfig {
        InitialHandshakeConfig {
            server_version: String::from("8.0.36-turso"),
            connection_id: 0x1234_5678,
            capability_flags: REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES
                | CLIENT_MULTI_RESULTS
                | 0x0000_0001
                | 0x4000_0000,
            character_set: DEFAULT_UTF8MB4_COLLATION,
            status_flags: 0x0202,
            auth_plugin_data: [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13,
            ],
            auth_plugin_name: String::from("caching_sha2_password"),
        }
    }

    #[test]
    fn default_settings_advertise_client_found_rows() {
        assert_ne!(
            InitialHandshakeSettings::default().capability_flags & CLIENT_FOUND_ROWS,
            0
        );
    }

    #[test]
    fn encodes_a_deterministic_v10_packet() {
        let frame = CODEC.encode_initial_handshake(0, &config()).unwrap();
        let packet = CODEC.decode(&frame).unwrap();

        assert_eq!(packet.sequence_id, 0);
        assert_eq!(packet.payload[0], PROTOCOL_VERSION_10);
        assert_eq!(packet.payload.last(), Some(&0));
        assert_eq!(
            frame,
            CODEC.encode_initial_handshake(0, &config()).unwrap(),
            "same config must produce the same bytes"
        );
    }

    #[test]
    fn all_zero_nonce_is_rejected_by_the_caller_supplied_constructor() {
        let mut config = config();
        config.auth_plugin_data = [0; AUTH_PLUGIN_DATA_LENGTH];
        assert_eq!(
            config.validate(),
            Err(InitialHandshakeError::ZeroAuthPluginData)
        );
    }

    #[test]
    fn round_trips_v10_handshake_and_split_capabilities() {
        let expected = config();
        let frame = CODEC.encode_initial_handshake(9, &expected).unwrap();
        let decoded = CODEC.decode_initial_handshake(&frame).unwrap();

        assert_eq!(decoded.sequence_id, 9);
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION_10);
        assert_eq!(decoded.server_version, expected.server_version);
        assert_eq!(decoded.connection_id, expected.connection_id);
        assert_eq!(
            decoded.capability_flags_lower,
            expected.capability_flags as u16
        );
        assert_eq!(
            decoded.capability_flags_upper,
            (expected.capability_flags >> 16) as u16
        );
        assert_eq!(decoded.capability_flags(), expected.capability_flags);
        assert_eq!(decoded.character_set, expected.character_set);
        assert_eq!(decoded.status_flags, expected.status_flags);
        assert_eq!(decoded.auth_plugin_data, expected.auth_plugin_data);
        assert_eq!(decoded.auth_plugin_name, expected.auth_plugin_name);
    }

    #[test]
    fn rejects_missing_or_embedded_nuls() {
        let mut frame = CODEC.encode_initial_handshake(0, &config()).unwrap();
        let packet = CODEC.decode(&frame).unwrap();
        let server_version_end = packet.payload.iter().position(|byte| *byte == 0).unwrap();
        frame.truncate(PACKET_HEADER_LEN + server_version_end);
        let truncated_payload_len = server_version_end;
        frame[0] = truncated_payload_len as u8;
        frame[1] = (truncated_payload_len >> 8) as u8;
        frame[2] = (truncated_payload_len >> 16) as u8;
        assert_eq!(
            CODEC.decode_initial_handshake(&frame),
            Err(InitialHandshakeError::MissingTerminator {
                field: "server version"
            })
        );

        let mut embedded = config();
        embedded.server_version.insert(2, '\0');
        assert_eq!(
            CODEC.encode_initial_handshake(0, &embedded),
            Err(InitialHandshakeError::EmbeddedNul {
                field: "server version",
                offset: 2
            })
        );

        let mut plugin_nul = CODEC.encode_initial_handshake(0, &config()).unwrap();
        let plugin_start = PACKET_HEADER_LEN
            + 1
            + config().server_version.len()
            + 1
            + 4
            + 8
            + 1
            + 2
            + 1
            + 2
            + 2
            + 1
            + 10
            + AUTH_PLUGIN_DATA_PART_2_LENGTH
            + 1;
        plugin_nul[plugin_start + config().auth_plugin_name.len()] = b'x';
        assert_eq!(
            CODEC.decode_initial_handshake(&plugin_nul),
            Err(InitialHandshakeError::MissingTerminator {
                field: "authentication plugin name"
            })
        );
    }

    #[test]
    fn rejects_short_fixed_fields_and_trailing_payload() {
        let frame = CODEC.encode_initial_handshake(0, &config()).unwrap();
        for length in 0..frame.len() {
            let result = CODEC.decode_initial_handshake(&frame[..length]);
            assert!(
                result.is_err(),
                "truncated frame at {length} unexpectedly decoded"
            );
        }

        let mut trailing = CODEC.encode_initial_handshake(0, &config()).unwrap();
        trailing[0] = trailing[0].wrapping_add(1);
        trailing.push(0);
        assert!(matches!(
            CODEC.decode_initial_handshake(&trailing),
            Err(InitialHandshakeError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn rejects_capability_and_auth_length_mismatches() {
        let mut no_plugin = config();
        no_plugin.capability_flags &= !CLIENT_PLUGIN_AUTH;
        assert!(matches!(
            CODEC.encode_initial_handshake(0, &no_plugin),
            Err(InitialHandshakeError::MissingCapabilities { .. })
        ));

        let mut frame = CODEC.encode_initial_handshake(0, &config()).unwrap();
        let plugin_bit_offset = 1 + config().server_version.len() + 1 + 4 + 8 + 1 + 2 + 1 + 2 + 2;
        let upper = PACKET_HEADER_LEN + plugin_bit_offset;
        frame[upper] = frame[upper].wrapping_sub(1);
        assert!(matches!(
            CODEC.decode_initial_handshake(&frame),
            Err(InitialHandshakeError::InvalidAuthPluginDataLength { .. })
        ));
    }

    #[test]
    fn rejects_nonzero_filler_reserved_and_invalid_utf8() {
        let mut filler = CODEC.encode_initial_handshake(0, &config()).unwrap();
        let filler_offset = PACKET_HEADER_LEN + 1 + config().server_version.len() + 1 + 4 + 8;
        filler[filler_offset] = 1;
        assert!(matches!(
            CODEC.decode_initial_handshake(&filler),
            Err(InitialHandshakeError::InvalidFiller { actual: 1 })
        ));

        let mut reserved = CODEC.encode_initial_handshake(0, &config()).unwrap();
        let reserved_offset = PACKET_HEADER_LEN
            + 1
            + config().server_version.len()
            + 1
            + 4
            + 8
            + 1
            + 2
            + 1
            + 2
            + 2
            + 1;
        reserved[reserved_offset] = 1;
        assert!(matches!(
            CODEC.decode_initial_handshake(&reserved),
            Err(InitialHandshakeError::NonZeroReservedBytes)
        ));

        let mut invalid = CODEC.encode_initial_handshake(0, &config()).unwrap();
        invalid[PACKET_HEADER_LEN + 1] = 0xff;
        assert!(matches!(
            CODEC.decode_initial_handshake(&invalid),
            Err(InitialHandshakeError::InvalidUtf8 {
                field: "server version"
            })
        ));
    }
}
