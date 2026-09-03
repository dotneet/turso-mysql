//! A bounded MySQL 4.1 client handshake response.

use std::{error::Error, fmt, str};

use crate::{
    is_supported_utf8mb4_collation, PacketCodec, PacketCodecError, CLIENT_DEPRECATE_EOF,
    CLIENT_PLUGIN_AUTH, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, CLIENT_PROTOCOL_41,
    CLIENT_SECURE_CONNECTION, CLIENT_SSL, DEFAULT_UTF8MB4_COLLATION,
};

/// Capability bit for a database name in the handshake response.
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
/// Capability bit for length-encoded connection attributes.
pub const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;

/// Number of reserved bytes in a protocol 4.1 handshake response.
pub const CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH: usize = 23;
/// Payload length of the fixed-size SSLRequest packet.
pub const CLIENT_SSL_REQUEST_PAYLOAD_LENGTH: usize =
    4 + 4 + 1 + CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH;
/// Maximum payload accepted by the client handshake response model.
pub const MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH: usize = 4096;
/// Maximum username length accepted by this model.
pub const MAX_CLIENT_USERNAME_LENGTH: usize = u8::MAX as usize;
/// Maximum database name length accepted by this model.
pub const MAX_CLIENT_DATABASE_LENGTH: usize = u8::MAX as usize;
/// Maximum authentication plugin name length accepted by this model.
pub const MAX_CLIENT_AUTH_PLUGIN_NAME_LENGTH: usize = u8::MAX as usize;
/// Maximum authentication response length for the classic secure-connection layout.
pub const MAX_CLIENT_AUTH_RESPONSE_LENGTH: usize = u8::MAX as usize;
/// Maximum length of one connection-attribute key or value.
pub const MAX_CLIENT_ATTRIBUTE_LENGTH: usize = u8::MAX as usize;
/// Maximum total bytes in the connection-attributes block.
pub const MAX_CLIENT_ATTRIBUTES_LENGTH: usize = 2048;
/// Maximum number of connection-attribute pairs.
pub const MAX_CLIENT_ATTRIBUTE_COUNT: usize = 64;
/// Smallest non-zero client packet limit that can carry protocol-4.1 OK/ERR.
/// Smallest client packet limit accepted by this bounded server.
///
/// The response encoder and frontend adapter both cap individual payloads at
/// 4096 bytes. Requiring that full bound during negotiation guarantees that
/// every response accepted by their preflight checks can actually be framed;
/// a smaller negotiated codec would otherwise turn an ordinary command error
/// or result column into a connection-closing encode failure.
pub const MIN_SERVER_RESPONSE_PAYLOAD_LENGTH: u32 =
    crate::MAX_RESPONSE_PACKET_PAYLOAD_LENGTH as u32;

/// Capabilities required for the bounded response layout implemented here.
pub const REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES: u32 =
    CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
/// Capabilities whose response fields this model understands.
pub const SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES: u32 =
    REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
        | CLIENT_CONNECT_WITH_DB
        | CLIENT_CONNECT_ATTRS
        | CLIENT_SSL
        | CLIENT_DEPRECATE_EOF;

/// Values to send in one protocol 4.1 client handshake response.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientHandshakeResponseConfig {
    /// Combined client capability flags.
    pub capability_flags: u32,
    /// Maximum packet size accepted by the client.
    pub max_packet_size: u32,
    /// Character set/collation identifier selected by the client.
    pub character_set: u8,
    /// Twenty-three bytes reserved by protocol 4.1. They must be zero.
    pub reserved: [u8; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH],
    /// NUL-terminated client username.
    pub username: String,
    /// Authentication response bytes.
    pub auth_response: Vec<u8>,
    /// Optional NUL-terminated initial database name.
    pub database: Option<String>,
    /// Optional NUL-terminated authentication plugin name.
    pub auth_plugin_name: Option<String>,
    /// Ordered connection attributes. Ordering is retained on the wire.
    pub connect_attributes: Option<Vec<(String, String)>>,
}

impl fmt::Debug for ClientHandshakeResponseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientHandshakeResponseConfig")
            .field("capability_flags", &self.capability_flags)
            .field("max_packet_size", &self.max_packet_size)
            .field("character_set", &self.character_set)
            .field("reserved", &self.reserved)
            .field("username", &self.username)
            .field("auth_response_len", &self.auth_response.len())
            .field("database", &self.database)
            .field("auth_plugin_name", &self.auth_plugin_name)
            .field("connect_attributes", &self.connect_attributes)
            .finish()
    }
}

impl ClientHandshakeResponseConfig {
    /// Creates a response configuration with zero reserved bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        capability_flags: u32,
        max_packet_size: u32,
        character_set: u8,
        username: impl Into<String>,
        auth_response: impl Into<Vec<u8>>,
        database: Option<impl Into<String>>,
        auth_plugin_name: Option<impl Into<String>>,
        connect_attributes: Option<Vec<(String, String)>>,
    ) -> Self {
        Self {
            capability_flags,
            max_packet_size,
            character_set,
            reserved: [0; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH],
            username: username.into(),
            auth_response: auth_response.into(),
            database: database.map(Into::into),
            auth_plugin_name: auth_plugin_name.map(Into::into),
            connect_attributes,
        }
    }

    /// Checks all values that affect the response wire layout.
    pub fn validate(&self) -> Result<(), ClientHandshakeResponseError> {
        validate_capabilities(self.capability_flags)?;
        validate_max_packet_size(self.max_packet_size)?;
        validate_character_set(self.character_set)?;
        validate_reserved(&self.reserved)?;
        validate_text(&self.username, "username", MAX_CLIENT_USERNAME_LENGTH, true)?;
        if self.auth_response.len() > MAX_CLIENT_AUTH_RESPONSE_LENGTH {
            return Err(ClientHandshakeResponseError::FieldTooLong {
                field: "authentication response",
                length: self.auth_response.len(),
                limit: MAX_CLIENT_AUTH_RESPONSE_LENGTH,
            });
        }

        validate_optional_text_capability(
            self.database.as_deref(),
            self.capability_flags,
            CLIENT_CONNECT_WITH_DB,
            "database",
            MAX_CLIENT_DATABASE_LENGTH,
        )?;
        validate_optional_text_capability(
            self.auth_plugin_name.as_deref(),
            self.capability_flags,
            CLIENT_PLUGIN_AUTH,
            "authentication plugin name",
            MAX_CLIENT_AUTH_PLUGIN_NAME_LENGTH,
        )?;
        if self.capability_flags & CLIENT_PLUGIN_AUTH != 0
            && self.auth_plugin_name.as_deref() == Some("")
        {
            return Err(ClientHandshakeResponseError::EmptyField {
                field: "authentication plugin name",
            });
        }

        validate_attributes(self.connect_attributes.as_deref(), self.capability_flags)?;
        let payload_length = encoded_payload_length(self)?;
        if payload_length > MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH {
            return Err(ClientHandshakeResponseError::PayloadTooLarge {
                length: payload_length,
                limit: MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH,
            });
        }
        Ok(())
    }

    /// Encodes this configuration as one framed packet.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ClientHandshakeResponseError> {
        ClientHandshakeResponseCodec::new(codec).encode(sequence_id, self)
    }
}

impl Default for ClientHandshakeResponseConfig {
    fn default() -> Self {
        Self::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "",
            Vec::new(),
            None::<String>,
            Some(String::from("caching_sha2_password")),
            None,
        )
    }
}

/// A decoded protocol 4.1 client handshake response.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientHandshakeResponse {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Combined client capability flags.
    pub capability_flags: u32,
    /// Maximum packet size accepted by the client.
    pub max_packet_size: u32,
    /// Character set/collation identifier selected by the client.
    pub character_set: u8,
    /// Twenty-three reserved bytes. They are always zero after decoding.
    pub reserved: [u8; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH],
    /// Client username.
    pub username: String,
    /// Authentication response bytes.
    pub auth_response: Vec<u8>,
    /// Optional initial database name.
    pub database: Option<String>,
    /// Optional authentication plugin name.
    pub auth_plugin_name: Option<String>,
    /// Ordered connection attributes.
    pub connect_attributes: Option<Vec<(String, String)>>,
}

impl fmt::Debug for ClientHandshakeResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientHandshakeResponse")
            .field("sequence_id", &self.sequence_id)
            .field("capability_flags", &self.capability_flags)
            .field("max_packet_size", &self.max_packet_size)
            .field("character_set", &self.character_set)
            .field("reserved", &self.reserved)
            .field("username", &self.username)
            .field("auth_response_len", &self.auth_response.len())
            .field("database", &self.database)
            .field("auth_plugin_name", &self.auth_plugin_name)
            .field("connect_attributes", &self.connect_attributes)
            .finish()
    }
}

impl ClientHandshakeResponse {
    /// Decodes one framed protocol 4.1 client handshake response.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, ClientHandshakeResponseError> {
        ClientHandshakeResponseCodec::new(codec).decode(frame)
    }

    /// Converts decoded values to an encoding configuration.
    pub fn to_config(&self) -> ClientHandshakeResponseConfig {
        ClientHandshakeResponseConfig {
            capability_flags: self.capability_flags,
            max_packet_size: self.max_packet_size,
            character_set: self.character_set,
            reserved: self.reserved,
            username: self.username.clone(),
            auth_response: self.auth_response.clone(),
            database: self.database.clone(),
            auth_plugin_name: self.auth_plugin_name.clone(),
            connect_attributes: self.connect_attributes.clone(),
        }
    }

    /// Returns the reserved bytes as a slice.
    pub fn reserved_bytes(&self) -> &[u8; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH] {
        &self.reserved
    }

    /// Encodes this response with a new sequence number.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ClientHandshakeResponseError> {
        self.to_config().encode(codec, sequence_id)
    }
}

/// Codec for one bounded client handshake response packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientHandshakeResponseCodec {
    packet_codec: PacketCodec,
}

impl ClientHandshakeResponseCodec {
    /// Creates a handshake response codec on top of packet framing.
    pub const fn new(packet_codec: PacketCodec) -> Self {
        Self { packet_codec }
    }

    /// Returns the packet framing codec used by this codec.
    pub const fn packet_codec(self) -> PacketCodec {
        self.packet_codec
    }

    /// Encodes one bounded client handshake response.
    pub fn encode(
        self,
        sequence_id: u8,
        config: &ClientHandshakeResponseConfig,
    ) -> Result<Vec<u8>, ClientHandshakeResponseError> {
        config.validate()?;
        let payload = encode_payload(config)?;
        self.packet_codec
            .encode(sequence_id, &payload)
            .map_err(ClientHandshakeResponseError::from)
    }

    /// Decodes exactly one bounded client handshake response packet.
    pub fn decode(
        self,
        frame: &[u8],
    ) -> Result<ClientHandshakeResponse, ClientHandshakeResponseError> {
        let packet = self
            .packet_codec
            .decode(frame)
            .map_err(ClientHandshakeResponseError::from)?;
        if packet.payload.len() > MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH {
            return Err(ClientHandshakeResponseError::PayloadTooLarge {
                length: packet.payload.len(),
                limit: MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH,
            });
        }
        decode_payload(packet.sequence_id, packet.payload)
    }
}

impl PacketCodec {
    /// Encodes one client handshake response using this packet codec.
    pub fn encode_client_handshake_response(
        self,
        sequence_id: u8,
        config: &ClientHandshakeResponseConfig,
    ) -> Result<Vec<u8>, ClientHandshakeResponseError> {
        ClientHandshakeResponseCodec::new(self).encode(sequence_id, config)
    }

    /// Decodes one client handshake response using this packet codec.
    pub fn decode_client_handshake_response(
        self,
        frame: &[u8],
    ) -> Result<ClientHandshakeResponse, ClientHandshakeResponseError> {
        ClientHandshakeResponseCodec::new(self).decode(frame)
    }

    /// Encodes one fixed-size SSLRequest packet.
    pub fn encode_client_ssl_request(
        self,
        sequence_id: u8,
        config: &ClientSslRequestConfig,
    ) -> Result<Vec<u8>, ClientSslRequestError> {
        config.encode(self, sequence_id)
    }

    /// Decodes one fixed-size SSLRequest packet.
    pub fn decode_client_ssl_request(
        self,
        frame: &[u8],
    ) -> Result<ClientSslRequest, ClientSslRequestError> {
        ClientSslRequest::decode(self, frame)
    }
}

/// Values carried by a MySQL SSLRequest packet before the TLS handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSslRequestConfig {
    /// Combined client capability flags, including [`CLIENT_SSL`].
    pub capability_flags: u32,
    /// Maximum packet size accepted by the client.
    pub max_packet_size: u32,
    /// Character set/collation identifier selected by the client.
    pub character_set: u8,
    /// Twenty-three reserved bytes. They must be zero.
    pub reserved: [u8; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH],
}

impl ClientSslRequestConfig {
    /// Creates an SSLRequest configuration with zero reserved bytes.
    pub fn new(capability_flags: u32, max_packet_size: u32, character_set: u8) -> Self {
        Self {
            capability_flags,
            max_packet_size,
            character_set,
            reserved: [0; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH],
        }
    }

    /// Checks the fixed fields and the TLS capability bit.
    pub fn validate(&self) -> Result<(), ClientSslRequestError> {
        if self.capability_flags & CLIENT_SSL == 0 {
            return Err(ClientSslRequestError::MissingSslCapability {
                flags: self.capability_flags,
            });
        }
        let missing = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES & !self.capability_flags;
        if missing != 0 {
            return Err(ClientSslRequestError::MissingCapabilities {
                flags: self.capability_flags,
                missing,
            });
        }
        validate_ssl_max_packet_size(self.max_packet_size)?;
        validate_ssl_character_set(self.character_set)?;
        validate_reserved(&self.reserved).map_err(|_| ClientSslRequestError::NonZeroReservedBytes)
    }

    /// Encodes this configuration as one fixed-size framed packet.
    pub fn encode(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
    ) -> Result<Vec<u8>, ClientSslRequestError> {
        self.validate()?;
        let mut payload = Vec::with_capacity(CLIENT_SSL_REQUEST_PAYLOAD_LENGTH);
        payload.extend_from_slice(&self.capability_flags.to_le_bytes());
        payload.extend_from_slice(&self.max_packet_size.to_le_bytes());
        payload.push(self.character_set);
        payload.extend_from_slice(&self.reserved);
        codec
            .encode(sequence_id, &payload)
            .map_err(ClientSslRequestError::from)
    }
}

/// A decoded fixed-size MySQL SSLRequest packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSslRequest {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Combined client capability flags.
    pub capability_flags: u32,
    /// Maximum packet size accepted by the client.
    pub max_packet_size: u32,
    /// Character set/collation identifier selected by the client.
    pub character_set: u8,
    /// Twenty-three reserved bytes, always zero after decoding.
    pub reserved: [u8; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH],
}

impl ClientSslRequest {
    /// Decodes one fixed-size SSLRequest packet.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, ClientSslRequestError> {
        let packet = codec.decode(frame).map_err(ClientSslRequestError::from)?;
        if packet.payload.len() != CLIENT_SSL_REQUEST_PAYLOAD_LENGTH {
            return Err(ClientSslRequestError::InvalidPayloadLength {
                actual: packet.payload.len(),
                expected: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
            });
        }
        let capability_flags = u32::from_le_bytes([
            packet.payload[0],
            packet.payload[1],
            packet.payload[2],
            packet.payload[3],
        ]);
        if capability_flags & CLIENT_SSL == 0 {
            return Err(ClientSslRequestError::MissingSslCapability {
                flags: capability_flags,
            });
        }
        let missing = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES & !capability_flags;
        if missing != 0 {
            return Err(ClientSslRequestError::MissingCapabilities {
                flags: capability_flags,
                missing,
            });
        }
        let max_packet_size = u32::from_le_bytes([
            packet.payload[4],
            packet.payload[5],
            packet.payload[6],
            packet.payload[7],
        ]);
        let character_set = packet.payload[8];
        validate_ssl_max_packet_size(max_packet_size)?;
        validate_ssl_character_set(character_set)?;
        let mut reserved = [0; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH];
        reserved.copy_from_slice(&packet.payload[9..]);
        if reserved.iter().any(|byte| *byte != 0) {
            return Err(ClientSslRequestError::NonZeroReservedBytes);
        }
        Ok(Self {
            sequence_id: packet.sequence_id,
            capability_flags,
            max_packet_size,
            character_set,
            reserved,
        })
    }

    /// Converts this request to an encoding configuration.
    pub fn to_config(&self) -> ClientSslRequestConfig {
        ClientSslRequestConfig {
            capability_flags: self.capability_flags,
            max_packet_size: self.max_packet_size,
            character_set: self.character_set,
            reserved: self.reserved,
        }
    }

    /// Returns the reserved bytes as a slice.
    pub fn reserved_bytes(&self) -> &[u8; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH] {
        &self.reserved
    }
}

/// Errors returned by the fixed-size SSLRequest codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientSslRequestError {
    /// Packet framing rejected the frame.
    PacketCodec(PacketCodecError),
    /// The frame payload is not exactly the fixed SSLRequest size.
    InvalidPayloadLength { actual: usize, expected: usize },
    /// The request did not ask to upgrade to TLS.
    MissingSslCapability { flags: u32 },
    /// Required protocol 4.1 response capabilities are absent.
    MissingCapabilities { flags: u32, missing: u32 },
    /// The peer cannot receive the smallest protocol-4.1 server response.
    MaxPacketSizeTooSmall { max_packet_size: u32, minimum: u32 },
    /// The peer selected a collation outside this UTF-8-only slice.
    UnsupportedCharacterSet { character_set: u8 },
    /// Reserved bytes must be zero.
    NonZeroReservedBytes,
}

impl From<PacketCodecError> for ClientSslRequestError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl fmt::Display for ClientSslRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::InvalidPayloadLength { actual, expected } => {
                write!(
                    f,
                    "SSLRequest payload is {actual} bytes, expected {expected}"
                )
            }
            Self::MissingSslCapability { flags } => {
                write!(f, "SSLRequest capabilities 0x{flags:08x} omit CLIENT_SSL")
            }
            Self::MissingCapabilities { flags, missing } => write!(
                f,
                "SSLRequest capabilities 0x{flags:08x} are missing required bits 0x{missing:08x}"
            ),
            Self::MaxPacketSizeTooSmall {
                max_packet_size,
                minimum,
            } => write!(
                f,
                "client maximum packet size {max_packet_size} is below required minimum {minimum}"
            ),
            Self::UnsupportedCharacterSet { character_set } => write!(
                f,
                "unsupported client character-set/collation id {character_set}"
            ),
            Self::NonZeroReservedBytes => f.write_str("SSLRequest reserved bytes must be zero"),
        }
    }
}

impl Error for ClientSslRequestError {}

fn encode_payload(
    config: &ClientHandshakeResponseConfig,
) -> Result<Vec<u8>, ClientHandshakeResponseError> {
    let payload_length = encoded_payload_length(config)?;
    let mut payload = Vec::with_capacity(payload_length);
    payload.extend_from_slice(&config.capability_flags.to_le_bytes());
    payload.extend_from_slice(&config.max_packet_size.to_le_bytes());
    payload.push(config.character_set);
    payload.extend_from_slice(&config.reserved);
    push_nul_string(&mut payload, config.username.as_bytes());
    push_secure_auth_response(&mut payload, &config.auth_response);

    if config.capability_flags & CLIENT_CONNECT_WITH_DB != 0 {
        push_nul_string(
            &mut payload,
            config
                .database
                .as_deref()
                .expect("database capability was validated")
                .as_bytes(),
        );
    }
    if config.capability_flags & CLIENT_PLUGIN_AUTH != 0 {
        push_nul_string(
            &mut payload,
            config
                .auth_plugin_name
                .as_deref()
                .expect("plugin capability was validated")
                .as_bytes(),
        );
    }
    if config.capability_flags & CLIENT_CONNECT_ATTRS != 0 {
        let attributes = config
            .connect_attributes
            .as_deref()
            .expect("attributes capability was validated");
        let attributes_length = encoded_attributes_length(attributes)?;
        push_lenenc_integer(&mut payload, attributes_length as u64);
        for (key, value) in attributes {
            push_lenenc_bytes(&mut payload, key.as_bytes());
            push_lenenc_bytes(&mut payload, value.as_bytes());
        }
    }
    debug_assert_eq!(payload.len(), payload_length);
    Ok(payload)
}

fn decode_payload(
    sequence_id: u8,
    payload: &[u8],
) -> Result<ClientHandshakeResponse, ClientHandshakeResponseError> {
    let mut reader = Reader::new(payload);
    let capability_flags = reader.read_u32("capability flags")?;
    validate_capabilities(capability_flags)?;
    let max_packet_size = reader.read_u32("maximum packet size")?;
    let character_set = reader.read_u8("character set")?;
    validate_max_packet_size(max_packet_size)?;
    validate_character_set(character_set)?;
    let reserved_bytes =
        reader.read_exact(CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH, "reserved bytes")?;
    let mut reserved = [0; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH];
    reserved.copy_from_slice(reserved_bytes);
    validate_reserved(&reserved)?;

    let username = reader.read_string("username", MAX_CLIENT_USERNAME_LENGTH, true)?;
    let auth_response = reader.read_secure_auth_response()?;

    let database = if capability_flags & CLIENT_CONNECT_WITH_DB != 0 {
        Some(reader.read_string("database", MAX_CLIENT_DATABASE_LENGTH, true)?)
    } else {
        None
    };
    let auth_plugin_name = if capability_flags & CLIENT_PLUGIN_AUTH != 0 {
        let name = reader.read_string(
            "authentication plugin name",
            MAX_CLIENT_AUTH_PLUGIN_NAME_LENGTH,
            false,
        )?;
        Some(name)
    } else {
        None
    };
    let connect_attributes = if capability_flags & CLIENT_CONNECT_ATTRS != 0 {
        Some(read_attributes(&mut reader)?)
    } else {
        None
    };
    reader.finish()?;

    Ok(ClientHandshakeResponse {
        sequence_id,
        capability_flags,
        max_packet_size,
        character_set,
        reserved,
        username,
        auth_response,
        database,
        auth_plugin_name,
        connect_attributes,
    })
}

fn validate_capabilities(flags: u32) -> Result<(), ClientHandshakeResponseError> {
    let missing = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES & !flags;
    if missing != 0 {
        return Err(ClientHandshakeResponseError::MissingCapabilities { flags, missing });
    }
    let unsupported = flags & !SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
    if unsupported != 0 {
        return Err(ClientHandshakeResponseError::UnsupportedCapabilities { flags, unsupported });
    }
    if flags & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        return Err(ClientHandshakeResponseError::UnsupportedCapabilities {
            flags,
            unsupported: CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
        });
    }
    Ok(())
}

fn validate_max_packet_size(max_packet_size: u32) -> Result<(), ClientHandshakeResponseError> {
    if max_packet_size != 0 && max_packet_size < MIN_SERVER_RESPONSE_PAYLOAD_LENGTH {
        return Err(ClientHandshakeResponseError::MaxPacketSizeTooSmall {
            max_packet_size,
            minimum: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
        });
    }
    Ok(())
}

fn validate_character_set(character_set: u8) -> Result<(), ClientHandshakeResponseError> {
    if is_supported_utf8mb4_collation(character_set) {
        Ok(())
    } else {
        Err(ClientHandshakeResponseError::UnsupportedCharacterSet { character_set })
    }
}

fn validate_ssl_max_packet_size(max_packet_size: u32) -> Result<(), ClientSslRequestError> {
    if max_packet_size != 0 && max_packet_size < MIN_SERVER_RESPONSE_PAYLOAD_LENGTH {
        return Err(ClientSslRequestError::MaxPacketSizeTooSmall {
            max_packet_size,
            minimum: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
        });
    }
    Ok(())
}

fn validate_ssl_character_set(character_set: u8) -> Result<(), ClientSslRequestError> {
    if is_supported_utf8mb4_collation(character_set) {
        Ok(())
    } else {
        Err(ClientSslRequestError::UnsupportedCharacterSet { character_set })
    }
}

fn validate_reserved(
    reserved: &[u8; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH],
) -> Result<(), ClientHandshakeResponseError> {
    if reserved.iter().any(|byte| *byte != 0) {
        return Err(ClientHandshakeResponseError::NonZeroReservedBytes);
    }
    Ok(())
}

fn validate_optional_text_capability(
    value: Option<&str>,
    flags: u32,
    capability: u32,
    field: &'static str,
    limit: usize,
) -> Result<(), ClientHandshakeResponseError> {
    let enabled = flags & capability != 0;
    if enabled != value.is_some() {
        return Err(ClientHandshakeResponseError::FieldCapabilityMismatch {
            field,
            capability,
            enabled,
        });
    }
    if let Some(value) = value {
        validate_text(value, field, limit, true)?;
    }
    Ok(())
}

fn validate_text(
    value: &str,
    field: &'static str,
    limit: usize,
    allow_empty: bool,
) -> Result<(), ClientHandshakeResponseError> {
    if !allow_empty && value.is_empty() {
        return Err(ClientHandshakeResponseError::EmptyField { field });
    }
    if value.len() > limit {
        return Err(ClientHandshakeResponseError::FieldTooLong {
            field,
            length: value.len(),
            limit,
        });
    }
    if let Some(offset) = value.as_bytes().iter().position(|byte| *byte == 0) {
        return Err(ClientHandshakeResponseError::EmbeddedNul { field, offset });
    }
    Ok(())
}

fn validate_attributes(
    attributes: Option<&[(String, String)]>,
    flags: u32,
) -> Result<(), ClientHandshakeResponseError> {
    let enabled = flags & CLIENT_CONNECT_ATTRS != 0;
    if enabled != attributes.is_some() {
        return Err(ClientHandshakeResponseError::FieldCapabilityMismatch {
            field: "connection attributes",
            capability: CLIENT_CONNECT_ATTRS,
            enabled,
        });
    }
    let Some(attributes) = attributes else {
        return Ok(());
    };
    if attributes.len() > MAX_CLIENT_ATTRIBUTE_COUNT {
        return Err(ClientHandshakeResponseError::TooManyAttributes {
            count: attributes.len(),
            limit: MAX_CLIENT_ATTRIBUTE_COUNT,
        });
    }
    for (index, (key, value)) in attributes.iter().enumerate() {
        validate_text(
            key,
            "connection attribute key",
            MAX_CLIENT_ATTRIBUTE_LENGTH,
            false,
        )?;
        validate_text(
            value,
            "connection attribute value",
            MAX_CLIENT_ATTRIBUTE_LENGTH,
            true,
        )?;
        if attributes[..index]
            .iter()
            .any(|(old_key, _)| old_key == key)
        {
            return Err(ClientHandshakeResponseError::DuplicateAttribute { key: key.clone() });
        }
    }
    let total = encoded_attributes_length(attributes)?;
    if total > MAX_CLIENT_ATTRIBUTES_LENGTH {
        return Err(ClientHandshakeResponseError::AttributesTooLarge {
            length: total,
            limit: MAX_CLIENT_ATTRIBUTES_LENGTH,
        });
    }
    Ok(())
}

fn encoded_payload_length(
    config: &ClientHandshakeResponseConfig,
) -> Result<usize, ClientHandshakeResponseError> {
    let mut length = 4 + 4 + 1 + CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH;
    length = checked_add(length, config.username.len() + 1)?;
    length = checked_add(length, config.auth_response.len() + 1)?;
    if config.capability_flags & CLIENT_CONNECT_WITH_DB != 0 {
        length = checked_add(
            length,
            config
                .database
                .as_deref()
                .map_or(1, |value| value.len() + 1),
        )?;
    }
    if config.capability_flags & CLIENT_PLUGIN_AUTH != 0 {
        length = checked_add(
            length,
            config
                .auth_plugin_name
                .as_deref()
                .map_or(1, |value| value.len() + 1),
        )?;
    }
    if config.capability_flags & CLIENT_CONNECT_ATTRS != 0 {
        let attributes = config.connect_attributes.as_deref().unwrap_or_default();
        let total = encoded_attributes_length(attributes)?;
        length = checked_add(length, lenenc_integer_length(total as u64))?;
        length = checked_add(length, total)?;
    }
    Ok(length)
}

fn checked_add(left: usize, right: usize) -> Result<usize, ClientHandshakeResponseError> {
    left.checked_add(right)
        .ok_or(ClientHandshakeResponseError::PayloadTooLarge {
            length: usize::MAX,
            limit: MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH,
        })
}

fn encoded_attributes_length(
    attributes: &[(String, String)],
) -> Result<usize, ClientHandshakeResponseError> {
    let mut length = 0;
    for (key, value) in attributes {
        length = checked_add(length, lenenc_integer_length(key.len() as u64))?;
        length = checked_add(length, key.len())?;
        length = checked_add(length, lenenc_integer_length(value.len() as u64))?;
        length = checked_add(length, value.len())?;
    }
    Ok(length)
}

fn push_nul_string(payload: &mut Vec<u8>, value: &[u8]) {
    payload.extend_from_slice(value);
    payload.push(0);
}

fn push_secure_auth_response(payload: &mut Vec<u8>, value: &[u8]) {
    payload.push(value.len() as u8);
    payload.extend_from_slice(value);
}

fn push_lenenc_bytes(payload: &mut Vec<u8>, value: &[u8]) {
    push_lenenc_integer(payload, value.len() as u64);
    payload.extend_from_slice(value);
}

fn push_lenenc_integer(payload: &mut Vec<u8>, value: u64) {
    match value {
        0..=0xfa => payload.push(value as u8),
        0xfb..=0xffff => {
            payload.push(0xfc);
            payload.extend_from_slice(&(value as u16).to_le_bytes());
        }
        0x1_0000..=0xff_ffff => {
            payload.push(0xfd);
            let value = value as u32;
            payload.extend_from_slice(&value.to_le_bytes()[..3]);
        }
        _ => {
            payload.push(0xfe);
            payload.extend_from_slice(&value.to_le_bytes());
        }
    }
}

fn lenenc_integer_length(value: u64) -> usize {
    match value {
        0..=0xfa => 1,
        0xfb..=0xffff => 3,
        0x1_0000..=0xff_ffff => 4,
        _ => 9,
    }
}

fn read_attributes(
    reader: &mut Reader<'_>,
) -> Result<Vec<(String, String)>, ClientHandshakeResponseError> {
    let total = reader.read_lenenc_integer("connection attributes length")?;
    if total > MAX_CLIENT_ATTRIBUTES_LENGTH as u64 {
        return Err(ClientHandshakeResponseError::AttributesTooLarge {
            length: usize::try_from(total).unwrap_or(usize::MAX),
            limit: MAX_CLIENT_ATTRIBUTES_LENGTH,
        });
    }
    let total =
        usize::try_from(total).map_err(|_| ClientHandshakeResponseError::LengthTooLarge {
            field: "connection attributes length",
            length: u64::MAX,
            limit: MAX_CLIENT_ATTRIBUTES_LENGTH,
        })?;
    let bytes = reader.read_exact(total, "connection attributes")?;
    let mut attributes_reader = Reader::new(bytes);
    let mut attributes = Vec::new();
    while attributes_reader.remaining() != 0 {
        if attributes.len() == MAX_CLIENT_ATTRIBUTE_COUNT {
            return Err(ClientHandshakeResponseError::TooManyAttributes {
                count: attributes.len() + 1,
                limit: MAX_CLIENT_ATTRIBUTE_COUNT,
            });
        }
        let key = attributes_reader.read_lenenc_string(
            "connection attribute key",
            MAX_CLIENT_ATTRIBUTE_LENGTH,
            false,
        )?;
        let value = attributes_reader.read_lenenc_string(
            "connection attribute value",
            MAX_CLIENT_ATTRIBUTE_LENGTH,
            true,
        )?;
        if attributes.iter().any(|(old_key, _)| old_key == &key) {
            return Err(ClientHandshakeResponseError::DuplicateAttribute { key });
        }
        attributes.push((key, value));
    }
    Ok(attributes)
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn read_exact(
        &mut self,
        length: usize,
        field: &'static str,
    ) -> Result<&'a [u8], ClientHandshakeResponseError> {
        let remaining = self.remaining();
        if remaining < length {
            return Err(ClientHandshakeResponseError::Truncated {
                field,
                needed: length,
                remaining,
            });
        }
        let start = self.offset;
        self.offset += length;
        Ok(&self.bytes[start..self.offset])
    }

    fn read_u8(&mut self, field: &'static str) -> Result<u8, ClientHandshakeResponseError> {
        Ok(self.read_exact(1, field)?[0])
    }

    fn read_u32(&mut self, field: &'static str) -> Result<u32, ClientHandshakeResponseError> {
        let bytes = self.read_exact(4, field)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_string(
        &mut self,
        field: &'static str,
        max_length: usize,
        allow_empty: bool,
    ) -> Result<String, ClientHandshakeResponseError> {
        let remaining = &self.bytes[self.offset..];
        let Some(end) = remaining.iter().position(|byte| *byte == 0) else {
            return Err(ClientHandshakeResponseError::MissingTerminator { field });
        };
        if end > max_length {
            return Err(ClientHandshakeResponseError::FieldTooLong {
                field,
                length: end,
                limit: max_length,
            });
        }
        if end == 0 && !allow_empty {
            return Err(ClientHandshakeResponseError::EmptyField { field });
        }
        let value = str::from_utf8(&remaining[..end])
            .map_err(|_| ClientHandshakeResponseError::InvalidUtf8 { field })?
            .to_owned();
        self.offset += end + 1;
        Ok(value)
    }

    fn read_lenenc_string(
        &mut self,
        field: &'static str,
        max_length: usize,
        allow_empty: bool,
    ) -> Result<String, ClientHandshakeResponseError> {
        let bytes = self.read_lenenc_bytes(field, max_length)?;
        if bytes.is_empty() && !allow_empty {
            return Err(ClientHandshakeResponseError::EmptyField { field });
        }
        if let Some(offset) = bytes.iter().position(|byte| *byte == 0) {
            return Err(ClientHandshakeResponseError::EmbeddedNul { field, offset });
        }
        str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ClientHandshakeResponseError::InvalidUtf8 { field })
    }

    fn read_secure_auth_response(&mut self) -> Result<Vec<u8>, ClientHandshakeResponseError> {
        let length = usize::from(self.read_u8("authentication response length")?);
        Ok(self.read_exact(length, "authentication response")?.to_vec())
    }

    fn read_lenenc_bytes(
        &mut self,
        field: &'static str,
        max_length: usize,
    ) -> Result<&'a [u8], ClientHandshakeResponseError> {
        let length = self.read_lenenc_integer(field)?;
        if length > max_length as u64 {
            return Err(ClientHandshakeResponseError::LengthTooLarge {
                field,
                length,
                limit: max_length,
            });
        }
        let length =
            usize::try_from(length).map_err(|_| ClientHandshakeResponseError::LengthTooLarge {
                field,
                length,
                limit: max_length,
            })?;
        self.read_exact(length, field)
    }

    fn read_lenenc_integer(
        &mut self,
        field: &'static str,
    ) -> Result<u64, ClientHandshakeResponseError> {
        let marker = self.read_u8(field)?;
        match marker {
            0..=0xfa => Ok(u64::from(marker)),
            0xfb => {
                Err(ClientHandshakeResponseError::InvalidLengthEncodedInteger { field, marker })
            }
            0xfc => {
                let bytes = self.read_exact(2, field)?;
                let value = u64::from(u16::from_le_bytes([bytes[0], bytes[1]]));
                if value < 0xfb {
                    return Err(
                        ClientHandshakeResponseError::NonCanonicalLengthEncodedInteger {
                            field,
                            value,
                        },
                    );
                }
                Ok(value)
            }
            0xfd => {
                let bytes = self.read_exact(3, field)?;
                let value =
                    u64::from(bytes[0]) | (u64::from(bytes[1]) << 8) | (u64::from(bytes[2]) << 16);
                if value <= 0xffff {
                    return Err(
                        ClientHandshakeResponseError::NonCanonicalLengthEncodedInteger {
                            field,
                            value,
                        },
                    );
                }
                Ok(value)
            }
            0xfe => {
                let bytes = self.read_exact(8, field)?;
                let value = u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]);
                if value <= 0xff_ffff {
                    return Err(
                        ClientHandshakeResponseError::NonCanonicalLengthEncodedInteger {
                            field,
                            value,
                        },
                    );
                }
                Ok(value)
            }
            0xff => {
                Err(ClientHandshakeResponseError::InvalidLengthEncodedInteger { field, marker })
            }
        }
    }

    fn finish(&self) -> Result<(), ClientHandshakeResponseError> {
        if self.offset != self.bytes.len() {
            return Err(ClientHandshakeResponseError::TrailingBytes {
                remaining: self.bytes.len() - self.offset,
            });
        }
        Ok(())
    }
}

/// Errors returned by the bounded client handshake response codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientHandshakeResponseError {
    /// Packet framing rejected the frame.
    PacketCodec(PacketCodecError),
    /// The payload exceeds this model's independent bound.
    PayloadTooLarge { length: usize, limit: usize },
    /// A required textual field was empty.
    EmptyField { field: &'static str },
    /// A textual or binary field exceeds its bound.
    FieldTooLong {
        field: &'static str,
        length: usize,
        limit: usize,
    },
    /// A field does not match the capability controlling its wire presence.
    FieldCapabilityMismatch {
        field: &'static str,
        capability: u32,
        enabled: bool,
    },
    /// A configuration string contains a NUL that cannot be represented safely.
    EmbeddedNul { field: &'static str, offset: usize },
    /// A NUL-terminated wire string has no terminator.
    MissingTerminator { field: &'static str },
    /// A wire string is not valid UTF-8.
    InvalidUtf8 { field: &'static str },
    /// A fixed-width field is shorter than required.
    Truncated {
        field: &'static str,
        needed: usize,
        remaining: usize,
    },
    /// Required capabilities are absent.
    MissingCapabilities { flags: u32, missing: u32 },
    /// The peer cannot receive the smallest protocol-4.1 server response.
    MaxPacketSizeTooSmall { max_packet_size: u32, minimum: u32 },
    /// The peer selected a collation outside this UTF-8-only slice.
    UnsupportedCharacterSet { character_set: u8 },
    /// A capability is not part of this bounded response model.
    UnsupportedCapabilities { flags: u32, unsupported: u32 },
    /// Reserved bytes must be zero.
    NonZeroReservedBytes,
    /// The outer attribute block exceeds its bound.
    AttributesTooLarge { length: usize, limit: usize },
    /// Too many connection-attribute pairs were supplied.
    TooManyAttributes { count: usize, limit: usize },
    /// An attribute key occurs more than once.
    DuplicateAttribute { key: String },
    /// A length-encoded integer has an invalid marker.
    InvalidLengthEncodedInteger { field: &'static str, marker: u8 },
    /// A length-encoded integer used a wider-than-needed representation.
    NonCanonicalLengthEncodedInteger { field: &'static str, value: u64 },
    /// A length-encoded field exceeds its independent bound.
    LengthTooLarge {
        field: &'static str,
        length: u64,
        limit: usize,
    },
    /// The packet has bytes after all capability-controlled fields.
    TrailingBytes { remaining: usize },
}

impl From<PacketCodecError> for ClientHandshakeResponseError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl fmt::Display for ClientHandshakeResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::PayloadTooLarge { length, limit } => {
                write!(f, "client handshake payload {length} exceeds limit {limit}")
            }
            Self::EmptyField { field } => write!(f, "{field} must not be empty"),
            Self::FieldTooLong {
                field,
                length,
                limit,
            } => write!(f, "{field} length {length} exceeds limit {limit}"),
            Self::FieldCapabilityMismatch {
                field,
                capability,
                enabled,
            } => write!(
                f,
                "{field} presence does not match capability 0x{capability:08x} (enabled={enabled})"
            ),
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
            Self::MissingCapabilities { flags, missing } => write!(
                f,
                "capabilities 0x{flags:08x} are missing required bits 0x{missing:08x}"
            ),
            Self::MaxPacketSizeTooSmall {
                max_packet_size,
                minimum,
            } => write!(
                f,
                "client maximum packet size {max_packet_size} is below required minimum {minimum}"
            ),
            Self::UnsupportedCharacterSet { character_set } => write!(
                f,
                "unsupported client character-set/collation id {character_set}"
            ),
            Self::UnsupportedCapabilities { flags, unsupported } => write!(
                f,
                "capabilities 0x{flags:08x} contain unsupported bits 0x{unsupported:08x}"
            ),
            Self::NonZeroReservedBytes => {
                f.write_str("handshake response reserved bytes must be zero")
            }
            Self::AttributesTooLarge { length, limit } => {
                write!(
                    f,
                    "connection attributes length {length} exceeds limit {limit}"
                )
            }
            Self::TooManyAttributes { count, limit } => {
                write!(
                    f,
                    "connection attribute count {count} exceeds limit {limit}"
                )
            }
            Self::DuplicateAttribute { key } => {
                write!(f, "connection attribute key {key:?} occurs more than once")
            }
            Self::InvalidLengthEncodedInteger { field, marker } => {
                write!(
                    f,
                    "{field} has invalid length-encoded integer marker 0x{marker:02x}"
                )
            }
            Self::NonCanonicalLengthEncodedInteger { field, value } => {
                write!(f, "{field} encodes {value} non-canonically")
            }
            Self::LengthTooLarge {
                field,
                length,
                limit,
            } => write!(f, "{field} length {length} exceeds limit {limit}"),
            Self::TrailingBytes { remaining } => {
                write!(
                    f,
                    "client handshake response has {remaining} trailing bytes"
                )
            }
        }
    }
}

impl Error for ClientHandshakeResponseError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PACKET_HEADER_LEN;

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH,
    };

    fn config() -> ClientHandshakeResponseConfig {
        ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                | CLIENT_CONNECT_WITH_DB
                | CLIENT_CONNECT_ATTRS,
            0x0102_0304,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            vec![0, 1, 2, 3, 0xff],
            Some("test_db"),
            Some("caching_sha2_password"),
            Some(vec![
                ("_client_name".to_owned(), "turso".to_owned()),
                ("program_name".to_owned(), "tursodb".to_owned()),
            ]),
        )
    }

    #[test]
    fn encodes_a_deterministic_response_and_round_trips() {
        let expected = config();
        let frame = CODEC
            .encode_client_handshake_response(8, &expected)
            .unwrap();
        assert_eq!(
            frame,
            CODEC
                .encode_client_handshake_response(8, &expected)
                .unwrap()
        );
        let decoded = CODEC.decode_client_handshake_response(&frame).unwrap();
        assert_eq!(decoded.sequence_id, 8);
        assert_eq!(decoded.to_config(), expected);
    }

    #[test]
    fn encodes_the_capability_controlled_wire_order() {
        let config = config();
        let frame = CODEC.encode_client_handshake_response(0, &config).unwrap();
        let payload = &frame[PACKET_HEADER_LEN..];
        assert_eq!(&payload[..4], &config.capability_flags.to_le_bytes());
        assert_eq!(
            &payload[9..32],
            &[0; CLIENT_HANDSHAKE_RESPONSE_RESERVED_LENGTH]
        );
        assert_eq!(&payload[32..37], b"root\0");
        assert_eq!(payload[37], 5);
        assert_eq!(&payload[38..43], [0, 1, 2, 3, 0xff]);
        assert_eq!(&payload[43..51], b"test_db\0");
        assert_eq!(&payload[51..73], b"caching_sha2_password\0");
        assert_eq!(payload[73], 40);
    }

    #[test]
    fn rejects_missing_and_unsupported_capabilities() {
        let mut missing = config();
        missing.capability_flags &= !CLIENT_PROTOCOL_41;
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &missing),
            Err(ClientHandshakeResponseError::MissingCapabilities { .. })
        ));

        let mut unsupported = config();
        unsupported.capability_flags |= CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &unsupported),
            Err(ClientHandshakeResponseError::UnsupportedCapabilities { .. })
        ));
    }

    #[test]
    fn rejects_capability_field_mismatches_and_bad_text() {
        let mut missing_db = config();
        missing_db.database = None;
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &missing_db),
            Err(ClientHandshakeResponseError::FieldCapabilityMismatch {
                field: "database",
                ..
            })
        ));

        let mut no_attrs = config();
        no_attrs.capability_flags &= !CLIENT_CONNECT_ATTRS;
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &no_attrs),
            Err(ClientHandshakeResponseError::FieldCapabilityMismatch {
                field: "connection attributes",
                ..
            })
        ));

        let mut nul = config();
        nul.username.push('\0');
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &nul),
            Err(ClientHandshakeResponseError::EmbeddedNul {
                field: "username",
                ..
            })
        ));
    }

    #[test]
    fn rejects_too_small_packet_limits_and_non_utf8_collations() {
        let below_minimum = MIN_SERVER_RESPONSE_PAYLOAD_LENGTH - 1;
        let mut too_small = config();
        too_small.max_packet_size = below_minimum;
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &too_small),
            Err(ClientHandshakeResponseError::MaxPacketSizeTooSmall {
                max_packet_size,
                minimum: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
            }) if max_packet_size == below_minimum
        ));

        let mut frame = CODEC
            .encode_client_handshake_response(0, &config())
            .unwrap();
        frame[PACKET_HEADER_LEN + 4..PACKET_HEADER_LEN + 8]
            .copy_from_slice(&below_minimum.to_le_bytes());
        assert!(matches!(
            CODEC.decode_client_handshake_response(&frame),
            Err(ClientHandshakeResponseError::MaxPacketSizeTooSmall {
                max_packet_size,
                minimum: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
            }) if max_packet_size == below_minimum
        ));

        let mut charset = config();
        charset.character_set = 33;
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &charset),
            Err(ClientHandshakeResponseError::UnsupportedCharacterSet { character_set: 33 })
        ));

        let mut ssl = ClientSslRequestConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            below_minimum,
            DEFAULT_UTF8MB4_COLLATION,
        );
        assert!(matches!(
            CODEC.encode_client_ssl_request(0, &ssl),
            Err(ClientSslRequestError::MaxPacketSizeTooSmall {
                max_packet_size,
                minimum: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
            }) if max_packet_size == below_minimum
        ));
        ssl.max_packet_size = 0;
        ssl.character_set = 33;
        assert!(matches!(
            CODEC.encode_client_ssl_request(0, &ssl),
            Err(ClientSslRequestError::UnsupportedCharacterSet { character_set: 33 })
        ));
    }

    #[test]
    fn rejects_reserved_truncation_and_trailing_bytes() {
        let frame = CODEC
            .encode_client_handshake_response(0, &config())
            .unwrap();
        for length in 0..frame.len() {
            assert!(
                CODEC
                    .decode_client_handshake_response(&frame[..length])
                    .is_err(),
                "truncated frame at {length} unexpectedly decoded"
            );
        }

        let mut reserved = frame.clone();
        reserved[PACKET_HEADER_LEN + 9] = 1;
        assert_eq!(
            CODEC.decode_client_handshake_response(&reserved),
            Err(ClientHandshakeResponseError::NonZeroReservedBytes)
        );

        let mut trailing = frame;
        trailing[0] = trailing[0].wrapping_add(1);
        trailing.push(0);
        assert!(matches!(
            CODEC.decode_client_handshake_response(&trailing),
            Err(ClientHandshakeResponseError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn rejects_malformed_attributes_and_non_utf8() {
        let mut frame = CODEC
            .encode_client_handshake_response(0, &config())
            .unwrap();
        let attrs_start = PACKET_HEADER_LEN + 73;
        frame[attrs_start] = 0xfc;
        frame[attrs_start + 1] = 0x20;
        frame[attrs_start + 2] = 0;
        assert!(matches!(
            CODEC.decode_client_handshake_response(&frame),
            Err(ClientHandshakeResponseError::NonCanonicalLengthEncodedInteger { .. })
        ));

        let mut invalid = CODEC
            .encode_client_handshake_response(0, &config())
            .unwrap();
        invalid[PACKET_HEADER_LEN + 32] = 0xff;
        assert!(matches!(
            CODEC.decode_client_handshake_response(&invalid),
            Err(ClientHandshakeResponseError::InvalidUtf8 { field: "username" })
        ));

        let mut duplicate = config();
        duplicate.connect_attributes = Some(vec![
            ("same".to_owned(), "one".to_owned()),
            ("same".to_owned(), "two".to_owned()),
        ]);
        assert!(matches!(
            CODEC.encode_client_handshake_response(0, &duplicate),
            Err(ClientHandshakeResponseError::DuplicateAttribute { .. })
        ));
    }
}
