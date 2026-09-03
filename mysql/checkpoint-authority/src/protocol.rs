// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Strict, fixed local authority protocol codec.

use std::{error::Error, fmt};

use turso_mysql_server::AccountStoreCheckpoint;

/// The largest accepted payload, excluding its four-byte length prefix.
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 512;
/// The largest accepted complete frame.
pub const MAX_FRAME_BYTES: usize = 4 + MAX_FRAME_PAYLOAD_BYTES;
/// The exact encoded account checkpoint length.
pub const CHECKPOINT_BYTES: usize = 72;

const MAGIC: &[u8; 4] = b"TMCA";
const VERSION: u8 = 1;
const REQUEST_HEADER_BYTES: usize = 10;
const RESPONSE_HEADER_BYTES: usize = 8;
const MAX_AUTHORITY_ID_BYTES: usize = 256;

const OP_GET: u8 = 1;
const OP_COMPARE_AND_PERSIST: u8 = 2;

const STATUS_OK: u8 = 0;
const STATUS_MISSING: u8 = 1;
const STATUS_CONFLICT: u8 = 1;
const STATUS_INVALID: u8 = 2;
const STATUS_FAILED: u8 = 2;

/// A configured, non-path authority name.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AuthorityId(String);

impl AuthorityId {
    /// Validates a bounded authority name without treating it as a filename.
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_AUTHORITY_ID_BYTES
            || bytes.contains(&0)
            || value == "."
            || value == ".."
            || value.starts_with('/')
            || value.contains('/')
            || value.contains('\\')
        {
            return Err(ProtocolError::InvalidAuthority);
        }
        Ok(Self(value))
    }

    /// Returns the configured authority name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AuthorityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthorityId(<redacted>)")
    }
}

/// One authority operation.
#[derive(Clone, PartialEq, Eq)]
pub enum Request {
    /// Reads the one current checkpoint for this authority.
    Get { authority: AuthorityId },
    /// Compares and durably persists a replacement checkpoint.
    CompareAndPersist {
        authority: AuthorityId,
        expected: Option<AccountStoreCheckpoint>,
        replacement: AccountStoreCheckpoint,
    },
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Get { authority } => f
                .debug_struct("Request::Get")
                .field("authority", authority)
                .finish(),
            Self::CompareAndPersist { authority, .. } => f
                .debug_struct("Request::CompareAndPersist")
                .field("authority", authority)
                .field("checkpoint", &"<redacted>")
                .finish(),
        }
    }
}

/// The result of a checkpoint read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GetResponse {
    /// The authority has one exact current checkpoint.
    Checkpoint(AccountStoreCheckpoint),
    /// No checkpoint was provisioned yet.
    Missing,
    /// Authority state was corrupt or otherwise rejected.
    Invalid,
}

impl fmt::Debug for GetResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Checkpoint(_) => f.write_str("GetResponse::Checkpoint(<redacted>)"),
            Self::Missing => f.write_str("GetResponse::Missing"),
            Self::Invalid => f.write_str("GetResponse::Invalid"),
        }
    }
}

/// The result of a durable compare-and-persist operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasResponse {
    /// The replacement is durable, including an idempotent repeat.
    Durable,
    /// The expected checkpoint did not equal the current checkpoint.
    Conflict,
    /// The authority definitely did not durably persist the replacement.
    Failed,
}

/// A response corresponding to one request operation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// A response to [`Request::Get`].
    Get(GetResponse),
    /// A response to [`Request::CompareAndPersist`].
    CompareAndPersist(CasResponse),
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Get(response) => f.debug_tuple("Response::Get").field(response).finish(),
            Self::CompareAndPersist(response) => f
                .debug_tuple("Response::CompareAndPersist")
                .field(response)
                .finish(),
        }
    }
}

/// A malformed or unsupported local-authority protocol frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    /// The complete frame length was not accepted.
    InvalidFrame,
    /// The protocol magic or version was not accepted.
    UnsupportedVersion,
    /// An operation, status, or reserved value was not accepted.
    InvalidOperation,
    /// The authority name was malformed or unsuitable for this protocol.
    InvalidAuthority,
    /// A checkpoint was malformed.
    InvalidCheckpoint,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrame => f.write_str("local checkpoint protocol frame is invalid"),
            Self::UnsupportedVersion => {
                f.write_str("local checkpoint protocol version is unsupported")
            }
            Self::InvalidOperation => f.write_str("local checkpoint protocol operation is invalid"),
            Self::InvalidAuthority => f.write_str("local checkpoint authority is invalid"),
            Self::InvalidCheckpoint => f.write_str("local checkpoint is invalid"),
        }
    }
}

impl Error for ProtocolError {}

/// Encodes one complete, bounded request frame.
pub fn encode_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    let (operation, authority, body) = match request {
        Request::Get { authority } => (OP_GET, authority, Vec::new()),
        Request::CompareAndPersist {
            authority,
            expected,
            replacement,
        } => {
            let mut body = Vec::with_capacity(1 + CHECKPOINT_BYTES * 2);
            match expected {
                Some(checkpoint) => {
                    body.push(1);
                    body.extend_from_slice(&checkpoint.to_bytes());
                }
                None => body.push(0),
            }
            body.extend_from_slice(&replacement.to_bytes());
            (OP_COMPARE_AND_PERSIST, authority, body)
        }
    };
    let authority = authority.as_str().as_bytes();
    let authority_len: u16 = authority
        .len()
        .try_into()
        .map_err(|_| ProtocolError::InvalidAuthority)?;
    let payload_len = REQUEST_HEADER_BYTES
        .checked_add(authority.len())
        .and_then(|value| value.checked_add(body.len()))
        .ok_or(ProtocolError::InvalidFrame)?;
    let mut payload = Vec::with_capacity(payload_len);
    payload.extend_from_slice(MAGIC);
    payload.push(VERSION);
    payload.push(operation);
    payload.extend_from_slice(&0_u16.to_be_bytes());
    payload.extend_from_slice(&authority_len.to_be_bytes());
    payload.extend_from_slice(authority);
    payload.extend_from_slice(&body);
    frame(payload)
}

/// Decodes one complete request frame without accepting trailing bytes.
pub fn decode_request(frame: &[u8]) -> Result<Request, ProtocolError> {
    let payload = payload(frame)?;
    if payload.len() < REQUEST_HEADER_BYTES || payload[..4] != *MAGIC || payload[4] != VERSION {
        return Err(if payload.len() >= 5 && payload[..4] == *MAGIC {
            ProtocolError::UnsupportedVersion
        } else {
            ProtocolError::InvalidFrame
        });
    }
    if payload[6..8] != [0, 0] {
        return Err(ProtocolError::InvalidOperation);
    }
    let authority_len = usize::from(u16::from_be_bytes([payload[8], payload[9]]));
    let authority_end = REQUEST_HEADER_BYTES
        .checked_add(authority_len)
        .ok_or(ProtocolError::InvalidFrame)?;
    if authority_end > payload.len() {
        return Err(ProtocolError::InvalidFrame);
    }
    let authority = std::str::from_utf8(&payload[REQUEST_HEADER_BYTES..authority_end])
        .map_err(|_| ProtocolError::InvalidAuthority)
        .and_then(|value| AuthorityId::new(value.to_owned()))?;
    let body = &payload[authority_end..];
    match payload[5] {
        OP_GET if body.is_empty() => Ok(Request::Get { authority }),
        OP_COMPARE_AND_PERSIST => decode_cas_request(authority, body),
        OP_GET => Err(ProtocolError::InvalidFrame),
        _ => Err(ProtocolError::InvalidOperation),
    }
}

/// Encodes one complete bounded response frame.
pub fn encode_response(response: Response) -> Result<Vec<u8>, ProtocolError> {
    let (operation, status, body) = match response {
        Response::Get(GetResponse::Checkpoint(checkpoint)) => {
            (OP_GET, STATUS_OK, checkpoint.to_bytes().to_vec())
        }
        Response::Get(GetResponse::Missing) => (OP_GET, STATUS_MISSING, Vec::new()),
        Response::Get(GetResponse::Invalid) => (OP_GET, STATUS_INVALID, Vec::new()),
        Response::CompareAndPersist(CasResponse::Durable) => {
            (OP_COMPARE_AND_PERSIST, STATUS_OK, Vec::new())
        }
        Response::CompareAndPersist(CasResponse::Conflict) => {
            (OP_COMPARE_AND_PERSIST, STATUS_CONFLICT, Vec::new())
        }
        Response::CompareAndPersist(CasResponse::Failed) => {
            (OP_COMPARE_AND_PERSIST, STATUS_FAILED, Vec::new())
        }
    };
    let mut payload = Vec::with_capacity(RESPONSE_HEADER_BYTES + body.len());
    payload.extend_from_slice(MAGIC);
    payload.push(VERSION);
    payload.push(operation);
    payload.push(status);
    payload.push(0);
    payload.extend_from_slice(&body);
    frame(payload)
}

/// Decodes one complete response frame without accepting trailing bytes.
pub fn decode_response(frame: &[u8]) -> Result<Response, ProtocolError> {
    let payload = payload(frame)?;
    if payload.len() < RESPONSE_HEADER_BYTES || payload[..4] != *MAGIC || payload[4] != VERSION {
        return Err(if payload.len() >= 5 && payload[..4] == *MAGIC {
            ProtocolError::UnsupportedVersion
        } else {
            ProtocolError::InvalidFrame
        });
    }
    if payload[7] != 0 {
        return Err(ProtocolError::InvalidOperation);
    }
    let body = &payload[RESPONSE_HEADER_BYTES..];
    match (payload[5], payload[6]) {
        (OP_GET, STATUS_OK) if body.len() == CHECKPOINT_BYTES => Ok(Response::Get(
            GetResponse::Checkpoint(parse_checkpoint(body)?),
        )),
        (OP_GET, STATUS_MISSING) if body.is_empty() => Ok(Response::Get(GetResponse::Missing)),
        (OP_GET, STATUS_INVALID) if body.is_empty() => Ok(Response::Get(GetResponse::Invalid)),
        (OP_COMPARE_AND_PERSIST, STATUS_OK) if body.is_empty() => {
            Ok(Response::CompareAndPersist(CasResponse::Durable))
        }
        (OP_COMPARE_AND_PERSIST, STATUS_CONFLICT) if body.is_empty() => {
            Ok(Response::CompareAndPersist(CasResponse::Conflict))
        }
        (OP_COMPARE_AND_PERSIST, STATUS_FAILED) if body.is_empty() => {
            Ok(Response::CompareAndPersist(CasResponse::Failed))
        }
        (OP_GET | OP_COMPARE_AND_PERSIST, _) => Err(ProtocolError::InvalidOperation),
        _ => Err(ProtocolError::InvalidOperation),
    }
}

fn decode_cas_request(authority: AuthorityId, body: &[u8]) -> Result<Request, ProtocolError> {
    let Some((&tag, rest)) = body.split_first() else {
        return Err(ProtocolError::InvalidFrame);
    };
    let (expected, replacement) = match tag {
        0 if rest.len() == CHECKPOINT_BYTES => (None, parse_checkpoint(rest)?),
        1 if rest.len() == CHECKPOINT_BYTES * 2 => (
            Some(parse_checkpoint(&rest[..CHECKPOINT_BYTES])?),
            parse_checkpoint(&rest[CHECKPOINT_BYTES..])?,
        ),
        0 | 1 => return Err(ProtocolError::InvalidFrame),
        _ => return Err(ProtocolError::InvalidOperation),
    };
    Ok(Request::CompareAndPersist {
        authority,
        expected,
        replacement,
    })
}

fn parse_checkpoint(bytes: &[u8]) -> Result<AccountStoreCheckpoint, ProtocolError> {
    if bytes.len() != CHECKPOINT_BYTES {
        return Err(ProtocolError::InvalidCheckpoint);
    }
    AccountStoreCheckpoint::from_bytes(bytes).map_err(|_| ProtocolError::InvalidCheckpoint)
}

fn frame(payload: Vec<u8>) -> Result<Vec<u8>, ProtocolError> {
    if payload.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(ProtocolError::InvalidFrame);
    }
    let payload_len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| ProtocolError::InvalidFrame)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn payload(frame: &[u8]) -> Result<&[u8], ProtocolError> {
    if frame.len() < 4 || frame.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidFrame);
    }
    let declared = usize::try_from(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]))
        .map_err(|_| ProtocolError::InvalidFrame)?;
    if declared > MAX_FRAME_PAYLOAD_BYTES || frame.len() != 4 + declared {
        return Err(ProtocolError::InvalidFrame);
    }
    Ok(&frame[4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(byte: u8) -> AccountStoreCheckpoint {
        let mut bytes = [0_u8; CHECKPOINT_BYTES];
        bytes[..32].fill(byte.max(1));
        bytes[40..].fill(byte);
        AccountStoreCheckpoint::from_bytes(&bytes).unwrap()
    }

    #[test]
    fn requests_round_trip_without_exposing_checkpoints() {
        let authority = AuthorityId::new("local-accounts-v1").unwrap();
        let requests = [
            Request::Get {
                authority: authority.clone(),
            },
            Request::CompareAndPersist {
                authority,
                expected: Some(checkpoint(4)),
                replacement: checkpoint(9),
            },
        ];
        for request in requests {
            let frame = encode_request(&request).unwrap();
            assert_eq!(decode_request(&frame).unwrap(), request);
            assert!(!format!("{request:?}").contains("040404"));
        }
    }

    #[test]
    fn responses_round_trip() {
        let responses = [
            Response::Get(GetResponse::Checkpoint(checkpoint(1))),
            Response::Get(GetResponse::Missing),
            Response::Get(GetResponse::Invalid),
            Response::CompareAndPersist(CasResponse::Durable),
            Response::CompareAndPersist(CasResponse::Conflict),
            Response::CompareAndPersist(CasResponse::Failed),
        ];
        for response in responses {
            let frame = encode_response(response).unwrap();
            assert_eq!(decode_response(&frame).unwrap(), response);
        }
    }

    #[test]
    fn rejects_bad_lengths_versions_and_trailing_data() {
        let request = encode_request(&Request::Get {
            authority: AuthorityId::new("authority").unwrap(),
        })
        .unwrap();
        let mut wrong_length = request.clone();
        wrong_length[3] = wrong_length[3].saturating_add(1);
        assert_eq!(
            decode_request(&wrong_length),
            Err(ProtocolError::InvalidFrame)
        );

        let mut wrong_version = request.clone();
        wrong_version[8] = VERSION + 1;
        assert_eq!(
            decode_request(&wrong_version),
            Err(ProtocolError::UnsupportedVersion)
        );

        let mut trailing = request;
        trailing.push(0);
        assert_eq!(decode_request(&trailing), Err(ProtocolError::InvalidFrame));
    }

    #[test]
    fn rejects_malformed_checkpoints_and_reserved_bytes() {
        let request = Request::CompareAndPersist {
            authority: AuthorityId::new("authority").unwrap(),
            expected: None,
            replacement: checkpoint(2),
        };
        let mut frame = encode_request(&request).unwrap();
        let checkpoint_start = frame.len() - CHECKPOINT_BYTES;
        frame[checkpoint_start..checkpoint_start + 32].fill(0);
        assert_eq!(
            decode_request(&frame),
            Err(ProtocolError::InvalidCheckpoint)
        );

        let mut response = encode_response(Response::Get(GetResponse::Missing)).unwrap();
        response[11] = 1;
        assert_eq!(
            decode_response(&response),
            Err(ProtocolError::InvalidOperation)
        );
    }

    #[test]
    fn authority_ids_are_not_paths() {
        for value in ["", ".", "..", "/tmp/a", "a/b", "a\\b"] {
            assert_eq!(
                AuthorityId::new(value),
                Err(ProtocolError::InvalidAuthority)
            );
        }
    }
}
