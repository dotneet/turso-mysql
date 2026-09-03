//! A complete-frame owner for one classic MySQL connection.
//!
//! This module deliberately stops at the transport boundary. Callers provide
//! one complete classic packet at a time and own the socket, TLS engine, and
//! scheduling around this type. The orchestrator owns the protocol state,
//! verifier, command adapter, and bounded response queue.

use std::{error::Error, fmt};

use crate::{
    CachingSha2Verifier, ClassicConnection, CommandDispatcher, CommandDispatcherError,
    CommandExecutor, ConnectionState, ConnectionStateError, CredentialProvider,
    InitialDatabaseSelector, InitialHandshakeSettings, PacketCodec, PacketCodecError,
    PacketWriteQueue, PacketWriteQueueError, TransportSecurity, CLIENT_SSL,
};

/// A complete, owned classic MySQL packet frame.
///
/// Construction validates the four-byte header, declared payload length, and
/// configured payload limit. A stream decoder belongs outside the
/// orchestrator; this type intentionally cannot represent a partial frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassicFrame {
    bytes: Vec<u8>,
}

impl ClassicFrame {
    /// Validates and owns one complete packet frame.
    pub fn new(codec: PacketCodec, bytes: Vec<u8>) -> Result<Self, PacketCodecError> {
        codec.decode(&bytes)?;
        Ok(Self { bytes })
    }

    /// Encodes and owns one complete packet frame.
    pub fn from_payload(
        codec: PacketCodec,
        sequence_id: u8,
        payload: &[u8],
    ) -> Result<Self, PacketCodecError> {
        Ok(Self {
            bytes: codec.encode(sequence_id, payload)?,
        })
    }

    /// Returns the complete frame bytes for transport output or inspection.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the owned complete frame bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// The externally visible result of one orchestrator action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestratorEvent {
    /// The caller may provide the next complete client frame.
    AwaitingClientFrame,
    /// The client sent an SSLRequest; the caller must complete TLS externally.
    TlsUpgradeRequired,
    /// Authentication completed and commands may be sent.
    Ready,
    /// Closing has started; the caller may flush pending output before ending
    /// the transport.
    Closing,
    /// The transport has closed and no more protocol work is accepted.
    Closed,
}

/// Errors from complete-frame connection orchestration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestratorError {
    /// The protocol state machine rejected an action or frame.
    Connection(ConnectionStateError),
    /// Ready-command decoding, execution, or response encoding failed.
    Dispatch(CommandDispatcherError),
    /// A response could not be retained in the bounded write queue.
    WriteQueue(PacketWriteQueueError),
    /// A transport reported no progress while a response frame was pending.
    ZeroByteWrite,
    /// A plaintext-starting server must advertise the mandatory TLS upgrade.
    TlsCapabilityRequired,
    /// The write acknowledgement was not valid for the queue's front frame.
    WriteAdvance(PacketWriteQueueError),
}

impl From<ConnectionStateError> for OrchestratorError {
    fn from(error: ConnectionStateError) -> Self {
        Self::Connection(error)
    }
}

impl From<CommandDispatcherError> for OrchestratorError {
    fn from(error: CommandDispatcherError) -> Self {
        Self::Dispatch(error)
    }
}

impl fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(error) => write!(f, "connection orchestration failed: {error}"),
            Self::Dispatch(error) => write!(f, "command dispatch failed: {error}"),
            Self::WriteQueue(error) => write!(f, "response queue failed: {error}"),
            Self::ZeroByteWrite => f.write_str("transport made no progress writing a response"),
            Self::TlsCapabilityRequired => {
                f.write_str("plaintext connection must advertise CLIENT_SSL")
            }
            Self::WriteAdvance(error) => write!(f, "response write advance failed: {error}"),
        }
    }
}

impl Error for OrchestratorError {}

/// Owns all protocol-side state for one complete-frame classic connection.
///
/// `P` is the credential provider and `E` is the single command adapter that
/// both executes ready commands and selects databases. Keeping those traits on
/// one owned value is important: `USE`, handshake database selection, and
/// queries must update the same session. Network authorization is outside this
/// trusted protocol owner and must wrap command execution before a production
/// listener is connected.
pub struct ClassicConnectionOrchestrator<P, E>
where
    P: CredentialProvider,
    E: CommandExecutor + InitialDatabaseSelector,
{
    connection: ClassicConnection,
    verifier: CachingSha2Verifier<P>,
    executor: E,
    dispatcher: CommandDispatcher,
    write_queue: PacketWriteQueue,
}

impl<P, E> fmt::Debug for ClassicConnectionOrchestrator<P, E>
where
    P: CredentialProvider,
    E: CommandExecutor + InitialDatabaseSelector + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassicConnectionOrchestrator")
            .field("connection", &self.connection)
            .field("verifier", &self.verifier)
            .field("executor", &self.executor)
            .field("write_queue", &self.write_queue)
            .finish()
    }
}

impl<P, E> ClassicConnectionOrchestrator<P, E>
where
    P: CredentialProvider,
    E: CommandExecutor + InitialDatabaseSelector,
{
    /// Creates a plaintext-starting orchestrator with the standard bounded
    /// packet codec and a caller-selected response queue budget.
    pub fn new(
        settings: InitialHandshakeSettings,
        verifier: CachingSha2Verifier<P>,
        executor: E,
        max_queued_bytes: usize,
        max_queued_frames: usize,
    ) -> Result<Self, OrchestratorError> {
        if settings.capability_flags & CLIENT_SSL == 0 {
            return Err(OrchestratorError::TlsCapabilityRequired);
        }
        Self::with_transport_security(
            settings,
            TransportSecurity::Plaintext,
            verifier,
            executor,
            max_queued_bytes,
            max_queued_frames,
        )
    }

    /// Creates an orchestrator for an in-crate transport owner that has already
    /// established whether this connection starts secure.
    pub(crate) fn with_transport_security(
        settings: InitialHandshakeSettings,
        transport_security: TransportSecurity,
        verifier: CachingSha2Verifier<P>,
        executor: E,
        max_queued_bytes: usize,
        max_queued_frames: usize,
    ) -> Result<Self, OrchestratorError> {
        let codec = PacketCodec::new(
            crate::MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH
                .max(crate::MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH),
        )?;
        let connection = ClassicConnection::with_codec(settings, codec, transport_security)?;
        let write_queue = PacketWriteQueue::new(codec, max_queued_bytes, max_queued_frames)
            .map_err(OrchestratorError::WriteQueue)?;
        Ok(Self {
            connection,
            verifier,
            executor,
            dispatcher: CommandDispatcher::new(),
            write_queue,
        })
    }

    /// Takes ownership of an already-created connection and response queue.
    ///
    /// This constructor is reserved for in-crate tests that create a
    /// deterministic nonce or use a separately configured packet codec.
    #[cfg(test)]
    pub(crate) fn from_parts(
        connection: ClassicConnection,
        verifier: CachingSha2Verifier<P>,
        executor: E,
        write_queue: PacketWriteQueue,
    ) -> Self {
        Self {
            connection,
            verifier,
            executor,
            dispatcher: CommandDispatcher::new(),
            write_queue,
        }
    }

    /// Returns the current protocol state.
    pub const fn state(&self) -> ConnectionState {
        self.connection.state()
    }

    /// Returns the event corresponding to the current protocol state.
    pub const fn event(&self) -> OrchestratorEvent {
        match self.connection.state() {
            ConnectionState::TlsUpgradeRequired => OrchestratorEvent::TlsUpgradeRequired,
            ConnectionState::Ready => OrchestratorEvent::Ready,
            ConnectionState::Closing => OrchestratorEvent::Closing,
            ConnectionState::Closed => OrchestratorEvent::Closed,
            ConnectionState::SendInitialHandshake
            | ConnectionState::AwaitClientResponse
            | ConnectionState::TlsNegotiated
            | ConnectionState::AuthenticateCachingSha2Password
            | ConnectionState::AuthenticateFast
            | ConnectionState::AuthenticateFull
            | ConnectionState::AuthenticateFullVerification => {
                OrchestratorEvent::AwaitingClientFrame
            }
        }
    }

    /// Emits and queues the server's initial handshake.
    pub fn start(&mut self) -> Result<OrchestratorEvent, OrchestratorError> {
        let frame = match self.connection.send_initial_handshake() {
            Ok(frame) => frame,
            Err(error) => return self.fail(OrchestratorError::Connection(error)),
        };
        if let Err(error) = self.write_queue.enqueue(frame) {
            return self.fail(OrchestratorError::WriteQueue(error));
        }
        Ok(self.event())
    }

    /// Supplies one complete client frame and advances the protocol.
    ///
    /// Responses remain in wire order when the caller receives another frame
    /// before earlier output has drained. A credential or provider failure
    /// does not synthesize an error packet; already queued protocol output is
    /// left for the transport to flush or discard with [`Self::transport_closed`].
    pub fn receive_frame(
        &mut self,
        frame: ClassicFrame,
    ) -> Result<OrchestratorEvent, OrchestratorError> {
        let result = self.receive_frame_inner(frame.as_bytes());
        match result {
            Ok(event) => Ok(event),
            Err(error) => self.fail(error),
        }
    }

    /// Reports completion of an externally owned TLS handshake.
    pub fn tls_negotiated(&mut self) -> Result<OrchestratorEvent, OrchestratorError> {
        match self.connection.tls_upgrade_complete() {
            Ok(()) => Ok(self.event()),
            Err(error) => self.fail(OrchestratorError::Connection(error)),
        }
    }

    /// Starts graceful protocol shutdown while retaining queued output.
    pub fn close(&mut self) -> Result<OrchestratorEvent, OrchestratorError> {
        match self.connection.state() {
            ConnectionState::Closing | ConnectionState::Closed => Ok(self.event()),
            _ => match self.connection.begin_close() {
                Ok(()) => Ok(self.event()),
                Err(error) => self.fail(OrchestratorError::Connection(error)),
            },
        }
    }

    /// Reports that the transport is gone and discards any unflushed output.
    pub fn transport_closed(&mut self) -> Result<OrchestratorEvent, OrchestratorError> {
        if self.connection.state() == ConnectionState::Closed {
            self.write_queue.reset();
            return Ok(OrchestratorEvent::Closed);
        }
        if self.connection.state() != ConnectionState::Closing {
            self.connection.begin_close()?;
        }
        self.connection.finish_close()?;
        self.write_queue.reset();
        Ok(OrchestratorEvent::Closed)
    }

    /// Returns the oldest unsent response bytes, if any.
    pub fn front_write(&self) -> Option<&[u8]> {
        self.write_queue.front()
    }

    /// Acknowledges bytes written from the oldest queued response frame.
    pub fn advance_write(&mut self, written: usize) -> Result<(), OrchestratorError> {
        if written == 0 && self.write_queue.front().is_some() {
            return self.fail(OrchestratorError::ZeroByteWrite);
        }
        match self.write_queue.advance(written) {
            Ok(()) => Ok(()),
            Err(error) => self.fail(OrchestratorError::WriteAdvance(error)),
        }
    }

    fn receive_frame_inner(
        &mut self,
        frame: &[u8],
    ) -> Result<OrchestratorEvent, OrchestratorError> {
        match self.connection.state() {
            ConnectionState::AwaitClientResponse | ConnectionState::TlsNegotiated => {
                self.connection.receive_client_handshake_frame(frame)?;
                if self.connection.state() == ConnectionState::TlsUpgradeRequired {
                    return Ok(self.event());
                }
                if self.connection.state() == ConnectionState::TlsNegotiated {
                    self.connection.begin_authentication()?;
                }
                self.authenticate_initial()?;
            }
            ConnectionState::AuthenticateFull => self.authenticate_full(frame)?,
            ConnectionState::Ready => {
                let frames =
                    self.dispatcher
                        .dispatch(&mut self.connection, &mut self.executor, frame)?;
                self.write_queue
                    .enqueue_batch(frames)
                    .map_err(OrchestratorError::WriteQueue)?;
            }
            state => {
                return Err(OrchestratorError::Connection(
                    ConnectionStateError::InvalidTransition {
                        state,
                        event: crate::ConnectionEvent::ReceiveClientResponse,
                    },
                ));
            }
        }
        Ok(self.event())
    }

    fn authenticate_initial(&mut self) -> Result<(), OrchestratorError> {
        let result = {
            let request = self.connection.authentication_verification_request()?;
            self.verifier
                .verify_initial(&request)
                .map_err(ConnectionStateError::CredentialVerification)?
        };
        let auth_frame = self
            .connection
            .apply_initial_authentication_result(result)?;
        let mut frames = vec![auth_frame];
        if self.connection.state() == ConnectionState::AuthenticateFast {
            let response = self
                .connection
                .send_authentication_ok_with_selector(&mut self.executor)?;
            frames.push(response.frame().to_vec());
        }
        self.write_queue
            .enqueue_batch(frames)
            .map_err(OrchestratorError::WriteQueue)?;
        Ok(())
    }

    fn authenticate_full(&mut self, frame: &[u8]) -> Result<(), OrchestratorError> {
        let result = {
            let request = self.connection.receive_full_authentication_frame(frame)?;
            self.verifier
                .verify_full(&request)
                .map_err(ConnectionStateError::CredentialVerification)?
        };
        let response = self
            .connection
            .apply_full_authentication_result_with_selector(result, &mut self.executor)?;
        self.write_queue
            .enqueue_batch([response.frame().to_vec()])
            .map_err(OrchestratorError::WriteQueue)
    }

    fn fail<T>(&mut self, error: OrchestratorError) -> Result<T, OrchestratorError> {
        if !matches!(
            self.connection.state(),
            ConnectionState::Closing | ConnectionState::Closed
        ) {
            self.connection
                .begin_close()
                .expect("every active protocol state can begin close");
        }
        Err(error)
    }
}

impl From<PacketCodecError> for OrchestratorError {
    fn from(error: PacketCodecError) -> Self {
        Self::Connection(ConnectionStateError::PacketCodec(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClientHandshakeResponseConfig, CommandExecutionResult, CommandOkResult,
        InitialHandshakeSettings, StoredCredential, AUTH_PLUGIN_DATA_LENGTH,
        CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_SSL, FAST_AUTH_RESPONSE_LENGTH,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };
    use sha2::{Digest, Sha256};

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: 4096,
    };
    const SCRAMBLE: [u8; AUTH_PLUGIN_DATA_LENGTH] = [0x52; AUTH_PLUGIN_DATA_LENGTH];

    #[derive(Debug, Default)]
    struct TestExecutor;

    impl CommandExecutor for TestExecutor {
        fn execute_init_db(
            &mut self,
            _database: &str,
        ) -> Result<CommandExecutionResult, crate::FrontendErrorKind> {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }

        fn execute_query(
            &mut self,
            _sql: &str,
        ) -> Result<CommandExecutionResult, crate::FrontendErrorKind> {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }
    }

    impl InitialDatabaseSelector for TestExecutor {
        fn select_initial_database(
            &mut self,
            _database: &str,
        ) -> Result<(), crate::FrontendErrorKind> {
            Ok(())
        }
    }

    fn verifier_material(password: &[u8]) -> [u8; 32] {
        let first = Sha256::digest(password);
        let second = Sha256::digest(first);
        second.into()
    }

    fn fast_response(password: &[u8]) -> [u8; FAST_AUTH_RESPONSE_LENGTH] {
        let first = Sha256::digest(password);
        let second = Sha256::digest(first);
        let third = Sha256::digest(second);
        let mut challenge = Vec::with_capacity(third.len() + SCRAMBLE.len());
        challenge.extend_from_slice(&third);
        challenge.extend_from_slice(&SCRAMBLE);
        let mask = Sha256::digest(challenge);
        let mut response = [0; FAST_AUTH_RESPONSE_LENGTH];
        for (out, (&password_hash, &mask_byte)) in
            response.iter_mut().zip(first.iter().zip(mask.iter()))
        {
            *out = password_hash ^ mask_byte;
        }
        response
    }

    fn settings() -> InitialHandshakeSettings {
        InitialHandshakeSettings {
            capability_flags: REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                | crate::CLIENT_SSL
                | crate::CLIENT_CONNECT_WITH_DB,
            ..InitialHandshakeSettings::default()
        }
    }

    fn client_response(auth_response: Vec<u8>, database: Option<String>) -> ClassicFrame {
        ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                | database
                    .as_ref()
                    .map_or(0, |_| crate::CLIENT_CONNECT_WITH_DB),
            0,
            crate::DEFAULT_UTF8MB4_COLLATION,
            "root",
            auth_response,
            database,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(CODEC, 1)
        .map(|bytes| ClassicFrame::new(CODEC, bytes).unwrap())
        .unwrap()
    }

    fn orchestrator(
        database: Option<String>,
    ) -> ClassicConnectionOrchestrator<crate::InMemoryCredentialProvider, TestExecutor> {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let connection = ClassicConnection::with_test_nonce(
            settings(),
            CODEC,
            TransportSecurity::Secure,
            SCRAMBLE,
        )
        .unwrap();
        let queue = PacketWriteQueue::new(CODEC, 128, 8).unwrap();
        let mut orchestrator = ClassicConnectionOrchestrator::from_parts(
            connection,
            CachingSha2Verifier::new(provider),
            TestExecutor,
            queue,
        );
        assert_eq!(
            orchestrator.start().unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        assert!(orchestrator.front_write().is_some());
        let response = client_response(fast_response(password).to_vec(), database);
        assert_eq!(
            orchestrator.receive_frame(response).unwrap(),
            OrchestratorEvent::Ready
        );
        orchestrator
    }

    #[test]
    fn complete_frame_constructor_rejects_partial_and_trailing_bytes() {
        assert_eq!(
            ClassicFrame::new(CODEC, vec![1, 0, 0]),
            Err(crate::PacketCodecError::TruncatedHeader { actual: 3 })
        );
        let mut trailing = CODEC.encode(0, b"x").unwrap();
        trailing.push(0);
        assert!(matches!(
            ClassicFrame::new(CODEC, trailing),
            Err(crate::PacketCodecError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn public_constructor_requires_the_tls_upgrade_capability() {
        let result = ClassicConnectionOrchestrator::new(
            InitialHandshakeSettings::default(),
            CachingSha2Verifier::<crate::DefaultCredentialProvider>::default(),
            TestExecutor,
            128,
            8,
        );

        assert!(matches!(
            result,
            Err(OrchestratorError::TlsCapabilityRequired)
        ));
    }

    #[test]
    fn secure_fast_auth_queues_handshake_auth_more_and_final_ok_in_order() {
        let orchestrator = orchestrator(None);
        let mut frames = Vec::new();
        let mut current = orchestrator;
        while let Some(front) = current.front_write() {
            frames.push(front.to_vec());
            let len = front.len();
            current.advance_write(len).unwrap();
        }
        assert_eq!(frames.len(), 3);
        assert_eq!(CODEC.decode(&frames[0]).unwrap().sequence_id, 0);
        assert_eq!(CODEC.decode(&frames[1]).unwrap().sequence_id, 2);
        assert_eq!(CODEC.decode(&frames[2]).unwrap().sequence_id, 3);
        assert_eq!(current.state(), ConnectionState::Ready);
    }

    #[test]
    fn secure_full_auth_selects_initial_database_before_final_ok() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(password)),
            )
            .unwrap();
        let connection = ClassicConnection::with_test_nonce(
            settings(),
            CODEC,
            TransportSecurity::Secure,
            SCRAMBLE,
        )
        .unwrap();
        let queue = PacketWriteQueue::new(CODEC, 256, 8).unwrap();
        let mut orchestrator = ClassicConnectionOrchestrator::from_parts(
            connection,
            CachingSha2Verifier::new(provider),
            TestExecutor,
            queue,
        );
        orchestrator.start().unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(client_response(
                    vec![0; FAST_AUTH_RESPONSE_LENGTH],
                    Some("tenant".to_owned()),
                ))
                .unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        assert_eq!(orchestrator.state(), ConnectionState::AuthenticateFull);
        let full = ClassicFrame::from_payload(CODEC, 3, b"secret\0").unwrap();
        assert_eq!(
            orchestrator.receive_frame(full).unwrap(),
            OrchestratorEvent::Ready
        );
        assert_eq!(orchestrator.state(), ConnectionState::Ready);
        let mut last = None;
        while let Some(front) = orchestrator.front_write() {
            last = Some(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        assert_eq!(CODEC.decode(&last.unwrap()).unwrap().sequence_id, 4);
    }

    #[test]
    fn ssl_request_is_an_external_upgrade_event_and_transport_close_is_idempotent() {
        let mut orchestrator = ClassicConnectionOrchestrator::with_transport_security(
            settings(),
            TransportSecurity::Plaintext,
            CachingSha2Verifier::<crate::DefaultCredentialProvider>::default(),
            TestExecutor,
            128,
            8,
        )
        .unwrap();
        orchestrator.start().unwrap();
        let ssl = crate::ClientSslRequestConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
            crate::DEFAULT_UTF8MB4_COLLATION,
        )
        .encode(CODEC, 1)
        .unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(ClassicFrame::new(CODEC, ssl).unwrap())
                .unwrap(),
            OrchestratorEvent::TlsUpgradeRequired
        );
        assert_eq!(
            orchestrator.tls_negotiated().unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        assert_eq!(
            orchestrator.transport_closed().unwrap(),
            OrchestratorEvent::Closed
        );
        assert_eq!(
            orchestrator.transport_closed().unwrap(),
            OrchestratorEvent::Closed
        );
        assert_eq!(orchestrator.state(), ConnectionState::Closed);
    }

    #[test]
    fn tls_upgrade_then_post_tls_handshake_runs_fast_auth_with_correct_sequences() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let connection = ClassicConnection::with_test_nonce(
            settings(),
            CODEC,
            TransportSecurity::Plaintext,
            SCRAMBLE,
        )
        .unwrap();
        let queue = PacketWriteQueue::new(CODEC, 256, 8).unwrap();
        let mut orchestrator = ClassicConnectionOrchestrator::from_parts(
            connection,
            CachingSha2Verifier::new(provider),
            TestExecutor,
            queue,
        );
        orchestrator.start().unwrap();
        let ssl = crate::ClientSslRequestConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
            crate::DEFAULT_UTF8MB4_COLLATION,
        )
        .encode(CODEC, 1)
        .unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(ClassicFrame::new(CODEC, ssl).unwrap())
                .unwrap(),
            OrchestratorEvent::TlsUpgradeRequired
        );
        orchestrator.tls_negotiated().unwrap();
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
            crate::DEFAULT_UTF8MB4_COLLATION,
            "root",
            fast_response(password),
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(CODEC, 2)
        .unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(ClassicFrame::new(CODEC, response).unwrap())
                .unwrap(),
            OrchestratorEvent::Ready
        );
        let mut frames = Vec::new();
        while let Some(front) = orchestrator.front_write() {
            frames.push(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        assert_eq!(frames.len(), 3);
        assert_eq!(CODEC.decode(&frames[1]).unwrap().sequence_id, 3);
        assert_eq!(CODEC.decode(&frames[2]).unwrap().sequence_id, 4);
    }

    #[test]
    fn quit_closes_without_queuing_a_response_and_rejects_follow_up_frames() {
        let mut orchestrator = orchestrator(None);
        while let Some(front) = orchestrator.front_write() {
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        let quit =
            ClassicFrame::from_payload(CODEC, crate::COMMAND_SEQUENCE_ID, &[crate::COM_QUIT])
                .unwrap();
        assert_eq!(
            orchestrator.receive_frame(quit).unwrap(),
            OrchestratorEvent::Closing
        );
        assert_eq!(orchestrator.front_write(), None);
        let ping =
            ClassicFrame::from_payload(CODEC, crate::COMMAND_SEQUENCE_ID, &[crate::COM_PING])
                .unwrap();
        assert!(matches!(
            orchestrator.receive_frame(ping),
            Err(OrchestratorError::Connection(
                ConnectionStateError::InvalidTransition { .. }
            ))
        ));
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
    }

    #[test]
    fn initial_database_selector_is_required_before_ready() {
        let orchestrator = orchestrator(Some("tenant".to_owned()));
        assert_eq!(orchestrator.state(), ConnectionState::Ready);
    }

    #[test]
    fn ready_commands_are_dispatched_into_the_owned_write_queue() {
        let mut orchestrator = orchestrator(None);
        while let Some(front) = orchestrator.front_write() {
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        let ping =
            ClassicFrame::from_payload(CODEC, crate::COMMAND_SEQUENCE_ID, &[crate::COM_PING])
                .unwrap();
        assert_eq!(
            orchestrator.receive_frame(ping).unwrap(),
            OrchestratorEvent::Ready
        );
        let response = orchestrator.front_write().unwrap().to_vec();
        assert_eq!(CODEC.decode(&response).unwrap().sequence_id, 1);
        assert_eq!(response[4], crate::AUTH_OK_HEADER);
    }

    #[test]
    fn queue_write_errors_are_reported_without_accepting_partial_frames() {
        let mut orchestrator = ClassicConnectionOrchestrator::with_transport_security(
            settings(),
            TransportSecurity::Secure,
            CachingSha2Verifier::<crate::DefaultCredentialProvider>::default(),
            TestExecutor,
            1,
            1,
        )
        .unwrap();
        assert!(matches!(
            orchestrator.start(),
            Err(OrchestratorError::WriteQueue(_))
        ));
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        assert_eq!(orchestrator.front_write(), None);
    }

    #[test]
    fn zero_progress_write_closes_with_the_pending_frame_intact() {
        let mut orchestrator = orchestrator(None);
        assert_eq!(
            orchestrator.advance_write(0),
            Err(OrchestratorError::ZeroByteWrite)
        );
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        assert!(orchestrator.front_write().is_some());
    }
}
