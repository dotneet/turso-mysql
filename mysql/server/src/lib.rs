//! Bounded classic MySQL packet framing, stream adapters, and connection state.
//!
//! The crate owns bounded Unix and mandatory-TLS TCP runtime boundaries. SQL
//! execution remains behind the explicit connection state machine and checked
//! frontend adapters.

#[cfg(unix)]
mod account_store;
#[cfg(unix)]
mod account_store_format;
#[cfg(unix)]
mod account_store_fs;
mod auth;
mod authorization;
mod client_handshake;
mod connection_facts;
mod connection_state;
mod dispatcher;
mod frontend_adapter;
mod handshake;
#[cfg(unix)]
mod offline_provisioning;
mod orchestrator;
#[cfg(unix)]
mod persistent_account_store;
mod response;
#[cfg(unix)]
mod runtime_account_administration;
#[cfg(unix)]
mod runtime_account_reload_supervisor;
#[cfg(unix)]
mod runtime_account_store;
#[cfg(unix)]
mod runtime_config;
#[cfg(unix)]
mod runtime_tcp_connection;
#[cfg(unix)]
mod runtime_tcp_listener;
#[cfg(unix)]
mod runtime_tcp_server;
#[cfg(unix)]
mod runtime_tls;
#[cfg(unix)]
mod runtime_unix_connection;
#[cfg(unix)]
mod runtime_unix_listener;
#[cfg(unix)]
mod runtime_unix_server;
mod session_variables;
mod statement_execute;
mod static_result_metadata;
mod stream;
#[cfg(unix)]
mod unix_peer;
#[cfg(unix)]
mod unix_socket_fs;
mod verifier;

#[cfg(unix)]
pub use account_store::*;
pub use auth::*;
pub use authorization::*;
pub use client_handshake::*;
pub use connection_state::*;
pub use dispatcher::*;
pub use frontend_adapter::*;
pub use handshake::*;
#[cfg(unix)]
pub use offline_provisioning::*;
pub use orchestrator::*;
#[cfg(unix)]
pub use persistent_account_store::*;
pub use response::*;
#[cfg(unix)]
pub use runtime_account_administration::*;
#[cfg(unix)]
pub use runtime_account_store::*;
#[cfg(unix)]
pub use runtime_config::*;
#[cfg(unix)]
pub use runtime_tcp_connection::*;
#[cfg(unix)]
pub use runtime_tcp_listener::*;
#[cfg(unix)]
pub use runtime_tcp_server::*;
#[cfg(unix)]
pub use runtime_tls::*;
#[cfg(unix)]
pub use runtime_unix_connection::*;
#[cfg(unix)]
pub use runtime_unix_listener::*;
#[cfg(unix)]
pub use runtime_unix_server::*;
pub use statement_execute::*;
pub use stream::*;
#[cfg(unix)]
pub use unix_peer::{UnixPeerError, UnixPeerVerifier};
pub use verifier::*;

use std::{error::Error, fmt};

/// The number of bytes in a classic MySQL packet header.
pub const PACKET_HEADER_LEN: usize = 4;

/// The largest payload length representable by MySQL's three-byte header.
pub const MAX_PACKET_PAYLOAD_LEN: usize = 0xFF_FFFF;

/// The stack each connection's thread runs on.
///
/// A statement is read and planned by recursion over its nesting, and a
/// thread's default 2 MiB runs out on statements a real application sends —
/// Gitea's branch listing nests five subqueries, joins inside two of them
/// and ORs twelve tests — which aborts the whole server rather than refuse the
/// one statement. The stack is reserved, not committed, so an idle connection
/// costs no more memory than before.
pub(crate) const CONNECTION_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Returns the maximum payload size reported by `mysql_common`.
///
/// Keeping this check in one place makes a dependency version change fail
/// loudly if its protocol constant ever stops matching the wire format used
/// here.
pub fn mysql_common_max_payload_len() -> usize {
    mysql_common::constants::MAX_PAYLOAD_LEN
}

/// A decoded classic MySQL packet that borrows its input frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Packet payload, without its four-byte header.
    pub payload: &'a [u8],
}

/// Limits and validates classic packet frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketCodec {
    max_payload_len: usize,
}

impl PacketCodec {
    /// Creates a codec with a non-zero payload limit.
    ///
    /// The configured limit cannot exceed the protocol's 24-bit payload
    /// field. A smaller limit is useful for applying the server's negotiated
    /// resource policy before allocating an encoded frame.
    pub fn new(max_payload_len: usize) -> Result<Self, PacketCodecError> {
        if max_payload_len == 0 {
            return Err(PacketCodecError::ZeroPayloadLimit);
        }
        if max_payload_len > MAX_PACKET_PAYLOAD_LEN {
            return Err(PacketCodecError::PayloadLimitExceedsWireMaximum {
                limit: max_payload_len,
                wire_maximum: MAX_PACKET_PAYLOAD_LEN,
            });
        }
        if mysql_common_max_payload_len() != MAX_PACKET_PAYLOAD_LEN {
            return Err(PacketCodecError::DependencyWireMaximumMismatch {
                dependency: mysql_common_max_payload_len(),
                codec: MAX_PACKET_PAYLOAD_LEN,
            });
        }
        Ok(Self { max_payload_len })
    }

    /// Returns the maximum payload accepted by this codec.
    pub const fn max_payload_len(self) -> usize {
        self.max_payload_len
    }

    /// Encodes one packet header and payload into an owned frame.
    pub fn encode(self, sequence_id: u8, payload: &[u8]) -> Result<Vec<u8>, PacketCodecError> {
        self.check_payload_len(payload.len())?;

        let payload_len = payload.len();
        let mut frame = Vec::with_capacity(PACKET_HEADER_LEN + payload_len);
        frame.push((payload_len & 0xFF) as u8);
        frame.push(((payload_len >> 8) & 0xFF) as u8);
        frame.push(((payload_len >> 16) & 0xFF) as u8);
        frame.push(sequence_id);
        frame.extend_from_slice(payload);
        Ok(frame)
    }

    /// Decodes exactly one packet frame without allocating its payload.
    ///
    /// A frame must contain exactly the four-byte header followed by the
    /// declared payload. Rejecting trailing bytes keeps packet boundaries
    /// explicit until a stream decoder is introduced.
    pub fn decode<'a>(self, frame: &'a [u8]) -> Result<Packet<'a>, PacketCodecError> {
        if frame.len() < PACKET_HEADER_LEN {
            return Err(PacketCodecError::TruncatedHeader {
                actual: frame.len(),
            });
        }

        let payload_len =
            usize::from(frame[0]) | (usize::from(frame[1]) << 8) | (usize::from(frame[2]) << 16);
        self.check_payload_len(payload_len)?;

        let expected_len = PACKET_HEADER_LEN
            .checked_add(payload_len)
            .expect("24-bit payload length cannot overflow a usize");
        match frame.len().cmp(&expected_len) {
            std::cmp::Ordering::Less => {
                return Err(PacketCodecError::TruncatedPayload {
                    declared: payload_len,
                    actual: frame.len() - PACKET_HEADER_LEN,
                });
            }
            std::cmp::Ordering::Greater => {
                return Err(PacketCodecError::TrailingBytes {
                    expected: expected_len,
                    actual: frame.len(),
                });
            }
            std::cmp::Ordering::Equal => {}
        }

        Ok(Packet {
            sequence_id: frame[3],
            payload: &frame[PACKET_HEADER_LEN..],
        })
    }

    fn check_payload_len(self, payload_len: usize) -> Result<(), PacketCodecError> {
        if payload_len > self.max_payload_len {
            return Err(PacketCodecError::PayloadTooLarge {
                length: payload_len,
                limit: self.max_payload_len,
            });
        }
        Ok(())
    }
}

/// Writes one payload as the packets the wire carries it in.
///
/// A payload of 0xFFFFFF bytes or more is sent as full 0xFFFFFF-byte packets
/// followed by one shorter packet, which is empty when the payload is an exact
/// multiple, because a full packet always tells the reader that another one
/// follows. The packets take consecutive sequence numbers from `sequence_id`.
pub fn encode_split_payload(sequence_id: u8, payload: &[u8]) -> Vec<u8> {
    let packet_count = payload.len() / MAX_PACKET_PAYLOAD_LEN + 1;
    let mut frame = Vec::with_capacity(payload.len() + packet_count * PACKET_HEADER_LEN);
    let mut sequence_id = sequence_id;
    let mut rest = payload;
    loop {
        let length = rest.len().min(MAX_PACKET_PAYLOAD_LEN);
        frame.extend_from_slice(&[
            (length & 0xFF) as u8,
            ((length >> 8) & 0xFF) as u8,
            ((length >> 16) & 0xFF) as u8,
            sequence_id,
        ]);
        frame.extend_from_slice(&rest[..length]);
        rest = &rest[length..];
        sequence_id = sequence_id.wrapping_add(1);
        if length < MAX_PACKET_PAYLOAD_LEN {
            return frame;
        }
    }
}

pub(crate) fn frame_with_room_for_its_header(payload_length: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(PACKET_HEADER_LEN + payload_length);
    frame.extend_from_slice(&[0; PACKET_HEADER_LEN]);
    frame
}

pub(crate) fn write_the_header_before_the_payload(mut frame: Vec<u8>, sequence_id: u8) -> Vec<u8> {
    let length = frame.len() - PACKET_HEADER_LEN;
    if length >= MAX_PACKET_PAYLOAD_LEN {
        return encode_split_payload(sequence_id, &frame[PACKET_HEADER_LEN..]);
    }
    frame[..PACKET_HEADER_LEN].copy_from_slice(&[
        (length & 0xFF) as u8,
        ((length >> 8) & 0xFF) as u8,
        ((length >> 16) & 0xFF) as u8,
        sequence_id,
    ]);
    frame
}

/// Counts the packets in a frame holding one payload, split or not, and
/// checks that every packet but the last is full and the last is not.
pub fn split_payload_packet_count(frame: &[u8]) -> Result<usize, PacketCodecError> {
    let mut offset = 0;
    let mut packets = 0;
    loop {
        let header = frame.get(offset..offset + PACKET_HEADER_LEN).ok_or(
            PacketCodecError::TruncatedHeader {
                actual: frame.len() - offset,
            },
        )?;
        let length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let end = offset + PACKET_HEADER_LEN + length;
        if end > frame.len() {
            return Err(PacketCodecError::TruncatedPayload {
                declared: length,
                actual: frame.len() - offset - PACKET_HEADER_LEN,
            });
        }
        packets += 1;
        offset = end;
        if length < MAX_PACKET_PAYLOAD_LEN {
            if offset != frame.len() {
                return Err(PacketCodecError::TrailingBytes {
                    expected: offset,
                    actual: frame.len(),
                });
            }
            return Ok(packets);
        }
    }
}

/// Gives the packets of an encoded frame consecutive sequence numbers from
/// `sequence_id` and returns the number the packet after them takes.
///
/// The frame is one the server encoded, so a header that runs past its end is
/// a bug rather than a client's mistake.
pub fn renumber_packets(frame: &mut [u8], sequence_id: u8) -> u8 {
    let mut sequence_id = sequence_id;
    let mut offset = 0;
    while offset < frame.len() {
        assert!(
            offset + PACKET_HEADER_LEN <= frame.len(),
            "an encoded frame ends with a whole packet"
        );
        let length = usize::from(frame[offset])
            | (usize::from(frame[offset + 1]) << 8)
            | (usize::from(frame[offset + 2]) << 16);
        frame[offset + PACKET_HEADER_LEN - 1] = sequence_id;
        sequence_id = sequence_id.wrapping_add(1);
        offset += PACKET_HEADER_LEN + length;
    }
    assert_eq!(
        offset,
        frame.len(),
        "an encoded frame ends with a whole packet"
    );
    sequence_id
}

/// Errors returned when a packet limit or frame boundary is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketCodecError {
    /// A zero payload limit would reject every packet and is not useful.
    ZeroPayloadLimit,
    /// The configured limit cannot fit in MySQL's three-byte length field.
    PayloadLimitExceedsWireMaximum {
        /// Requested payload limit.
        limit: usize,
        /// Maximum representable wire payload.
        wire_maximum: usize,
    },
    /// The dependency's protocol maximum differs from this codec's maximum.
    DependencyWireMaximumMismatch {
        /// Maximum exported by `mysql_common`.
        dependency: usize,
        /// Maximum expected by this codec.
        codec: usize,
    },
    /// Fewer than four bytes were supplied for the packet header.
    TruncatedHeader {
        /// Number of bytes supplied.
        actual: usize,
    },
    /// The payload exceeds the configured allocation and parsing limit.
    PayloadTooLarge {
        /// Declared or supplied payload length.
        length: usize,
        /// Configured payload limit.
        limit: usize,
    },
    /// The header declares more payload than the frame contains.
    TruncatedPayload {
        /// Payload length declared by the header.
        declared: usize,
        /// Payload bytes supplied after the header.
        actual: usize,
    },
    /// The input contains bytes beyond the one packet boundary.
    TrailingBytes {
        /// Header plus declared payload length.
        expected: usize,
        /// Supplied frame length.
        actual: usize,
    },
}

impl fmt::Display for PacketCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPayloadLimit => f.write_str("packet payload limit must be non-zero"),
            Self::PayloadLimitExceedsWireMaximum {
                limit,
                wire_maximum,
            } => write!(
                f,
                "packet payload limit {limit} exceeds wire maximum {wire_maximum}"
            ),
            Self::DependencyWireMaximumMismatch { dependency, codec } => write!(
                f,
                "mysql_common wire maximum {dependency} does not match codec maximum {codec}"
            ),
            Self::TruncatedHeader { actual } => {
                write!(f, "packet header is truncated: got {actual} bytes")
            }
            Self::PayloadTooLarge { length, limit } => {
                write!(f, "packet payload length {length} exceeds limit {limit}")
            }
            Self::TruncatedPayload { declared, actual } => write!(
                f,
                "packet payload is truncated: declared {declared} bytes, got {actual}"
            ),
            Self::TrailingBytes { expected, actual } => write!(
                f,
                "packet frame has trailing bytes: expected {expected} bytes, got {actual}"
            ),
        }
    }
}

impl Error for PacketCodecError {}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: 64,
    };

    #[test]
    fn encodes_a_query_boundary_deterministically() {
        let frame = CODEC.encode(7, b"\x03SELECT 1").unwrap();

        assert_eq!(frame, b"\x09\x00\x00\x07\x03SELECT 1");
    }

    #[test]
    fn decodes_without_copying_payload() {
        let frame = b"\x03\x00\x00\x12abc";
        let packet = CODEC.decode(frame).unwrap();

        assert_eq!(packet.sequence_id, 0x12);
        assert_eq!(packet.payload, b"abc");
    }

    #[test]
    fn rejects_short_headers() {
        for length in 0..PACKET_HEADER_LEN {
            assert_eq!(
                CODEC.decode(&[0; PACKET_HEADER_LEN][..length]),
                Err(PacketCodecError::TruncatedHeader { actual: length })
            );
        }
    }

    #[test]
    fn rejects_truncated_payloads() {
        assert_eq!(
            CODEC.decode(b"\x04\x00\x00\x00abc"),
            Err(PacketCodecError::TruncatedPayload {
                declared: 4,
                actual: 3,
            })
        );
    }

    #[test]
    fn rejects_trailing_bytes() {
        assert_eq!(
            CODEC.decode(b"\x03\x00\x00\x00abcd"),
            Err(PacketCodecError::TrailingBytes {
                expected: 7,
                actual: 8,
            })
        );
    }

    #[test]
    fn rejects_oversized_declared_payload_before_slicing() {
        let codec = PacketCodec::new(2).unwrap();
        assert_eq!(
            codec.decode(b"\x03\x00\x00\x00abc"),
            Err(PacketCodecError::PayloadTooLarge {
                length: 3,
                limit: 2,
            })
        );
    }

    #[test]
    fn rejects_oversized_encode_payload() {
        let codec = PacketCodec::new(2).unwrap();
        assert_eq!(
            codec.encode(0, b"abc"),
            Err(PacketCodecError::PayloadTooLarge {
                length: 3,
                limit: 2,
            })
        );
    }

    #[test]
    fn validates_limits_at_construction() {
        assert_eq!(PacketCodec::new(0), Err(PacketCodecError::ZeroPayloadLimit));
        assert_eq!(
            PacketCodec::new(MAX_PACKET_PAYLOAD_LEN + 1),
            Err(PacketCodecError::PayloadLimitExceedsWireMaximum {
                limit: MAX_PACKET_PAYLOAD_LEN + 1,
                wire_maximum: MAX_PACKET_PAYLOAD_LEN,
            })
        );
        assert_eq!(mysql_common_max_payload_len(), MAX_PACKET_PAYLOAD_LEN);
    }

    #[test]
    fn splits_a_payload_into_full_packets_and_a_shorter_last_one() {
        let short = encode_split_payload(3, b"abc");
        assert_eq!(short, b"\x03\x00\x00\x03abc");
        assert_eq!(split_payload_packet_count(&short), Ok(1));

        let exact = vec![b'x'; MAX_PACKET_PAYLOAD_LEN];
        let frame = encode_split_payload(0, &exact);
        assert_eq!(frame.len(), 2 * PACKET_HEADER_LEN + MAX_PACKET_PAYLOAD_LEN);
        assert_eq!(&frame[..PACKET_HEADER_LEN], b"\xff\xff\xff\x00");
        assert_eq!(
            &frame[frame.len() - PACKET_HEADER_LEN..],
            b"\x00\x00\x00\x01"
        );
        assert_eq!(split_payload_packet_count(&frame), Ok(2));

        let longer = vec![b'y'; MAX_PACKET_PAYLOAD_LEN + 5];
        let mut frame = encode_split_payload(255, &longer);
        assert_eq!(frame[3], 255);
        assert_eq!(
            &frame[PACKET_HEADER_LEN + MAX_PACKET_PAYLOAD_LEN..][..PACKET_HEADER_LEN],
            b"\x05\x00\x00\x00"
        );
        assert_eq!(split_payload_packet_count(&frame), Ok(2));
        assert_eq!(renumber_packets(&mut frame, 7), 9);
        assert_eq!(frame[3], 7);
        assert_eq!(frame[PACKET_HEADER_LEN + MAX_PACKET_PAYLOAD_LEN + 3], 8);
    }

    #[test]
    fn a_split_payload_needs_its_last_packet() {
        let exact = vec![b'x'; MAX_PACKET_PAYLOAD_LEN];
        let frame = encode_split_payload(0, &exact);
        let without_last = &frame[..frame.len() - PACKET_HEADER_LEN];
        assert_eq!(
            split_payload_packet_count(without_last),
            Err(PacketCodecError::TruncatedHeader { actual: 0 })
        );
        let mut two = encode_split_payload(0, b"ab");
        two.extend_from_slice(&encode_split_payload(1, b"cd"));
        assert_eq!(
            split_payload_packet_count(&two),
            Err(PacketCodecError::TrailingBytes {
                expected: 6,
                actual: 12,
            })
        );
    }

    #[test]
    fn round_trips_every_sequence_id() {
        for sequence_id in u8::MIN..=u8::MAX {
            let frame = CODEC.encode(sequence_id, b"payload").unwrap();
            assert_eq!(CODEC.decode(&frame).unwrap().sequence_id, sequence_id);
        }
    }
}
