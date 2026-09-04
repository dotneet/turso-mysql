//! The bounded packet boundary immediately before a classic TLS upgrade.
//!
//! A TLS-capable MySQL client sends one ordinary SSLRequest packet and then
//! starts the TLS handshake on the same stream. The pre-TLS reader asks the
//! stream for only the four-byte header and the one declared payload, so a TLS
//! ClientHello coalesced by the kernel remains unread for rustls.

// This foundation has no live TCP caller until the listener transition slice.
#![allow(dead_code)]

use std::{error::Error, fmt, io, io::Read, net::TcpStream, time::Instant};

use crate::{
    ClientSslRequest, ClientSslRequestError, PacketCodec, CLIENT_HANDSHAKE_SEQUENCE_ID,
    CLIENT_SSL_REQUEST_PAYLOAD_LENGTH, PACKET_HEADER_LEN,
};

/// A stream operation that applies the supplied absolute deadline to each
/// bounded read.
pub(crate) trait DeadlinePacketReader {
    /// Reads at most `buffer.len()` bytes without changing the supplied
    /// absolute deadline.
    fn read_with_deadline(&mut self, buffer: &mut [u8], deadline: Instant) -> io::Result<usize>;
}

impl DeadlinePacketReader for TcpStream {
    fn read_with_deadline(&mut self, buffer: &mut [u8], deadline: Instant) -> io::Result<usize> {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|timeout| !timeout.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        self.set_read_timeout(Some(timeout))?;
        self.read(buffer)
    }
}

/// Reads exactly the first SSLRequest packet from a TCP-capable stream.
///
/// Only one packet is returned. Bytes after its declared payload are never
/// passed to this helper's buffers, so a coalesced TLS ClientHello remains in
/// the stream for the TLS layer. The same absolute deadline is used for every
/// header and payload read; partial progress cannot extend authentication.
pub(crate) fn read_ssl_request_packet<R: DeadlinePacketReader>(
    reader: &mut R,
    codec: PacketCodec,
    deadline: Instant,
) -> Result<ClientSslRequest, PreTlsPacketError> {
    let mut header = [0; 4];
    read_exact_with_deadline(reader, &mut header, deadline, ReadPart::Header)?;

    let payload_length =
        usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
    if payload_length > CLIENT_SSL_REQUEST_PAYLOAD_LENGTH {
        return Err(PreTlsPacketError::PayloadTooLarge {
            length: payload_length,
            limit: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
        });
    }
    if payload_length != CLIENT_SSL_REQUEST_PAYLOAD_LENGTH {
        return Err(PreTlsPacketError::InvalidPayloadLength {
            actual: payload_length,
            expected: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
        });
    }

    let sequence_id = header[3];
    if sequence_id != CLIENT_HANDSHAKE_SEQUENCE_ID {
        return Err(PreTlsPacketError::UnexpectedSequenceId {
            actual: sequence_id,
            expected: CLIENT_HANDSHAKE_SEQUENCE_ID,
        });
    }

    let mut payload = [0; CLIENT_SSL_REQUEST_PAYLOAD_LENGTH];
    read_exact_with_deadline(reader, &mut payload, deadline, ReadPart::Payload)?;
    let mut frame = [0; PACKET_HEADER_LEN + CLIENT_SSL_REQUEST_PAYLOAD_LENGTH];
    frame[..PACKET_HEADER_LEN].copy_from_slice(&header);
    frame[PACKET_HEADER_LEN..].copy_from_slice(&payload);
    codec
        .decode_client_ssl_request(&frame)
        .map_err(PreTlsPacketError::InvalidSslRequest)
}

fn read_exact_with_deadline<R: DeadlinePacketReader>(
    reader: &mut R,
    buffer: &mut [u8],
    deadline: Instant,
    part: ReadPart,
) -> Result<(), PreTlsPacketError> {
    let mut offset = 0;
    while offset < buffer.len() {
        if deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .is_none()
        {
            return Err(PreTlsPacketError::DeadlineExceeded);
        }
        match reader.read_with_deadline(&mut buffer[offset..], deadline) {
            Ok(0) => {
                return Err(match part {
                    ReadPart::Header => PreTlsPacketError::TruncatedHeader { actual: offset },
                    ReadPart::Payload => PreTlsPacketError::TruncatedPayload {
                        actual: offset,
                        expected: buffer.len(),
                    },
                });
            }
            Ok(read) if read <= buffer.len() - offset => offset += read,
            Ok(_) => return Err(PreTlsPacketError::ReadFailed),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(PreTlsPacketError::DeadlineExceeded)
            }
            Err(_) => return Err(PreTlsPacketError::ReadFailed),
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ReadPart {
    Header,
    Payload,
}

/// Failures while isolating the one pre-TLS classic packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreTlsPacketError {
    /// The absolute authentication deadline elapsed before a read completed.
    DeadlineExceeded,
    /// The stream returned a non-timeout read failure.
    ReadFailed,
    /// The stream ended before the four-byte packet header was complete.
    TruncatedHeader {
        /// Header bytes obtained before EOF.
        actual: usize,
    },
    /// The stream ended before the fixed SSLRequest payload was complete.
    TruncatedPayload {
        /// Payload bytes obtained before EOF.
        actual: usize,
        /// Required SSLRequest payload bytes.
        expected: usize,
    },
    /// The header declares a payload larger than an SSLRequest can contain.
    PayloadTooLarge {
        /// Payload bytes declared by the peer.
        length: usize,
        /// Maximum accepted SSLRequest payload bytes.
        limit: usize,
    },
    /// The packet is not the fixed-size SSLRequest shape.
    InvalidPayloadLength {
        /// Payload bytes declared by the peer.
        actual: usize,
        /// Required SSLRequest payload bytes.
        expected: usize,
    },
    /// The packet is not the first client handshake packet.
    UnexpectedSequenceId {
        /// Sequence number received in the packet header.
        actual: u8,
        /// Sequence number required for an SSLRequest.
        expected: u8,
    },
    /// The fixed packet shape was present but its SSLRequest fields were invalid.
    InvalidSslRequest(ClientSslRequestError),
}

impl fmt::Display for PreTlsPacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeadlineExceeded => f.write_str("pre-TLS packet deadline elapsed"),
            Self::ReadFailed => f.write_str("pre-TLS packet read failed"),
            Self::TruncatedHeader { actual } => {
                write!(f, "pre-TLS packet header truncated after {actual} bytes")
            }
            Self::TruncatedPayload { actual, expected } => write!(
                f,
                "pre-TLS packet payload truncated after {actual} of {expected} bytes"
            ),
            Self::PayloadTooLarge { length, limit } => write!(
                f,
                "pre-TLS packet payload length {length} exceeds limit {limit}"
            ),
            Self::InvalidPayloadLength { actual, expected } => write!(
                f,
                "pre-TLS packet payload length {actual}, expected {expected}"
            ),
            Self::UnexpectedSequenceId { actual, expected } => {
                write!(f, "pre-TLS packet sequence {actual}, expected {expected}")
            }
            Self::InvalidSslRequest(error) => write!(f, "invalid SSLRequest: {error}"),
        }
    }
}

impl Error for PreTlsPacketError {}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io,
        time::{Duration, Instant},
    };

    use super::{read_ssl_request_packet, DeadlinePacketReader, PreTlsPacketError};
    use crate::{
        ClientSslRequestConfig, ClientSslRequestError, PacketCodec, CLIENT_HANDSHAKE_SEQUENCE_ID,
        CLIENT_PLUGIN_AUTH, CLIENT_SSL, CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
        DEFAULT_UTF8MB4_COLLATION, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
        MIN_SERVER_RESPONSE_PAYLOAD_LENGTH, REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };

    struct ScriptedReader {
        bytes: VecDeque<u8>,
        chunk_limit: usize,
        deadlines: Vec<Instant>,
        timeout_after: Option<usize>,
        interrupts: usize,
        read_error: Option<io::ErrorKind>,
        reads: usize,
    }

    impl ScriptedReader {
        fn new(bytes: &[u8], chunk_limit: usize) -> Self {
            Self {
                bytes: bytes.iter().copied().collect(),
                chunk_limit,
                deadlines: Vec::new(),
                timeout_after: None,
                interrupts: 0,
                read_error: None,
                reads: 0,
            }
        }

        fn remaining(&self) -> Vec<u8> {
            self.bytes.iter().copied().collect()
        }
    }

    impl DeadlinePacketReader for ScriptedReader {
        fn read_with_deadline(
            &mut self,
            buffer: &mut [u8],
            deadline: Instant,
        ) -> io::Result<usize> {
            self.deadlines.push(deadline);
            if self.interrupts > 0 {
                self.interrupts -= 1;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            if self.timeout_after == Some(self.reads) {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            if let Some(kind) = self.read_error {
                return Err(io::Error::from(kind));
            }
            self.reads += 1;
            let count = self.chunk_limit.min(buffer.len()).min(self.bytes.len());
            for slot in &mut buffer[..count] {
                *slot = self.bytes.pop_front().expect("count fits the queue");
            }
            Ok(count)
        }
    }

    fn test_codec() -> PacketCodec {
        PacketCodec::new(MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH).expect("test codec")
    }

    fn ssl_request_header(sequence_id: u8, payload_length: usize) -> Vec<u8> {
        vec![
            payload_length as u8,
            (payload_length >> 8) as u8,
            (payload_length >> 16) as u8,
            sequence_id,
        ]
    }

    fn valid_ssl_request_frame(sequence_id: u8) -> Vec<u8> {
        ClientSslRequestConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
            DEFAULT_UTF8MB4_COLLATION,
        )
        .encode(test_codec(), sequence_id)
        .expect("valid SSLRequest")
    }

    #[test]
    fn split_header_and_body_reads_exactly_one_packet() {
        let mut frame = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        let tls_client_hello = b"tls-client-hello";
        frame.extend_from_slice(tls_client_hello);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reader = ScriptedReader::new(&frame, 1);

        let packet =
            read_ssl_request_packet(&mut reader, test_codec(), deadline).expect("SSLRequest");

        assert_eq!(packet.sequence_id, CLIENT_HANDSHAKE_SEQUENCE_ID);
        assert_eq!(
            packet.capability_flags,
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL
        );
        assert_eq!(reader.remaining(), tls_client_hello);
        assert!(reader.deadlines.iter().all(|seen| *seen == deadline));
    }

    #[test]
    fn coalesced_tls_bytes_are_left_for_rustls() {
        let mut frame = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        frame.extend_from_slice(b"client-hello");
        let mut reader = ScriptedReader::new(&frame, frame.len());

        read_ssl_request_packet(
            &mut reader,
            test_codec(),
            Instant::now() + Duration::from_secs(5),
        )
        .expect("SSLRequest");

        assert_eq!(reader.remaining(), b"client-hello");
    }

    #[test]
    fn oversized_header_is_rejected_without_reading_payload() {
        let mut frame = ssl_request_header(
            CLIENT_HANDSHAKE_SEQUENCE_ID,
            CLIENT_SSL_REQUEST_PAYLOAD_LENGTH + 1,
        );
        frame.extend([0x42; CLIENT_SSL_REQUEST_PAYLOAD_LENGTH + 1]);
        let mut reader = ScriptedReader::new(&frame, frame.len());

        assert_eq!(
            read_ssl_request_packet(
                &mut reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::PayloadTooLarge {
                length: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH + 1,
                limit: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
            })
        );
        assert_eq!(reader.remaining(), &frame[4..]);
    }

    #[test]
    fn truncated_header_and_payload_are_distinguished() {
        let mut header_only = ScriptedReader::new(
            &ssl_request_header(CLIENT_HANDSHAKE_SEQUENCE_ID, 0)[..2],
            usize::MAX,
        );
        assert_eq!(
            read_ssl_request_packet(
                &mut header_only,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::TruncatedHeader { actual: 2 })
        );

        let mut truncated = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        truncated.truncate(4 + CLIENT_SSL_REQUEST_PAYLOAD_LENGTH - 1);
        let mut payload_reader = ScriptedReader::new(&truncated, usize::MAX);
        assert_eq!(
            read_ssl_request_packet(
                &mut payload_reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::TruncatedPayload {
                actual: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH - 1,
                expected: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
            })
        );
    }

    #[test]
    fn wrong_sequence_fails_before_body_reads() {
        let mut wrong_sequence = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        wrong_sequence[3] = CLIENT_HANDSHAKE_SEQUENCE_ID.wrapping_add(1);
        let mut sequence_reader = ScriptedReader::new(&wrong_sequence, wrong_sequence.len());
        assert_eq!(
            read_ssl_request_packet(
                &mut sequence_reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::UnexpectedSequenceId {
                actual: CLIENT_HANDSHAKE_SEQUENCE_ID.wrapping_add(1),
                expected: CLIENT_HANDSHAKE_SEQUENCE_ID,
            })
        );
        assert_eq!(sequence_reader.remaining(), &wrong_sequence[4..]);
    }

    #[test]
    fn short_payload_fails_before_body_reads() {
        let short_payload = ssl_request_header(
            CLIENT_HANDSHAKE_SEQUENCE_ID,
            CLIENT_SSL_REQUEST_PAYLOAD_LENGTH - 1,
        );
        let mut short_reader = ScriptedReader::new(&short_payload, short_payload.len());
        assert_eq!(
            read_ssl_request_packet(
                &mut short_reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::InvalidPayloadLength {
                actual: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH - 1,
                expected: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
            })
        );
    }

    #[test]
    fn timeout_after_partial_progress_keeps_the_original_deadline() {
        let frame = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reader = ScriptedReader::new(&frame, 2);
        reader.timeout_after = Some(2);

        assert_eq!(
            read_ssl_request_packet(&mut reader, test_codec(), deadline),
            Err(PreTlsPacketError::DeadlineExceeded)
        );
        assert!(reader.deadlines.iter().all(|seen| *seen == deadline));
    }

    #[test]
    fn expired_deadline_fails_before_reader_operation() {
        let mut reader = ScriptedReader::new(&[], 1);
        assert_eq!(
            read_ssl_request_packet(
                &mut reader,
                test_codec(),
                Instant::now() - Duration::from_secs(1)
            ),
            Err(PreTlsPacketError::DeadlineExceeded)
        );
        assert!(reader.deadlines.is_empty());
    }

    #[test]
    fn invalid_capabilities_are_rejected_before_tls() {
        let mut missing_ssl = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        missing_ssl[4..8]
            .copy_from_slice(&REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES.to_le_bytes());
        let mut missing_ssl_reader = ScriptedReader::new(&missing_ssl, missing_ssl.len());
        assert_eq!(
            read_ssl_request_packet(
                &mut missing_ssl_reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::InvalidSslRequest(
                ClientSslRequestError::MissingSslCapability {
                    flags: REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
                }
            ))
        );

        let mut missing_required = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        let flags =
            (REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL) & !CLIENT_PLUGIN_AUTH;
        missing_required[4..8].copy_from_slice(&flags.to_le_bytes());
        let mut missing_required_reader =
            ScriptedReader::new(&missing_required, missing_required.len());
        assert_eq!(
            read_ssl_request_packet(
                &mut missing_required_reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::InvalidSslRequest(
                ClientSslRequestError::MissingCapabilities {
                    flags,
                    missing: CLIENT_PLUGIN_AUTH,
                }
            ))
        );
    }

    #[test]
    fn invalid_max_packet_size_and_charset_are_rejected_before_tls() {
        let mut too_small = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        too_small[8..12].copy_from_slice(&(MIN_SERVER_RESPONSE_PAYLOAD_LENGTH - 1).to_le_bytes());
        let mut too_small_reader = ScriptedReader::new(&too_small, too_small.len());
        assert_eq!(
            read_ssl_request_packet(
                &mut too_small_reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::InvalidSslRequest(
                ClientSslRequestError::MaxPacketSizeTooSmall {
                    max_packet_size: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH - 1,
                    minimum: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
                }
            ))
        );

        let mut unsupported_charset = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        unsupported_charset[12] = 33;
        let mut charset_reader =
            ScriptedReader::new(&unsupported_charset, unsupported_charset.len());
        assert_eq!(
            read_ssl_request_packet(
                &mut charset_reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::InvalidSslRequest(
                ClientSslRequestError::UnsupportedCharacterSet { character_set: 33 }
            ))
        );
    }

    #[test]
    fn nonzero_reserved_bytes_are_rejected_before_tls() {
        let mut frame = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        frame[13] = 1;
        let mut reader = ScriptedReader::new(&frame, frame.len());

        assert_eq!(
            read_ssl_request_packet(
                &mut reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::InvalidSslRequest(
                ClientSslRequestError::NonZeroReservedBytes
            ))
        );
    }

    #[test]
    fn interrupted_reads_retry_without_changing_deadline() {
        let frame = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reader = ScriptedReader::new(&frame, frame.len());
        reader.interrupts = 1;

        read_ssl_request_packet(&mut reader, test_codec(), deadline).expect("SSLRequest");

        assert!(reader.deadlines.iter().all(|seen| *seen == deadline));
    }

    #[test]
    fn generic_read_failures_are_coarsened() {
        let frame = valid_ssl_request_frame(CLIENT_HANDSHAKE_SEQUENCE_ID);
        let mut reader = ScriptedReader::new(&frame, frame.len());
        reader.read_error = Some(io::ErrorKind::ConnectionReset);

        assert_eq!(
            read_ssl_request_packet(
                &mut reader,
                test_codec(),
                Instant::now() + Duration::from_secs(5)
            ),
            Err(PreTlsPacketError::ReadFailed)
        );
    }
}
