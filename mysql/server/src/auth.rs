//! Bounded packet models for the `caching_sha2_password` exchange.
//!
//! These types only parse and frame protocol data. In particular, the full
//! authentication response is borrowed from its input frame so a connection
//! never retains a cleartext password.

use std::{error::Error, fmt, str};

use crate::{PacketCodec, PacketCodecError};

/// The marker used by a server `AuthMoreData` packet.
pub const AUTH_MORE_DATA_HEADER: u8 = 0x01;
/// The `caching_sha2_password` fast-authentication result.
pub const AUTH_MORE_DATA_FAST_AUTH_SUCCESS: u8 = 0x03;
/// The `caching_sha2_password` full-authentication request.
pub const AUTH_MORE_DATA_FULL_AUTH_REQUIRED: u8 = 0x04;
/// The marker used by a server authentication-switch packet.
pub const AUTH_SWITCH_REQUEST_HEADER: u8 = 0xfe;
/// The marker used by a server OK packet.
pub const AUTH_OK_HEADER: u8 = 0x00;

/// Maximum payload accepted by an authentication packet model.
pub const MAX_AUTH_PACKET_PAYLOAD_LENGTH: usize = 4096;
/// Maximum plugin name accepted by an authentication-switch packet.
pub const MAX_AUTH_SWITCH_PLUGIN_NAME_LENGTH: usize = u8::MAX as usize;
/// Maximum opaque plugin data accepted by an authentication-switch packet.
pub const MAX_AUTH_SWITCH_PLUGIN_DATA_LENGTH: usize = u8::MAX as usize;
/// Maximum cleartext authentication response, including its NUL terminator.
pub const MAX_FULL_AUTH_RESPONSE_LENGTH: usize = 256;
/// Maximum informational bytes retained by an authentication OK packet.
pub const MAX_AUTH_INFO_LENGTH: usize = MAX_AUTH_PACKET_PAYLOAD_LENGTH - 7;

/// The two `caching_sha2_password` server authentication outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMoreDataKind {
    /// The cached verifier accepted the client's handshake response.
    FastAuthSuccess,
    /// The server must receive a full response over secure transport.
    FullAuthenticationRequired,
}

/// A strict two-byte server `AuthMoreData` packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthMoreData {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Authentication outcome carried by the packet.
    pub kind: AuthMoreDataKind,
}

impl AuthMoreData {
    /// Encodes one `AuthMoreData` packet.
    pub fn encode(
        codec: PacketCodec,
        sequence_id: u8,
        kind: AuthMoreDataKind,
    ) -> Result<Vec<u8>, AuthPacketError> {
        let code = match kind {
            AuthMoreDataKind::FastAuthSuccess => AUTH_MORE_DATA_FAST_AUTH_SUCCESS,
            AuthMoreDataKind::FullAuthenticationRequired => AUTH_MORE_DATA_FULL_AUTH_REQUIRED,
        };
        codec
            .encode(sequence_id, &[AUTH_MORE_DATA_HEADER, code])
            .map_err(AuthPacketError::from)
    }

    /// Decodes one exact `AuthMoreData` packet.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, AuthPacketError> {
        let packet = codec.decode(frame).map_err(AuthPacketError::from)?;
        if packet.payload.len() != 2 {
            return Err(AuthPacketError::InvalidPayloadLength {
                actual: packet.payload.len(),
                expected: 2,
            });
        }
        if packet.payload[0] != AUTH_MORE_DATA_HEADER {
            return Err(AuthPacketError::UnexpectedMarker {
                actual: packet.payload[0],
                expected: AUTH_MORE_DATA_HEADER,
            });
        }
        let kind = match packet.payload[1] {
            AUTH_MORE_DATA_FAST_AUTH_SUCCESS => AuthMoreDataKind::FastAuthSuccess,
            AUTH_MORE_DATA_FULL_AUTH_REQUIRED => AuthMoreDataKind::FullAuthenticationRequired,
            code => return Err(AuthPacketError::UnsupportedAuthMoreDataCode { code }),
        };
        Ok(Self {
            sequence_id: packet.sequence_id,
            kind,
        })
    }
}

/// Values used to encode a server authentication-switch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSwitchRequestConfig {
    /// NUL-terminated authentication plugin name.
    pub plugin_name: String,
    /// Opaque plugin data sent after the plugin name.
    pub auth_plugin_data: Vec<u8>,
}

impl AuthSwitchRequestConfig {
    /// Creates a switch request from a plugin name and opaque plugin data.
    pub fn new(plugin_name: impl Into<String>, auth_plugin_data: impl Into<Vec<u8>>) -> Self {
        Self {
            plugin_name: plugin_name.into(),
            auth_plugin_data: auth_plugin_data.into(),
        }
    }

    /// Checks the bounded switch-request fields.
    pub fn validate(&self) -> Result<(), AuthPacketError> {
        if self.plugin_name.is_empty() {
            return Err(AuthPacketError::EmptyField {
                field: "authentication plugin name",
            });
        }
        if self.plugin_name.len() > MAX_AUTH_SWITCH_PLUGIN_NAME_LENGTH {
            return Err(AuthPacketError::FieldTooLong {
                field: "authentication plugin name",
                length: self.plugin_name.len(),
                limit: MAX_AUTH_SWITCH_PLUGIN_NAME_LENGTH,
            });
        }
        if let Some(offset) = self
            .plugin_name
            .as_bytes()
            .iter()
            .position(|byte| *byte == 0)
        {
            return Err(AuthPacketError::EmbeddedNul {
                field: "authentication plugin name",
                offset,
            });
        }
        if self.auth_plugin_data.len() > MAX_AUTH_SWITCH_PLUGIN_DATA_LENGTH {
            return Err(AuthPacketError::FieldTooLong {
                field: "authentication plugin data",
                length: self.auth_plugin_data.len(),
                limit: MAX_AUTH_SWITCH_PLUGIN_DATA_LENGTH,
            });
        }
        Ok(())
    }

    /// Encodes this switch request as one framed packet.
    pub fn encode(&self, codec: PacketCodec, sequence_id: u8) -> Result<Vec<u8>, AuthPacketError> {
        self.validate()?;
        let mut payload =
            Vec::with_capacity(1 + self.plugin_name.len() + 1 + self.auth_plugin_data.len());
        payload.push(AUTH_SWITCH_REQUEST_HEADER);
        payload.extend_from_slice(self.plugin_name.as_bytes());
        payload.push(0);
        payload.extend_from_slice(&self.auth_plugin_data);
        codec
            .encode(sequence_id, &payload)
            .map_err(AuthPacketError::from)
    }
}

/// A decoded server authentication-switch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSwitchRequest {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Authentication plugin selected by the server.
    pub plugin_name: String,
    /// Opaque plugin data sent by the server.
    pub auth_plugin_data: Vec<u8>,
}

impl AuthSwitchRequest {
    /// Decodes one bounded authentication-switch packet.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, AuthPacketError> {
        let packet = codec.decode(frame).map_err(AuthPacketError::from)?;
        if packet.payload.len() > MAX_AUTH_PACKET_PAYLOAD_LENGTH {
            return Err(AuthPacketError::PayloadTooLarge {
                length: packet.payload.len(),
                limit: MAX_AUTH_PACKET_PAYLOAD_LENGTH,
            });
        }
        if packet.payload.len() < 2 {
            return Err(AuthPacketError::InvalidPayloadLength {
                actual: packet.payload.len(),
                expected: 2,
            });
        }
        if packet.payload[0] != AUTH_SWITCH_REQUEST_HEADER {
            return Err(AuthPacketError::UnexpectedMarker {
                actual: packet.payload[0],
                expected: AUTH_SWITCH_REQUEST_HEADER,
            });
        }
        let name_end = packet.payload[1..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| offset + 1)
            .ok_or(AuthPacketError::MissingTerminator {
                field: "authentication plugin name",
            })?;
        let plugin_name = str::from_utf8(&packet.payload[1..name_end])
            .map_err(|_| AuthPacketError::InvalidUtf8 {
                field: "authentication plugin name",
            })?
            .to_owned();
        let config = AuthSwitchRequestConfig {
            plugin_name,
            auth_plugin_data: packet.payload[name_end + 1..].to_vec(),
        };
        config.validate()?;
        Ok(Self {
            sequence_id: packet.sequence_id,
            plugin_name: config.plugin_name,
            auth_plugin_data: config.auth_plugin_data,
        })
    }

    /// Encodes this request with a new sequence number.
    pub fn encode(&self, codec: PacketCodec, sequence_id: u8) -> Result<Vec<u8>, AuthPacketError> {
        AuthSwitchRequestConfig::new(&self.plugin_name, self.auth_plugin_data.clone())
            .encode(codec, sequence_id)
    }
}

/// Values used to encode the protocol-4.1 authentication OK packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthOkPacketConfig {
    /// Number of rows affected by the authentication-associated operation.
    pub affected_rows: u64,
    /// Last inserted identifier, normally zero during authentication.
    pub last_insert_id: u64,
    /// Server status flags.
    pub status_flags: u16,
    /// Server warning count.
    pub warnings: u16,
    /// Opaque informational bytes following the fixed fields.
    pub info: Vec<u8>,
}

impl AuthOkPacketConfig {
    /// Creates an OK packet with no informational bytes.
    pub fn new(affected_rows: u64, last_insert_id: u64, status_flags: u16, warnings: u16) -> Self {
        Self {
            affected_rows,
            last_insert_id,
            status_flags,
            warnings,
            info: Vec::new(),
        }
    }

    /// Checks the bounded OK-packet fields.
    pub fn validate(&self) -> Result<(), AuthPacketError> {
        if self.info.len() > MAX_AUTH_INFO_LENGTH {
            return Err(AuthPacketError::FieldTooLong {
                field: "OK packet info",
                length: self.info.len(),
                limit: MAX_AUTH_INFO_LENGTH,
            });
        }
        let payload_length = 1
            + lenenc_integer_len(self.affected_rows)
            + lenenc_integer_len(self.last_insert_id)
            + 4
            + self.info.len();
        if payload_length > MAX_AUTH_PACKET_PAYLOAD_LENGTH {
            return Err(AuthPacketError::PayloadTooLarge {
                length: payload_length,
                limit: MAX_AUTH_PACKET_PAYLOAD_LENGTH,
            });
        }
        Ok(())
    }

    /// Encodes this OK packet as one framed packet.
    pub fn encode(&self, codec: PacketCodec, sequence_id: u8) -> Result<Vec<u8>, AuthPacketError> {
        self.encode_with_header(codec, sequence_id, AUTH_OK_HEADER)
    }

    pub(crate) fn encode_with_header(
        &self,
        codec: PacketCodec,
        sequence_id: u8,
        header: u8,
    ) -> Result<Vec<u8>, AuthPacketError> {
        self.validate()?;
        let mut payload = Vec::with_capacity(7 + self.info.len());
        payload.push(header);
        push_lenenc_integer(&mut payload, self.affected_rows);
        push_lenenc_integer(&mut payload, self.last_insert_id);
        payload.extend_from_slice(&self.status_flags.to_le_bytes());
        payload.extend_from_slice(&self.warnings.to_le_bytes());
        payload.extend_from_slice(&self.info);
        codec
            .encode(sequence_id, &payload)
            .map_err(AuthPacketError::from)
    }
}

impl Default for AuthOkPacketConfig {
    fn default() -> Self {
        Self::new(0, 0, 0x0002, 0)
    }
}

/// A decoded protocol-4.1 authentication OK packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthOkPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Number of rows affected.
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

impl AuthOkPacket {
    /// Decodes one bounded protocol-4.1 authentication OK packet.
    pub fn decode(codec: PacketCodec, frame: &[u8]) -> Result<Self, AuthPacketError> {
        Self::decode_with_header(codec, frame, AUTH_OK_HEADER)
    }

    pub(crate) fn decode_with_header(
        codec: PacketCodec,
        frame: &[u8],
        expected_header: u8,
    ) -> Result<Self, AuthPacketError> {
        let packet = codec.decode(frame).map_err(AuthPacketError::from)?;
        if packet.payload.len() > MAX_AUTH_PACKET_PAYLOAD_LENGTH {
            return Err(AuthPacketError::PayloadTooLarge {
                length: packet.payload.len(),
                limit: MAX_AUTH_PACKET_PAYLOAD_LENGTH,
            });
        }
        if packet.payload.first().copied() != Some(expected_header) {
            return Err(AuthPacketError::UnexpectedMarker {
                actual: packet.payload.first().copied().unwrap_or_default(),
                expected: expected_header,
            });
        }
        let mut reader = AuthPacketReader::new(&packet.payload[1..]);
        let affected_rows = reader.read_lenenc_integer("affected rows")?;
        let last_insert_id = reader.read_lenenc_integer("last insert id")?;
        let status_flags = reader.read_u16("status flags")?;
        let warnings = reader.read_u16("warnings")?;
        let info = reader.remaining().to_vec();
        AuthOkPacketConfig {
            affected_rows,
            last_insert_id,
            status_flags,
            warnings,
            info: info.clone(),
        }
        .validate()?;
        Ok(Self {
            sequence_id: packet.sequence_id,
            affected_rows,
            last_insert_id,
            status_flags,
            warnings,
            info,
        })
    }

    /// Encodes this OK packet with a new sequence number.
    pub fn encode(&self, codec: PacketCodec, sequence_id: u8) -> Result<Vec<u8>, AuthPacketError> {
        AuthOkPacketConfig {
            affected_rows: self.affected_rows,
            last_insert_id: self.last_insert_id,
            status_flags: self.status_flags,
            warnings: self.warnings,
            info: self.info.clone(),
        }
        .encode(codec, sequence_id)
    }
}

/// A borrowed full-authentication response from a client.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ClientAuthResponse<'a> {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Cleartext password bytes without the required trailing NUL.
    pub auth_response: &'a [u8],
}

impl fmt::Debug for ClientAuthResponse<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientAuthResponse")
            .field("sequence_id", &self.sequence_id)
            .field("auth_response", &"<redacted>")
            .finish()
    }
}

impl<'a> ClientAuthResponse<'a> {
    /// Decodes one bounded, NUL-terminated full-authentication response.
    pub fn decode(codec: PacketCodec, frame: &'a [u8]) -> Result<Self, AuthPacketError> {
        let packet = codec.decode(frame).map_err(AuthPacketError::from)?;
        if packet.payload.len() > MAX_FULL_AUTH_RESPONSE_LENGTH {
            return Err(AuthPacketError::PayloadTooLarge {
                length: packet.payload.len(),
                limit: MAX_FULL_AUTH_RESPONSE_LENGTH,
            });
        }
        if packet.payload.last().copied() != Some(0) {
            return Err(AuthPacketError::MissingTerminator {
                field: "full authentication response",
            });
        }
        if let Some(offset) = packet.payload[..packet.payload.len() - 1]
            .iter()
            .position(|byte| *byte == 0)
        {
            return Err(AuthPacketError::EmbeddedNul {
                field: "full authentication response",
                offset,
            });
        }
        Ok(Self {
            sequence_id: packet.sequence_id,
            auth_response: &packet.payload[..packet.payload.len() - 1],
        })
    }
}

impl PacketCodec {
    /// Encodes one server `AuthMoreData` packet.
    pub fn encode_auth_more_data(
        self,
        sequence_id: u8,
        kind: AuthMoreDataKind,
    ) -> Result<Vec<u8>, AuthPacketError> {
        AuthMoreData::encode(self, sequence_id, kind)
    }

    /// Decodes one server `AuthMoreData` packet.
    pub fn decode_auth_more_data(self, frame: &[u8]) -> Result<AuthMoreData, AuthPacketError> {
        AuthMoreData::decode(self, frame)
    }

    /// Encodes one server authentication-switch request.
    pub fn encode_auth_switch_request(
        self,
        sequence_id: u8,
        config: &AuthSwitchRequestConfig,
    ) -> Result<Vec<u8>, AuthPacketError> {
        config.encode(self, sequence_id)
    }

    /// Decodes one server authentication-switch request.
    pub fn decode_auth_switch_request(
        self,
        frame: &[u8],
    ) -> Result<AuthSwitchRequest, AuthPacketError> {
        AuthSwitchRequest::decode(self, frame)
    }

    /// Encodes one server authentication OK packet.
    pub fn encode_auth_ok(
        self,
        sequence_id: u8,
        config: &AuthOkPacketConfig,
    ) -> Result<Vec<u8>, AuthPacketError> {
        config.encode(self, sequence_id)
    }

    /// Decodes one server authentication OK packet.
    pub fn decode_auth_ok(self, frame: &[u8]) -> Result<AuthOkPacket, AuthPacketError> {
        AuthOkPacket::decode(self, frame)
    }

    /// Decodes one borrowed client full-authentication response.
    pub fn decode_client_auth_response<'a>(
        self,
        frame: &'a [u8],
    ) -> Result<ClientAuthResponse<'a>, AuthPacketError> {
        ClientAuthResponse::decode(self, frame)
    }

    /// Encodes a cleartext client authentication response without retaining it.
    pub fn encode_client_auth_response(
        self,
        sequence_id: u8,
        auth_response: &[u8],
    ) -> Result<Vec<u8>, AuthPacketError> {
        let payload_len =
            auth_response
                .len()
                .checked_add(1)
                .ok_or(AuthPacketError::PayloadTooLarge {
                    length: usize::MAX,
                    limit: MAX_FULL_AUTH_RESPONSE_LENGTH,
                })?;
        if payload_len > MAX_FULL_AUTH_RESPONSE_LENGTH {
            return Err(AuthPacketError::PayloadTooLarge {
                length: payload_len,
                limit: MAX_FULL_AUTH_RESPONSE_LENGTH,
            });
        }
        if let Some(offset) = auth_response.iter().position(|byte| *byte == 0) {
            return Err(AuthPacketError::EmbeddedNul {
                field: "full authentication response",
                offset,
            });
        }
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(auth_response);
        payload.push(0);
        self.encode(sequence_id, &payload)
            .map_err(AuthPacketError::from)
    }
}

/// Errors returned while framing or decoding authentication packets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPacketError {
    /// Packet framing rejected the frame.
    PacketCodec(PacketCodecError),
    /// A fixed packet did not have the required payload length.
    InvalidPayloadLength { actual: usize, expected: usize },
    /// A packet marker was not valid for its model.
    UnexpectedMarker { actual: u8, expected: u8 },
    /// An `AuthMoreData` status byte is not implemented here.
    UnsupportedAuthMoreDataCode { code: u8 },
    /// A required NUL terminator was absent.
    MissingTerminator { field: &'static str },
    /// A text field contained a NUL before its terminator.
    EmbeddedNul { field: &'static str, offset: usize },
    /// A text field was not UTF-8.
    InvalidUtf8 { field: &'static str },
    /// A text field was empty.
    EmptyField { field: &'static str },
    /// A bounded field exceeded its configured limit.
    FieldTooLong {
        field: &'static str,
        length: usize,
        limit: usize,
    },
    /// A packet exceeded the authentication parser's independent bound.
    PayloadTooLarge { length: usize, limit: usize },
    /// A length-encoded integer used the NULL marker where an integer is required.
    NullLengthEncodedInteger { field: &'static str },
    /// A length-encoded integer had an unsupported marker.
    InvalidLengthEncodedInteger { marker: u8 },
    /// A length-encoded integer was truncated.
    TruncatedField { field: &'static str },
}

impl From<PacketCodecError> for AuthPacketError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl fmt::Display for AuthPacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::InvalidPayloadLength { actual, expected } => {
                write!(
                    f,
                    "authentication payload is {actual} bytes, expected {expected}"
                )
            }
            Self::UnexpectedMarker { actual, expected } => {
                write!(
                    f,
                    "authentication packet marker is 0x{actual:02x}, expected 0x{expected:02x}"
                )
            }
            Self::UnsupportedAuthMoreDataCode { code } => {
                write!(f, "unsupported AuthMoreData code 0x{code:02x}")
            }
            Self::MissingTerminator { field } => write!(f, "{field} is missing its NUL terminator"),
            Self::EmbeddedNul { field, offset } => {
                write!(f, "{field} contains an embedded NUL at byte {offset}")
            }
            Self::InvalidUtf8 { field } => write!(f, "{field} is not valid UTF-8"),
            Self::EmptyField { field } => write!(f, "{field} must not be empty"),
            Self::FieldTooLong {
                field,
                length,
                limit,
            } => write!(f, "{field} is {length} bytes, limit is {limit}"),
            Self::PayloadTooLarge { length, limit } => {
                write!(f, "authentication payload {length} exceeds limit {limit}")
            }
            Self::NullLengthEncodedInteger { field } => {
                write!(f, "{field} cannot be a NULL length-encoded integer")
            }
            Self::InvalidLengthEncodedInteger { marker } => {
                write!(f, "invalid length-encoded integer marker 0x{marker:02x}")
            }
            Self::TruncatedField { field } => write!(f, "{field} is truncated"),
        }
    }
}

impl Error for AuthPacketError {}

fn push_lenenc_integer(payload: &mut Vec<u8>, value: u64) {
    match value {
        0..=250 => payload.push(value as u8),
        251..=65_535 => {
            payload.push(0xfc);
            payload.extend_from_slice(&(value as u16).to_le_bytes());
        }
        65_536..=0x00ff_ffff => {
            payload.push(0xfd);
            payload.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
        }
        value => {
            payload.push(0xfe);
            payload.extend_from_slice(&value.to_le_bytes());
        }
    }
}

fn lenenc_integer_len(value: u64) -> usize {
    match value {
        0..=250 => 1,
        251..=65_535 => 3,
        65_536..=0x00ff_ffff => 4,
        _ => 9,
    }
}

struct AuthPacketReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> AuthPacketReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_u8(&mut self, field: &'static str) -> Result<u8, AuthPacketError> {
        let byte = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or(AuthPacketError::TruncatedField { field })?;
        self.offset += 1;
        Ok(byte)
    }

    fn read_u16(&mut self, field: &'static str) -> Result<u16, AuthPacketError> {
        let bytes = self
            .bytes
            .get(self.offset..self.offset + 2)
            .ok_or(AuthPacketError::TruncatedField { field })?;
        self.offset += 2;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_lenenc_integer(&mut self, field: &'static str) -> Result<u64, AuthPacketError> {
        match self.read_u8(field)? {
            value @ 0..=250 => Ok(u64::from(value)),
            0xfb => Err(AuthPacketError::NullLengthEncodedInteger { field }),
            0xfc => Ok(u64::from(self.read_u16(field)?)),
            0xfd => {
                let bytes = self
                    .bytes
                    .get(self.offset..self.offset + 3)
                    .ok_or(AuthPacketError::TruncatedField { field })?;
                self.offset += 3;
                Ok(u64::from(bytes[0]) | (u64::from(bytes[1]) << 8) | (u64::from(bytes[2]) << 16))
            }
            0xfe => {
                let bytes = self
                    .bytes
                    .get(self.offset..self.offset + 8)
                    .ok_or(AuthPacketError::TruncatedField { field })?;
                self.offset += 8;
                Ok(u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]))
            }
            marker => Err(AuthPacketError::InvalidLengthEncodedInteger { marker }),
        }
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: MAX_AUTH_PACKET_PAYLOAD_LENGTH,
    };

    #[test]
    fn auth_more_data_round_trips_only_supported_statuses() {
        for kind in [
            AuthMoreDataKind::FastAuthSuccess,
            AuthMoreDataKind::FullAuthenticationRequired,
        ] {
            let frame = AuthMoreData::encode(CODEC, 2, kind).unwrap();
            assert_eq!(
                AuthMoreData::decode(CODEC, &frame).unwrap(),
                AuthMoreData {
                    sequence_id: 2,
                    kind,
                }
            );
        }
        let unsupported = CODEC.encode(2, &[AUTH_MORE_DATA_HEADER, 0x05]).unwrap();
        assert_eq!(
            AuthMoreData::decode(CODEC, &unsupported),
            Err(AuthPacketError::UnsupportedAuthMoreDataCode { code: 0x05 })
        );
    }

    #[test]
    fn auth_switch_round_trips_and_rejects_bad_fields() {
        let config = AuthSwitchRequestConfig::new("caching_sha2_password", [1, 2, 3]);
        let frame = config.encode(CODEC, 4).unwrap();
        let decoded = AuthSwitchRequest::decode(CODEC, &frame).unwrap();
        assert_eq!(decoded.sequence_id, 4);
        assert_eq!(decoded.plugin_name, "caching_sha2_password");
        assert_eq!(decoded.auth_plugin_data, [1, 2, 3]);

        let malformed = CODEC
            .encode(4, &[AUTH_SWITCH_REQUEST_HEADER, b'x'])
            .unwrap();
        assert_eq!(
            AuthSwitchRequest::decode(CODEC, &malformed),
            Err(AuthPacketError::MissingTerminator {
                field: "authentication plugin name"
            })
        );
    }

    #[test]
    fn ok_packet_round_trips_length_encoded_fields() {
        let config = AuthOkPacketConfig {
            affected_rows: 65_537,
            last_insert_id: u64::MAX,
            status_flags: 2,
            warnings: 1,
            info: b"authenticated".to_vec(),
        };
        let frame = config.encode(CODEC, 5).unwrap();
        let decoded = AuthOkPacket::decode(CODEC, &frame).unwrap();
        assert_eq!(decoded.sequence_id, 5);
        assert_eq!(decoded.affected_rows, config.affected_rows);
        assert_eq!(decoded.last_insert_id, config.last_insert_id);
        assert_eq!(decoded.info, config.info);

        let truncated = CODEC.encode(5, &[AUTH_OK_HEADER, 0, 0]).unwrap();
        assert!(matches!(
            AuthOkPacket::decode(CODEC, &truncated),
            Err(AuthPacketError::TruncatedField {
                field: "status flags"
            })
        ));
    }

    #[test]
    fn full_auth_response_is_borrowed_and_strictly_terminated() {
        let frame = CODEC.encode_client_auth_response(3, b"secret").unwrap();
        let response = ClientAuthResponse::decode(CODEC, &frame).unwrap();
        assert_eq!(response.sequence_id, 3);
        assert_eq!(response.auth_response, b"secret");

        let embedded = CODEC.encode(3, b"sec\0ret\0").unwrap();
        assert!(matches!(
            ClientAuthResponse::decode(CODEC, &embedded),
            Err(AuthPacketError::EmbeddedNul { offset: 3, .. })
        ));
        let missing = CODEC.encode(3, b"secret").unwrap();
        assert!(matches!(
            ClientAuthResponse::decode(CODEC, &missing),
            Err(AuthPacketError::MissingTerminator { .. })
        ));
    }
}
