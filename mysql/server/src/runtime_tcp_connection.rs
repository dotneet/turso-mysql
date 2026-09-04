//! The bounded packet boundary immediately before a classic TLS upgrade.
//!
//! A TLS-capable MySQL client sends one ordinary SSLRequest packet and then
//! starts the TLS handshake on the same stream. The pre-TLS reader asks the
//! stream for only the four-byte header and the one declared payload, so a TLS
//! ClientHello coalesced by the kernel remains unread for rustls.

// This foundation has no live TCP caller until the listener transition slice.
#![allow(dead_code)]

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    net::TcpStream,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    AuthenticatedExecutorFactory, ClassicConnectionOrchestrator, ClassicFrame, ClientSslRequest,
    ClientSslRequestError, CredentialProvider, InitialHandshakeSettings, OrchestratorError,
    OrchestratorEvent, PacketCodec, PacketCodecError, TlsServerConfig,
    CLIENT_HANDSHAKE_SEQUENCE_ID, CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
    MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
    PACKET_HEADER_LEN,
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

fn read_classic_frame<R: DeadlinePacketReader>(
    reader: &mut R,
    codec: PacketCodec,
    deadline: Instant,
) -> Result<ClassicFrame, PreTlsPacketError> {
    let mut header = [0; PACKET_HEADER_LEN];
    read_exact_with_deadline(reader, &mut header, deadline, ReadPart::Header)?;

    let payload_length =
        usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
    if payload_length > codec.max_payload_len() {
        return Err(PreTlsPacketError::PayloadTooLarge {
            length: payload_length,
            limit: codec.max_payload_len(),
        });
    }

    let mut payload = vec![0; payload_length];
    read_exact_with_deadline(reader, &mut payload, deadline, ReadPart::Payload)?;
    let mut frame = Vec::with_capacity(PACKET_HEADER_LEN + payload_length);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&payload);
    ClassicFrame::new(codec, frame).map_err(PreTlsPacketError::InvalidPacket)
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
    /// The header declares a payload larger than the configured read bound.
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
    /// The complete frame did not satisfy the packet codec.
    InvalidPacket(PacketCodecError),
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
            Self::InvalidPacket(error) => write!(f, "invalid classic packet: {error}"),
        }
    }
}

impl Error for PreTlsPacketError {}

type RustlsServerConnection = rustls::ServerConnection;

struct TlsTransport {
    connection: RustlsServerConnection,
    stream: TcpStream,
}

impl TlsTransport {
    fn new(connection: RustlsServerConnection, stream: TcpStream) -> Self {
        Self { connection, stream }
    }

    fn complete_handshake(&mut self, deadline: Instant) -> Result<(), RuntimeTcpConnectionError> {
        while self.connection.is_handshaking() || self.connection.wants_write() {
            if self.connection.wants_write() {
                self.write_tls(deadline).map_err(map_tls_handshake_error)?;
            }
            if self.connection.is_handshaking() && self.connection.wants_read() {
                self.read_tls(deadline).map_err(map_tls_handshake_error)?;
            } else if self.connection.is_handshaking() && !self.connection.wants_write() {
                return Err(RuntimeTcpConnectionError::TlsHandshakeFailed);
            }
        }
        Ok(())
    }

    fn write_plain(
        &mut self,
        buffer: &[u8],
        deadline: Instant,
    ) -> Result<usize, RuntimeTcpConnectionError> {
        let written = self
            .connection
            .writer()
            .write(buffer)
            .map_err(map_tls_write_error)?;
        if written == 0 && !buffer.is_empty() {
            return Err(RuntimeTcpConnectionError::TlsWriteFailed);
        }
        self.write_tls(deadline).map_err(map_tls_write_error)?;
        Ok(written)
    }

    fn read_tls(&mut self, deadline: Instant) -> io::Result<usize> {
        set_read_timeout(&self.stream, deadline)?;
        let read = loop {
            match self.connection.read_tls(&mut self.stream) {
                Ok(read) => break read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        };
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        self.connection
            .process_new_packets()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(read)
    }

    fn write_tls(&mut self, deadline: Instant) -> io::Result<()> {
        while self.connection.wants_write() {
            set_write_timeout(&self.stream, deadline)?;
            let written = loop {
                match self.connection.write_tls(&mut self.stream) {
                    Ok(written) => break written,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            };
            if written == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
        }
        Ok(())
    }
}

impl DeadlinePacketReader for TlsTransport {
    fn read_with_deadline(&mut self, buffer: &mut [u8], deadline: Instant) -> io::Result<usize> {
        loop {
            match self.connection.reader().read(buffer) {
                Ok(read) if read > 0 || buffer.is_empty() => return Ok(read),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }

            if self.connection.wants_write() {
                self.write_tls(deadline)?;
            }
            if !self.connection.wants_read() {
                return Ok(0);
            }
            self.read_tls(deadline)?;
        }
    }
}

enum TcpTransport {
    Plain(TcpStream),
    Tls(Box<TlsTransport>),
}

/// Bounded response-queue limits for one runtime-owned TCP connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeTcpConnectionLimits {
    /// Maximum total queued response bytes.
    pub(crate) max_queued_bytes: usize,
    /// Maximum queued response frames.
    pub(crate) max_queued_frames: usize,
}

/// Owns one TCP stream through the TLS and initial-authentication transition.
///
/// This foundation intentionally stops after the first post-TLS handshake
/// response. A later runtime slice can continue from the returned
/// [`OrchestratorEvent::Ready`] state and add the command loop.
pub(crate) struct RuntimeTcpConnection<P, F>
where
    P: CredentialProvider,
    F: AuthenticatedExecutorFactory,
{
    transport: Option<TcpTransport>,
    tls_config: Arc<rustls::ServerConfig>,
    orchestrator: ClassicConnectionOrchestrator<P, F>,
    codec: PacketCodec,
    authentication_deadline: Instant,
    started: bool,
}

impl<P, F> RuntimeTcpConnection<P, F>
where
    P: CredentialProvider,
    F: AuthenticatedExecutorFactory,
{
    /// Creates a TLS-required connection owner without binding or accepting.
    pub(crate) fn new(
        stream: TcpStream,
        settings: InitialHandshakeSettings,
        tls_config: &TlsServerConfig,
        verifier: crate::CachingSha2Verifier<P>,
        executor_factory: F,
        authentication_deadline: Instant,
        limits: RuntimeTcpConnectionLimits,
    ) -> Result<Self, RuntimeTcpConnectionError> {
        let codec = PacketCodec::new(
            MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH.max(MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH),
        )
        .map_err(RuntimeTcpConnectionError::PacketCodec)?;
        let orchestrator = ClassicConnectionOrchestrator::new(
            settings,
            verifier,
            executor_factory,
            limits.max_queued_bytes,
            limits.max_queued_frames,
        )
        .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        Ok(Self {
            transport: Some(TcpTransport::Plain(stream)),
            tls_config: tls_config.server_config(),
            orchestrator,
            codec,
            authentication_deadline,
            started: false,
        })
    }

    /// Drives greeting, SSLRequest, TLS, and the first post-TLS response.
    pub(crate) fn drive_tls_transition(
        &mut self,
    ) -> Result<OrchestratorEvent, RuntimeTcpConnectionError> {
        if self.started {
            return Err(RuntimeTcpConnectionError::AlreadyStarted);
        }
        self.started = true;

        let event = self
            .orchestrator
            .start()
            .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        if event != OrchestratorEvent::AwaitingClientFrame {
            return Err(RuntimeTcpConnectionError::UnexpectedEvent(event));
        }
        self.flush_plain_writes()?;

        let request = match self.transport.as_mut() {
            Some(TcpTransport::Plain(stream)) => {
                read_ssl_request_packet(stream, self.codec, self.authentication_deadline)
                    .map_err(RuntimeTcpConnectionError::Packet)?
            }
            Some(TcpTransport::Tls(_)) | None => {
                return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
            }
        };
        let event = self
            .orchestrator
            .receive_ssl_request(request)
            .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        if event != OrchestratorEvent::TlsUpgradeRequired {
            return Err(RuntimeTcpConnectionError::UnexpectedEvent(event));
        }

        let stream = match self.transport.take() {
            Some(TcpTransport::Plain(stream)) => stream,
            Some(TcpTransport::Tls(_)) | None => {
                return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
            }
        };
        let connection = RustlsServerConnection::new(Arc::clone(&self.tls_config))
            .map_err(|_| RuntimeTcpConnectionError::TlsConfiguration)?;
        let mut tls = TlsTransport::new(connection, stream);
        tls.complete_handshake(self.authentication_deadline)?;
        self.transport = Some(TcpTransport::Tls(Box::new(tls)));

        self.orchestrator
            .tls_negotiated()
            .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        let frame = match self.transport.as_mut() {
            Some(TcpTransport::Tls(tls)) => {
                read_classic_frame(tls.as_mut(), self.codec, self.authentication_deadline)
                    .map_err(RuntimeTcpConnectionError::Packet)?
            }
            Some(TcpTransport::Plain(_)) | None => {
                return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
            }
        };
        let event = self
            .orchestrator
            .receive_frame(frame)
            .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        self.flush_tls_writes()?;
        Ok(event)
    }

    fn flush_plain_writes(&mut self) -> Result<(), RuntimeTcpConnectionError> {
        loop {
            let Some(frame) = self.orchestrator.front_write() else {
                return Ok(());
            };
            let written = match self.transport.as_mut() {
                Some(TcpTransport::Plain(stream)) => {
                    write_with_deadline(stream, frame, self.authentication_deadline)?
                }
                Some(TcpTransport::Tls(_)) | None => {
                    return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
                }
            };
            self.orchestrator
                .advance_write(written)
                .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        }
    }

    fn flush_tls_writes(&mut self) -> Result<(), RuntimeTcpConnectionError> {
        loop {
            let Some(frame) = self.orchestrator.front_write() else {
                return Ok(());
            };
            let written = match self.transport.as_mut() {
                Some(TcpTransport::Tls(tls)) => {
                    tls.write_plain(frame, self.authentication_deadline)?
                }
                Some(TcpTransport::Plain(_)) | None => {
                    return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
                }
            };
            self.orchestrator
                .advance_write(written)
                .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        }
    }
}

fn write_with_deadline(
    stream: &mut TcpStream,
    buffer: &[u8],
    deadline: Instant,
) -> Result<usize, RuntimeTcpConnectionError> {
    set_write_timeout(stream, deadline).map_err(map_plain_write_error)?;
    loop {
        match stream.write(buffer) {
            Ok(written) => {
                if written == 0 && !buffer.is_empty() {
                    return Err(RuntimeTcpConnectionError::WriteFailed);
                }
                return Ok(written);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(map_plain_write_error(error)),
        }
    }
}

fn set_read_timeout(stream: &TcpStream, deadline: Instant) -> io::Result<()> {
    stream.set_read_timeout(Some(remaining_timeout(deadline)?))
}

fn set_write_timeout(stream: &TcpStream, deadline: Instant) -> io::Result<()> {
    stream.set_write_timeout(Some(remaining_timeout(deadline)?))
}

fn remaining_timeout(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
}

fn map_plain_write_error(error: io::Error) -> RuntimeTcpConnectionError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        RuntimeTcpConnectionError::DeadlineExceeded
    } else {
        RuntimeTcpConnectionError::WriteFailed
    }
}

fn map_tls_handshake_error(error: io::Error) -> RuntimeTcpConnectionError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        RuntimeTcpConnectionError::DeadlineExceeded
    } else if error.kind() == io::ErrorKind::UnexpectedEof {
        RuntimeTcpConnectionError::TlsPeerClosed
    } else if error.kind() == io::ErrorKind::InvalidData {
        RuntimeTcpConnectionError::TlsHandshakeFailed
    } else {
        RuntimeTcpConnectionError::TlsReadFailed
    }
}

fn map_tls_write_error(error: io::Error) -> RuntimeTcpConnectionError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        RuntimeTcpConnectionError::DeadlineExceeded
    } else {
        RuntimeTcpConnectionError::TlsWriteFailed
    }
}

/// Redacted failures from the TCP/TLS protocol owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeTcpConnectionError {
    /// The one-shot transition driver was called more than once.
    AlreadyStarted,
    /// The packet codec could not be constructed or validated.
    PacketCodec(PacketCodecError),
    /// A bounded classic packet could not be read or validated.
    Packet(PreTlsPacketError),
    /// The protocol orchestrator rejected an owner action.
    Orchestrator(OrchestratorError),
    /// The absolute authentication deadline elapsed.
    DeadlineExceeded,
    /// A plaintext socket write failed.
    WriteFailed,
    /// Rustls rejected or could not process an encrypted read.
    TlsReadFailed,
    /// A TLS record write failed.
    TlsWriteFailed,
    /// The peer closed the TCP stream during TLS negotiation.
    TlsPeerClosed,
    /// Rustls could not be configured for this connection.
    TlsConfiguration,
    /// The peer's TLS handshake was invalid.
    TlsHandshakeFailed,
    /// The owner transport variant did not match the protocol phase.
    UnexpectedTransportState,
    /// The orchestrator returned an event that does not fit this phase.
    UnexpectedEvent(OrchestratorEvent),
}

impl fmt::Display for RuntimeTcpConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyStarted => f.write_str("TCP TLS transition already started"),
            Self::PacketCodec(error) => write!(f, "TCP packet codec failed: {error}"),
            Self::Packet(error) => write!(f, "TCP packet boundary failed: {error}"),
            Self::Orchestrator(error) => write!(f, "TCP protocol orchestration failed: {error}"),
            Self::DeadlineExceeded => f.write_str("TCP authentication deadline elapsed"),
            Self::WriteFailed => f.write_str("TCP plaintext write failed"),
            Self::TlsReadFailed => f.write_str("TLS read failed"),
            Self::TlsWriteFailed => f.write_str("TLS write failed"),
            Self::TlsPeerClosed => f.write_str("TLS peer closed the connection"),
            Self::TlsConfiguration => f.write_str("TLS configuration failed"),
            Self::TlsHandshakeFailed => f.write_str("TLS handshake failed"),
            Self::UnexpectedTransportState => f.write_str("TCP transport state was unexpected"),
            Self::UnexpectedEvent(event) => {
                write!(f, "TCP protocol event was unexpected: {event:?}")
            }
        }
    }
}

impl Error for RuntimeTcpConnectionError {}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::{self, Read, Write},
        net::{TcpListener, TcpStream},
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    use rustls::pki_types::{ServerName, UnixTime};
    use sha2::{Digest, Sha256};

    use super::{
        read_ssl_request_packet, DeadlinePacketReader, PreTlsPacketError, RuntimeTcpConnection,
        RuntimeTcpConnectionError, RuntimeTcpConnectionLimits,
    };
    use crate::{
        AuthenticatedCommandExecutor, AuthenticatedExecutorFactory, AuthorizationError,
        ClientHandshakeResponseConfig, ClientSslRequestConfig, ClientSslRequestError,
        CommandExecutionResult, CommandExecutor, CommandOkResult, FrontendErrorKind,
        InMemoryCredentialProvider, InitialDatabaseSelector, PacketCodec, StoredCredential,
        AUTH_PLUGIN_DATA_LENGTH, CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_HANDSHAKE_SEQUENCE_ID,
        CLIENT_PLUGIN_AUTH, CLIENT_SSL, CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
        DEFAULT_UTF8MB4_COLLATION, FAST_AUTH_RESPONSE_LENGTH, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
        MIN_SERVER_RESPONSE_PAYLOAD_LENGTH, REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        TLS_CLIENT_HANDSHAKE_SEQUENCE_ID,
    };

    #[derive(Debug, Default)]
    struct TestExecutor;

    impl CommandExecutor for TestExecutor {
        fn execute_init_db(
            &mut self,
            _database: &str,
        ) -> Result<CommandExecutionResult, FrontendErrorKind> {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }

        fn execute_query(
            &mut self,
            _sql: &str,
        ) -> Result<CommandExecutionResult, FrontendErrorKind> {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }
    }

    impl InitialDatabaseSelector for TestExecutor {
        fn select_initial_database(&mut self, _database: &str) -> Result<(), FrontendErrorKind> {
            Ok(())
        }
    }

    impl AuthenticatedCommandExecutor for TestExecutor {
        fn authorize_connection(&mut self) -> Result<(), AuthorizationError> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct TestExecutorFactory;

    impl AuthenticatedExecutorFactory for TestExecutorFactory {
        type Executor = TestExecutor;

        fn build(
            self,
            _principal: crate::AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            Ok(TestExecutor)
        }
    }

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

    fn verifier_material(password: &[u8]) -> [u8; 32] {
        let first = Sha256::digest(password);
        let second = Sha256::digest(first);
        second.into()
    }

    fn fast_response(
        password: &[u8],
        scramble: &[u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> [u8; FAST_AUTH_RESPONSE_LENGTH] {
        let first = Sha256::digest(password);
        let second = Sha256::digest(first);
        let third = Sha256::digest(second);
        let mut challenge = Vec::with_capacity(third.len() + scramble.len());
        challenge.extend_from_slice(&third);
        challenge.extend_from_slice(scramble);
        let mask = Sha256::digest(challenge);
        let mut response = [0; FAST_AUTH_RESPONSE_LENGTH];
        for (out, (&password_hash, &mask_byte)) in
            response.iter_mut().zip(first.iter().zip(mask.iter()))
        {
            *out = password_hash ^ mask_byte;
        }
        response
    }

    fn read_raw_frame(stream: &mut impl Read) -> io::Result<Vec<u8>> {
        let mut header = [0; 4];
        stream.read_exact(&mut header)?;
        let payload_length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let mut frame = Vec::with_capacity(4 + payload_length);
        frame.extend_from_slice(&header);
        frame.resize(4 + payload_length, 0);
        stream.read_exact(&mut frame[4..])?;
        Ok(frame)
    }

    #[derive(Debug)]
    struct AcceptAnyServer;

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![rustls::SignatureScheme::ECDSA_NISTP256_SHA256]
        }
    }

    fn client_config() -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .expect("client TLS versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
            .with_no_client_auth();
        Arc::new(config)
    }

    #[test]
    fn drives_tls_handshake_and_initial_auth_over_connected_sockets() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let address = listener.local_addr().expect("test listener address");
        let client = TcpStream::connect(address).expect("test client connection");
        let (server, _) = listener.accept().expect("test server connection");
        let password = b"secret";
        let tls_config = crate::runtime_tls::test_server_config();
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .expect("test credential");
        let settings = crate::InitialHandshakeSettings {
            capability_flags: REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            ..crate::InitialHandshakeSettings::default()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let server_thread = thread::spawn(move || {
            let mut owner = RuntimeTcpConnection::new(
                server,
                settings,
                &tls_config,
                crate::CachingSha2Verifier::new(provider),
                TestExecutorFactory,
                deadline,
                RuntimeTcpConnectionLimits {
                    max_queued_bytes: 16 * 1024,
                    max_queued_frames: 8,
                },
            )
            .expect("runtime TCP owner");
            owner.drive_tls_transition()
        });

        let mut client = client;
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        client
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("client write timeout");
        let codec = test_codec();
        let greeting_frame = read_raw_frame(&mut client).expect("greeting");
        let greeting = codec
            .decode_initial_handshake(&greeting_frame)
            .expect("decoded greeting");
        assert_eq!(greeting.sequence_id, 0);
        assert_ne!(greeting.capability_flags() & CLIENT_SSL, 0);
        let ssl_request = ClientSslRequestConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
            DEFAULT_UTF8MB4_COLLATION,
        )
        .encode(codec, CLIENT_HANDSHAKE_SEQUENCE_ID)
        .expect("SSLRequest");
        client.write_all(&ssl_request).expect("SSLRequest write");

        let server_name = ServerName::try_from("localhost").expect("server name");
        let mut tls_client = rustls::ClientConnection::new(client_config(), server_name)
            .expect("client TLS connection");
        let mut client_stream = client;
        while tls_client.is_handshaking() {
            tls_client
                .complete_io(&mut client_stream)
                .expect("TLS handshake");
        }
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            fast_response(password, &greeting.auth_plugin_data).to_vec(),
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(codec, TLS_CLIENT_HANDSHAKE_SEQUENCE_ID)
        .expect("post-TLS handshake response");
        let mut tls_client = rustls::StreamOwned::new(tls_client, client_stream);
        tls_client
            .write_all(&response)
            .expect("handshake response write");
        tls_client.flush().expect("handshake response flush");

        let event = server_thread.join().expect("server thread");
        assert_eq!(event, Ok(crate::OrchestratorEvent::Ready));
        let auth_more_data = read_raw_frame(&mut tls_client).expect("AuthMoreData");
        let auth_ok = read_raw_frame(&mut tls_client).expect("authentication OK");
        assert_eq!(
            auth_more_data[3],
            TLS_CLIENT_HANDSHAKE_SEQUENCE_ID.wrapping_add(1)
        );
        assert_eq!(auth_ok[3], TLS_CLIENT_HANDSHAKE_SEQUENCE_ID.wrapping_add(2));
    }

    #[test]
    fn owner_does_not_allow_a_plaintext_start_without_tls_advertisement() {
        let stream = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let address = stream.local_addr().expect("test listener address");
        let client = TcpStream::connect(address).expect("test client connection");
        let (server, _) = stream.accept().expect("test server connection");
        let tls_config = crate::runtime_tls::test_server_config();
        let provider = InMemoryCredentialProvider::new();
        let result = RuntimeTcpConnection::new(
            server,
            crate::InitialHandshakeSettings {
                capability_flags: REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
                ..crate::InitialHandshakeSettings::default()
            },
            &tls_config,
            crate::CachingSha2Verifier::new(provider),
            TestExecutorFactory,
            Instant::now() + Duration::from_secs(5),
            RuntimeTcpConnectionLimits {
                max_queued_bytes: 1024,
                max_queued_frames: 2,
            },
        );
        drop(client);
        assert!(matches!(
            result,
            Err(RuntimeTcpConnectionError::Orchestrator(
                crate::OrchestratorError::TlsCapabilityRequired
            ))
        ));
    }
}
