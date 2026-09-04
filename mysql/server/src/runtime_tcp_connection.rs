//! Blocking protocol ownership for one mandatory-TLS TCP connection.
//!
//! A TLS-capable MySQL client sends one ordinary SSLRequest packet and then
//! starts the TLS handshake on the same stream. The pre-TLS reader asks the
//! stream for only the four-byte header and the one declared payload, so a TLS
//! ClientHello coalesced by the kernel remains unread for rustls.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use turso_mysql::schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext};

use crate::runtime_tcp_listener::{AcceptedTcpStream, RuntimeTcpListenerError};
use crate::{
    AuthorizedDatabaseAdapterFactory, CachingSha2Verifier, ClassicConnectionOrchestrator,
    ClassicFrame, ClientSslRequest, ClientSslRequestError, InitialHandshakeSettings,
    OrchestratorError, OrchestratorEvent, PacketCodec, PacketCodecError,
    CLIENT_HANDSHAKE_SEQUENCE_ID, CLIENT_SSL, CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
    MAX_COMMAND_PAYLOAD_LENGTH, PACKET_HEADER_LEN,
    SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
};

/// A stream operation that applies the supplied absolute deadline to each
/// bounded read.
pub(crate) trait DeadlinePacketReader {
    /// Reads at most `buffer.len()` bytes without changing the supplied
    /// absolute deadline.
    fn read_with_deadline(&mut self, buffer: &mut [u8], deadline: Instant) -> io::Result<usize>;
}

impl DeadlinePacketReader for AcceptedTcpStream {
    fn read_with_deadline(&mut self, buffer: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.set_read_timeout(remaining_timeout(deadline)?)?;
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
pub enum PreTlsPacketError {
    /// The caller's absolute phase deadline elapsed before a read completed.
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
    stream: AcceptedTcpStream,
    write_timeout: Duration,
    read_control_write_error: Option<RuntimeTcpConnectionError>,
}

impl TlsTransport {
    fn new(
        connection: RustlsServerConnection,
        stream: AcceptedTcpStream,
        write_timeout: Duration,
    ) -> Self {
        Self {
            connection,
            stream,
            write_timeout,
            read_control_write_error: None,
        }
    }

    fn complete_handshake(&mut self, deadline: Instant) -> Result<(), RuntimeTcpConnectionError> {
        while self.connection.is_handshaking() || self.connection.wants_write() {
            if self.connection.wants_write() {
                self.write_tls(bounded_write_deadline(deadline, self.write_timeout))
                    .map_err(map_tls_write_error)?;
            }
            if self.connection.is_handshaking() && self.connection.wants_read() {
                self.read_tls(deadline).map_err(map_tls_handshake_error)?;
            } else if self.connection.is_handshaking() && !self.connection.wants_write() {
                return Err(RuntimeTcpConnectionError::TlsHandshakeFailed);
            }
        }
        Ok(())
    }

    fn write_plain(&mut self, buffer: &[u8], deadline: Instant) -> io::Result<usize> {
        let written = self.connection.writer().write(buffer)?;
        if written == 0 && !buffer.is_empty() {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        self.write_tls(deadline)?;
        Ok(written)
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeTcpListenerError> {
        self.stream.begin_protocol_work()
    }

    fn complete_admission(&mut self) -> Result<(), RuntimeTcpListenerError> {
        self.stream.complete_admission()
    }

    fn take_read_control_write_error(&mut self) -> Option<RuntimeTcpConnectionError> {
        self.read_control_write_error.take()
    }

    fn read_tls(&mut self, deadline: Instant) -> io::Result<usize> {
        let read = loop {
            self.stream.set_read_timeout(remaining_timeout(deadline)?)?;
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
            let written = loop {
                self.stream
                    .set_write_timeout(remaining_timeout(deadline)?)?;
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
                if let Err(error) =
                    self.write_tls(bounded_write_deadline(deadline, self.write_timeout))
                {
                    self.read_control_write_error =
                        Some(map_tls_write_error(io::Error::from(error.kind())));
                    return Err(error);
                }
            }
            if !self.connection.wants_read() {
                return Ok(0);
            }
            self.read_tls(deadline)?;
        }
    }
}

enum TcpTransport {
    Plain(Box<AcceptedTcpStream>),
    Tls(Box<TlsTransport>),
}

type TcpFactory = AuthorizedDatabaseAdapterFactory<crate::RuntimeAccountStore>;
type TcpOrchestrator = ClassicConnectionOrchestrator<Arc<crate::RuntimeAccountStore>, TcpFactory>;

/// Owns one accepted stream through mandatory TLS, authentication, and commands.
pub(crate) struct RuntimeTcpConnection {
    transport: Option<TcpTransport>,
    tls_config: Arc<rustls::ServerConfig>,
    orchestrator: TcpOrchestrator,
    codec: PacketCodec,
    tls_deadline: Instant,
    timeouts: crate::RuntimeTimeouts,
    transport_closed: bool,
}

impl RuntimeTcpConnection {
    /// Creates the only protocol owner accepted by the TCP listener.
    pub(crate) fn new(stream: AcceptedTcpStream) -> Result<Self, RuntimeTcpConnectionError> {
        let limits = stream.limits();
        let timeouts = stream.timeouts();
        let settings = tcp_handshake_settings(stream.connection_id());
        let verifier = CachingSha2Verifier::new(stream.account_store());
        let factory = AuthorizedDatabaseAdapterFactory::new(
            stream.catalog(),
            binary_schema_context(),
            stream.account_store(),
        )
        .with_query_timeout(timeouts.query())
        .with_bootstrap_settings(MAX_COMMAND_PAYLOAD_LENGTH, timeouts.idle());
        let codec = PacketCodec::new(MAX_COMMAND_PAYLOAD_LENGTH)
            .map_err(RuntimeTcpConnectionError::PacketCodec)?;
        let orchestrator = ClassicConnectionOrchestrator::new(
            settings,
            verifier,
            factory,
            limits.max_write_bytes(),
            limits.max_write_frames(),
        )
        .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        let tls_config = stream.tls_config().server_config();
        let tls_deadline = stream.tls_deadline();
        Ok(Self {
            transport: Some(TcpTransport::Plain(Box::new(stream))),
            tls_config,
            orchestrator,
            codec,
            tls_deadline,
            timeouts,
            transport_closed: false,
        })
    }

    /// Runs the complete serial protocol loop and closes orchestration once.
    pub(crate) fn run(mut self) -> Result<(), RuntimeTcpConnectionError> {
        let result = self.run_inner();
        let close = self.close_transport();
        match result {
            Ok(()) => close,
            Err(error) => {
                debug_assert!(close.is_ok(), "failed owner states must still close");
                Err(error)
            }
        }
    }

    fn run_inner(&mut self) -> Result<(), RuntimeTcpConnectionError> {
        self.begin_protocol_work()?;

        let event = self
            .orchestrator
            .start()
            .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        if event != OrchestratorEvent::AwaitingClientFrame {
            return Err(RuntimeTcpConnectionError::UnexpectedEvent(event));
        }
        self.flush_plain_writes(bounded_write_deadline(
            self.tls_deadline,
            self.timeouts.write(),
        ))?;

        let request = match self.transport.as_mut() {
            Some(TcpTransport::Plain(stream)) => {
                read_ssl_request_packet(stream.as_mut(), self.codec, self.tls_deadline).map_err(
                    |error| match error {
                        PreTlsPacketError::DeadlineExceeded => {
                            RuntimeTcpConnectionError::TlsDeadlineExceeded
                        }
                        error => RuntimeTcpConnectionError::PlaintextRejected(error),
                    },
                )?
            }
            Some(TcpTransport::Tls(_)) | None => {
                return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
            }
        };
        self.begin_protocol_work()?;
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
        let mut tls = TlsTransport::new(connection, *stream, self.timeouts.write());
        tls.complete_handshake(self.tls_deadline)?;
        self.transport = Some(TcpTransport::Tls(Box::new(tls)));

        self.begin_protocol_work()?;
        let event = self
            .orchestrator
            .tls_negotiated()
            .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        if event != OrchestratorEvent::AwaitingClientFrame {
            return Err(RuntimeTcpConnectionError::UnexpectedEvent(event));
        }
        self.run_tls_frames(Instant::now() + self.timeouts.authentication())
    }

    fn run_tls_frames(
        &mut self,
        authentication_deadline: Instant,
    ) -> Result<(), RuntimeTcpConnectionError> {
        let mut admission_complete = false;
        let mut read_deadline = authentication_deadline;
        loop {
            let kind = if admission_complete {
                DeadlineKind::Idle
            } else {
                DeadlineKind::Authentication
            };
            let Some(frame) = self.read_tls_frame(read_deadline, kind)? else {
                return Ok(());
            };
            self.begin_protocol_work()?;
            let event = self
                .orchestrator
                .receive_frame(frame)
                .map_err(RuntimeTcpConnectionError::Orchestrator)?;
            self.flush_tls_writes(bounded_write_deadline(
                if admission_complete {
                    Instant::now() + self.timeouts.write()
                } else {
                    authentication_deadline
                },
                self.timeouts.write(),
            ))?;

            match event {
                OrchestratorEvent::Ready => {
                    if !admission_complete {
                        self.complete_admission()?;
                        admission_complete = true;
                    }
                    read_deadline = Instant::now() + self.timeouts.idle();
                }
                OrchestratorEvent::AwaitingClientFrame => {}
                OrchestratorEvent::Closing | OrchestratorEvent::Closed => return Ok(()),
                OrchestratorEvent::TlsUpgradeRequired => {
                    return Err(RuntimeTcpConnectionError::UnexpectedEvent(event));
                }
            }
        }
    }

    fn read_tls_frame(
        &mut self,
        deadline: Instant,
        kind: DeadlineKind,
    ) -> Result<Option<ClassicFrame>, RuntimeTcpConnectionError> {
        let (result, control_write_error) = match self.transport.as_mut() {
            Some(TcpTransport::Tls(tls)) => {
                let result = read_classic_frame(tls.as_mut(), self.codec, deadline);
                (result, tls.take_read_control_write_error())
            }
            Some(TcpTransport::Plain(_)) | None => {
                return Err(RuntimeTcpConnectionError::UnexpectedTransportState);
            }
        };
        if let Some(error) = control_write_error {
            return Err(error);
        }
        match result {
            Ok(frame) => Ok(Some(frame)),
            Err(PreTlsPacketError::DeadlineExceeded) => Err(kind.exceeded()),
            Err(PreTlsPacketError::TruncatedHeader { actual: 0 })
                if matches!(kind, DeadlineKind::Idle) =>
            {
                Ok(None)
            }
            Err(PreTlsPacketError::TruncatedHeader { actual: 0 }) => {
                Err(RuntimeTcpConnectionError::TlsPeerClosed)
            }
            Err(
                PreTlsPacketError::TruncatedHeader { .. }
                | PreTlsPacketError::TruncatedPayload { .. },
            ) => Err(RuntimeTcpConnectionError::TruncatedFrame),
            Err(PreTlsPacketError::ReadFailed) => Err(RuntimeTcpConnectionError::TlsReadFailed),
            Err(error) => Err(RuntimeTcpConnectionError::Packet(error)),
        }
    }

    fn flush_plain_writes(&mut self, deadline: Instant) -> Result<(), RuntimeTcpConnectionError> {
        loop {
            let Some(frame) = self.orchestrator.front_write() else {
                return Ok(());
            };
            let written = match self.transport.as_mut() {
                Some(TcpTransport::Plain(stream)) => write_with_deadline(stream, frame, deadline)?,
                Some(TcpTransport::Tls(_)) | None => {
                    return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
                }
            };
            self.orchestrator
                .advance_write(written)
                .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        }
    }

    fn flush_tls_writes(&mut self, deadline: Instant) -> Result<(), RuntimeTcpConnectionError> {
        loop {
            let Some(frame) = self.orchestrator.front_write() else {
                return Ok(());
            };
            let written = match self.transport.as_mut() {
                Some(TcpTransport::Tls(tls)) => tls
                    .write_plain(frame, deadline)
                    .map_err(map_tls_write_error)?,
                Some(TcpTransport::Plain(_)) | None => {
                    return Err(RuntimeTcpConnectionError::UnexpectedTransportState)
                }
            };
            self.orchestrator
                .advance_write(written)
                .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        }
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeTcpConnectionError> {
        match self.transport.as_ref() {
            Some(TcpTransport::Plain(stream)) => stream
                .begin_protocol_work()
                .map_err(RuntimeTcpConnectionError::Listener),
            Some(TcpTransport::Tls(tls)) => tls
                .begin_protocol_work()
                .map_err(RuntimeTcpConnectionError::Listener),
            None => Err(RuntimeTcpConnectionError::UnexpectedTransportState),
        }
    }

    fn complete_admission(&mut self) -> Result<(), RuntimeTcpConnectionError> {
        match self.transport.as_mut() {
            Some(TcpTransport::Tls(tls)) => tls
                .complete_admission()
                .map_err(RuntimeTcpConnectionError::Listener),
            Some(TcpTransport::Plain(_)) | None => {
                Err(RuntimeTcpConnectionError::UnexpectedTransportState)
            }
        }
    }

    fn close_transport(&mut self) -> Result<(), RuntimeTcpConnectionError> {
        assert!(
            !self.transport_closed,
            "a TCP owner reports transport closure exactly once"
        );
        self.transport_closed = true;
        let event = self
            .orchestrator
            .transport_closed()
            .map_err(RuntimeTcpConnectionError::Orchestrator)?;
        if event != OrchestratorEvent::Closed {
            return Err(RuntimeTcpConnectionError::UnexpectedEvent(event));
        }
        Ok(())
    }
}

impl Drop for RuntimeTcpConnection {
    fn drop(&mut self) {
        if !self.transport_closed {
            let _ = self.close_transport();
        }
    }
}

fn tcp_handshake_settings(connection_id: u32) -> InitialHandshakeSettings {
    assert_ne!(
        connection_id, 0,
        "accepted TCP connections need non-zero IDs"
    );
    InitialHandshakeSettings {
        connection_id,
        capability_flags: SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
        ..InitialHandshakeSettings::default()
    }
}

fn binary_schema_context() -> SchemaSqlSessionContext {
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

fn write_with_deadline(
    stream: &mut AcceptedTcpStream,
    buffer: &[u8],
    deadline: Instant,
) -> Result<usize, RuntimeTcpConnectionError> {
    loop {
        stream
            .set_write_timeout(remaining_timeout(deadline).map_err(map_plain_write_error)?)
            .map_err(map_plain_write_error)?;
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
        RuntimeTcpConnectionError::WriteDeadlineExceeded
    } else {
        RuntimeTcpConnectionError::WriteFailed
    }
}

fn map_tls_handshake_error(error: io::Error) -> RuntimeTcpConnectionError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        RuntimeTcpConnectionError::TlsDeadlineExceeded
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
        RuntimeTcpConnectionError::WriteDeadlineExceeded
    } else {
        RuntimeTcpConnectionError::TlsWriteFailed
    }
}

fn bounded_write_deadline(phase_deadline: Instant, write_timeout: Duration) -> Instant {
    phase_deadline.min(Instant::now() + write_timeout)
}

#[derive(Clone, Copy)]
enum DeadlineKind {
    Authentication,
    Idle,
}

impl DeadlineKind {
    fn exceeded(self) -> RuntimeTcpConnectionError {
        match self {
            Self::Authentication => RuntimeTcpConnectionError::AuthenticationDeadlineExceeded,
            Self::Idle => RuntimeTcpConnectionError::IdleDeadlineExceeded,
        }
    }
}

/// Redacted failures from the TCP/TLS protocol owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTcpConnectionError {
    /// The listener could not finish its owned accepted stream.
    Listener(RuntimeTcpListenerError),
    /// The packet codec could not be constructed or validated.
    PacketCodec(PacketCodecError),
    /// A bounded classic packet could not be read or validated.
    Packet(PreTlsPacketError),
    /// The first client packet was not the mandatory SSLRequest.
    PlaintextRejected(PreTlsPacketError),
    /// The protocol orchestrator rejected an owner action.
    Orchestrator(OrchestratorError),
    /// The mandatory TLS transition did not finish by its fixed deadline.
    TlsDeadlineExceeded,
    /// Authentication did not finish by its fixed post-TLS deadline.
    AuthenticationDeadlineExceeded,
    /// A complete command did not arrive by the current idle deadline.
    IdleDeadlineExceeded,
    /// A queued response did not drain by its fixed write deadline.
    WriteDeadlineExceeded,
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
    /// The peer closed after starting a bounded classic packet.
    TruncatedFrame,
    /// The owner transport variant did not match the protocol phase.
    UnexpectedTransportState,
    /// The orchestrator returned an event that does not fit this phase.
    UnexpectedEvent(OrchestratorEvent),
}

impl fmt::Display for RuntimeTcpConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listener(error) => write!(f, "TCP listener operation failed: {error}"),
            Self::PacketCodec(error) => write!(f, "TCP packet codec failed: {error}"),
            Self::Packet(error) => write!(f, "TCP packet boundary failed: {error}"),
            Self::PlaintextRejected(error) => {
                write!(f, "TCP client did not begin mandatory TLS: {error}")
            }
            Self::Orchestrator(error) => write!(f, "TCP protocol orchestration failed: {error}"),
            Self::TlsDeadlineExceeded => f.write_str("TCP TLS deadline elapsed"),
            Self::AuthenticationDeadlineExceeded => {
                f.write_str("TCP authentication deadline elapsed")
            }
            Self::IdleDeadlineExceeded => f.write_str("TCP idle deadline elapsed"),
            Self::WriteDeadlineExceeded => f.write_str("TCP write deadline elapsed"),
            Self::WriteFailed => f.write_str("TCP plaintext write failed"),
            Self::TlsReadFailed => f.write_str("TLS read failed"),
            Self::TlsWriteFailed => f.write_str("TLS write failed"),
            Self::TlsPeerClosed => f.write_str("TLS peer closed the connection"),
            Self::TlsConfiguration => f.write_str("TLS configuration failed"),
            Self::TlsHandshakeFailed => f.write_str("TLS handshake failed"),
            Self::TruncatedFrame => f.write_str("TCP connection closed during a packet"),
            Self::UnexpectedTransportState => f.write_str("TCP transport state was unexpected"),
            Self::UnexpectedEvent(event) => {
                write!(f, "TCP protocol event was unexpected: {event:?}")
            }
        }
    }
}

impl Error for RuntimeTcpConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Listener(error) => Some(error),
            Self::PacketCodec(error) => Some(error),
            Self::Packet(error) | Self::PlaintextRejected(error) => Some(error),
            Self::Orchestrator(error) => Some(error),
            Self::TlsDeadlineExceeded
            | Self::AuthenticationDeadlineExceeded
            | Self::IdleDeadlineExceeded
            | Self::WriteDeadlineExceeded
            | Self::WriteFailed
            | Self::TlsReadFailed
            | Self::TlsWriteFailed
            | Self::TlsPeerClosed
            | Self::TlsConfiguration
            | Self::TlsHandshakeFailed
            | Self::TruncatedFrame
            | Self::UnexpectedTransportState
            | Self::UnexpectedEvent(_) => None,
        }
    }
}

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
