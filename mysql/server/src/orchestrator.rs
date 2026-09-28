//! A complete-frame owner for one classic MySQL connection.
//!
//! This module deliberately stops at the transport boundary. Callers provide
//! one complete classic packet at a time and own the socket, TLS engine, and
//! scheduling around this type. The orchestrator owns the protocol state,
//! verifier, command adapter, and bounded response queue.

use std::{error::Error, fmt};

use crate::{
    authorization_frontend_error, AuthenticatedCommandExecutor, AuthenticatedExecutorFactory,
    AuthenticatedPrincipal, AuthorizationError, CachingSha2Verifier, ClassicConnection,
    ClientSslRequest, CommandDispatcher, CommandDispatcherError, CommandExecutionOptions,
    ConnectionState, ConnectionStateError, CredentialProvider, InitialHandshakeSettings,
    PacketCodec, PacketCodecError, PacketWriteQueue, PacketWriteQueueError, PendingAuthentication,
    TransportSecurity, CLIENT_SSL,
};

/// One complete, owned client payload.
///
/// Construction from a packet validates the four-byte header, declared payload
/// length, and configured payload limit. A stream decoder belongs outside the
/// orchestrator; this type intentionally cannot represent a partial frame. A
/// command's payload may have arrived split into several packets, which the
/// reader has already joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassicFrame {
    sequence_id: u8,
    last_sequence_id: u8,
    payload: Vec<u8>,
}

impl ClassicFrame {
    /// Validates and owns one complete packet frame.
    pub fn new(codec: PacketCodec, mut bytes: Vec<u8>) -> Result<Self, PacketCodecError> {
        let sequence_id = codec.decode(&bytes)?.sequence_id;
        bytes.drain(..crate::PACKET_HEADER_LEN);
        Ok(Self {
            sequence_id,
            last_sequence_id: sequence_id,
            payload: bytes,
        })
    }

    /// Owns the payload of one packet after checking it against the codec.
    pub fn from_payload(
        codec: PacketCodec,
        sequence_id: u8,
        payload: &[u8],
    ) -> Result<Self, PacketCodecError> {
        if payload.len() > codec.max_payload_len() {
            return Err(PacketCodecError::PayloadTooLarge {
                length: payload.len(),
                limit: codec.max_payload_len(),
            });
        }
        Ok(Self {
            sequence_id,
            last_sequence_id: sequence_id,
            payload: payload.to_vec(),
        })
    }

    /// Owns a payload a reader joined from the packets numbered
    /// `sequence_id` through `last_sequence_id`.
    pub fn from_split_payload(sequence_id: u8, last_sequence_id: u8, payload: Vec<u8>) -> Self {
        Self {
            sequence_id,
            last_sequence_id,
            payload,
        }
    }

    /// Returns the sequence number of the first packet.
    pub const fn sequence_id(&self) -> u8 {
        self.sequence_id
    }

    /// Returns the sequence number the answer to this payload starts from.
    pub const fn response_sequence_id(&self) -> u8 {
        self.last_sequence_id.wrapping_add(1)
    }

    /// Returns the payload, joined when it arrived split.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Returns the packet a client sends before it has signed in, which is
    /// never split.
    fn sign_in_packet(&self) -> Vec<u8> {
        crate::encode_split_payload(self.sequence_id, &self.payload)
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
    /// Authentication succeeded but the one-shot executor factory was missing.
    ExecutorFactoryMissing,
    /// A ready command arrived without an executor installed after auth.
    ExecutorNotInstalled,
    /// A payload too long to take was reported before the client signed in.
    PacketTooLargeBeforeSignIn,
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
            Self::ExecutorFactoryMissing => {
                f.write_str("authenticated executor factory is missing")
            }
            Self::ExecutorNotInstalled => f.write_str("authenticated executor is not installed"),
            Self::PacketTooLargeBeforeSignIn => {
                f.write_str("a payload too long to take arrived before sign-in")
            }
        }
    }
}

impl Error for OrchestratorError {}

/// Owns all protocol-side state for one complete-frame classic connection.
///
/// `P` is the credential provider and `F` is a one-shot factory for the command
/// adapter. The factory is retained until authentication succeeds, so an
/// executor or database session cannot exist in the pre-authentication state.
pub struct ClassicConnectionOrchestrator<P, F>
where
    P: CredentialProvider,
    F: AuthenticatedExecutorFactory,
{
    connection: ClassicConnection,
    verifier: CachingSha2Verifier<P>,
    executor_factory: Option<F>,
    executor: Option<F::Executor>,
    dispatcher: CommandDispatcher,
    write_queue: PacketWriteQueue,
    pending_authentication: Option<PendingAuthentication>,
}

impl<P, F> fmt::Debug for ClassicConnectionOrchestrator<P, F>
where
    P: CredentialProvider,
    F: AuthenticatedExecutorFactory + fmt::Debug,
    F::Executor: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassicConnectionOrchestrator")
            .field("connection", &self.connection)
            .field("verifier", &self.verifier)
            .field(
                "executor_factory_installed",
                &self.executor_factory.is_some(),
            )
            .field("executor_installed", &self.executor.is_some())
            .field("write_queue", &self.write_queue)
            .finish()
    }
}

impl<P, F> ClassicConnectionOrchestrator<P, F>
where
    P: CredentialProvider,
    F: AuthenticatedExecutorFactory,
{
    /// Creates a plaintext-starting orchestrator with the standard bounded
    /// packet codec and a caller-selected response queue budget.
    pub fn new(
        settings: InitialHandshakeSettings,
        verifier: CachingSha2Verifier<P>,
        executor_factory: F,
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
            executor_factory,
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
        executor_factory: F,
        max_queued_bytes: usize,
        max_queued_frames: usize,
    ) -> Result<Self, OrchestratorError> {
        let codec = PacketCodec::new(
            crate::MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH
                .max(crate::MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH)
                .max(crate::MAX_SIGN_IN_PACKET_PAYLOAD_LENGTH),
        )?;
        let connection = ClassicConnection::with_codec(settings, codec, transport_security)?;
        let write_queue = PacketWriteQueue::new(codec, max_queued_bytes, max_queued_frames)
            .map_err(OrchestratorError::WriteQueue)?;
        Ok(Self {
            connection,
            verifier,
            executor_factory: Some(executor_factory),
            executor: None,
            dispatcher: CommandDispatcher::new(),
            write_queue,
            pending_authentication: None,
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
        executor_factory: F,
        write_queue: PacketWriteQueue,
    ) -> Self {
        Self {
            connection,
            verifier,
            executor_factory: Some(executor_factory),
            executor: None,
            dispatcher: CommandDispatcher::new(),
            write_queue,
            pending_authentication: None,
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
            | ConnectionState::SendAuthSwitchRequest
            | ConnectionState::AwaitAuthSwitchResponse
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
    /// before earlier output has drained. A sign-in packet that does not read
    /// as the one the client owed queues the error MySQL answers with before
    /// the error is returned, and the caller should flush it before closing.
    /// A credential or provider failure does not synthesize an error packet;
    /// already queued protocol output is left for the transport to flush or
    /// discard with [`Self::transport_closed`].
    pub fn receive_frame(
        &mut self,
        frame: ClassicFrame,
    ) -> Result<OrchestratorEvent, OrchestratorError> {
        let state = self.connection.state();
        let result = self.receive_frame_inner(&frame);
        match result {
            Ok(event) => {
                if matches!(
                    event,
                    OrchestratorEvent::Closing | OrchestratorEvent::Closed
                ) {
                    self.clear_connection_material();
                }
                Ok(event)
            }
            Err(error) => {
                if is_sign_in_state(state) && is_bad_handshake(&error) {
                    self.queue_sign_in_refusal(frame.response_sequence_id());
                }
                self.fail(error)
            }
        }
    }

    /// Answers a client whose payload would reach `max_allowed_packet`, and
    /// starts closing.
    ///
    /// Measured on MySQL 8.4.11: as soon as the header of the packet that
    /// takes a payload that far arrives, MySQL answers 1153 numbered after
    /// that packet and closes the connection, without reading the rest.
    /// `sequence_id` is the number of that packet.
    pub fn refuse_packet_too_large(
        &mut self,
        sequence_id: u8,
    ) -> Result<OrchestratorEvent, OrchestratorError> {
        if self.connection.state() != ConnectionState::Ready {
            return self.fail(OrchestratorError::PacketTooLargeBeforeSignIn);
        }
        let result = self.encode_error(
            crate::FrontendErrorKind::PacketTooLarge,
            sequence_id.wrapping_add(1),
        );
        let frame = match result {
            Ok(frame) => frame,
            Err(error) => return self.fail(error),
        };
        if let Err(error) = self.write_queue.enqueue_batch([frame]) {
            return self.fail(OrchestratorError::WriteQueue(error));
        }
        self.close()
    }

    /// Supplies the bounded SSLRequest that precedes an external TLS handshake.
    pub(crate) fn receive_ssl_request(
        &mut self,
        request: ClientSslRequest,
    ) -> Result<OrchestratorEvent, OrchestratorError> {
        match self.connection.receive_client_ssl_request(request) {
            Ok(()) => Ok(self.event()),
            Err(error) => self.fail(OrchestratorError::Connection(error)),
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
        self.clear_connection_material();
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
        self.clear_connection_material();
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

    /// Returns the idle time the authenticated session asked for in place of
    /// the runtime's own, if it asked for one.
    pub fn session_wait_timeout(&self) -> Option<std::time::Duration> {
        self.executor
            .as_ref()
            .and_then(crate::CommandExecutor::session_wait_timeout)
    }

    /// Returns how long the authenticated session asked a response to be
    /// given to be written, in place of the runtime's own, if it asked.
    pub fn session_net_write_timeout(&self) -> Option<std::time::Duration> {
        self.executor
            .as_ref()
            .and_then(crate::CommandExecutor::session_net_write_timeout)
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
        frame: &ClassicFrame,
    ) -> Result<OrchestratorEvent, OrchestratorError> {
        match self.connection.state() {
            ConnectionState::AwaitClientResponse | ConnectionState::TlsNegotiated => {
                self.connection
                    .receive_client_handshake_frame(&frame.sign_in_packet())?;
                if self.connection.state() == ConnectionState::TlsUpgradeRequired {
                    return Ok(self.event());
                }
                if self.connection.state() == ConnectionState::TlsNegotiated {
                    self.connection.begin_authentication()?;
                }
                if self.connection.state() == ConnectionState::SendAuthSwitchRequest {
                    let request = self.connection.send_auth_switch_request()?;
                    self.write_queue
                        .enqueue_batch([request])
                        .map_err(OrchestratorError::WriteQueue)?;
                    return Ok(self.event());
                }
                self.authenticate_initial()?;
            }
            ConnectionState::AwaitAuthSwitchResponse => {
                self.connection
                    .receive_auth_switch_response_frame(&frame.sign_in_packet())?;
                self.authenticate_initial()?;
            }
            ConnectionState::AuthenticateFull => {
                self.authenticate_full(&frame.sign_in_packet())?;
            }
            ConnectionState::Ready => {
                let executor = self
                    .executor
                    .as_mut()
                    .ok_or(OrchestratorError::ExecutorNotInstalled)?;
                let packet = crate::Packet {
                    sequence_id: frame.sequence_id(),
                    payload: frame.payload(),
                };
                let frames = self.dispatcher.dispatch_packet(
                    &mut self.connection,
                    executor,
                    packet,
                    frame.response_sequence_id(),
                )?;
                self.queue_answer(frames, frame.response_sequence_id())?;
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

    /// Queues an answer, or an error in its place when the answer is longer
    /// than the whole write queue holds.
    ///
    /// The queue is empty while a command runs, since each answer is written
    /// out before the next command is read, so an answer that does not fit
    /// now never will. Only a result set can be that long; answering 1235 for
    /// it, as for any other result this server will not send, keeps the
    /// connection where dropping it would leave the client with no reason.
    fn queue_answer(
        &mut self,
        frames: Vec<Vec<u8>>,
        response_sequence_id: u8,
    ) -> Result<(), OrchestratorError> {
        match self.write_queue.enqueue_batch(frames) {
            Ok(()) => Ok(()),
            Err(
                PacketWriteQueueError::ByteLimitExceeded { .. }
                | PacketWriteQueueError::FrameLimitExceeded { .. },
            ) if self.write_queue.queued_frames() == 0 => {
                let error =
                    self.encode_error(crate::FrontendErrorKind::Unsupported, response_sequence_id)?;
                self.write_queue
                    .enqueue_batch([error])
                    .map_err(OrchestratorError::WriteQueue)
            }
            Err(error) => Err(OrchestratorError::WriteQueue(error)),
        }
    }

    fn encode_error(
        &self,
        kind: crate::FrontendErrorKind,
        sequence_id: u8,
    ) -> Result<Vec<u8>, OrchestratorError> {
        let capabilities =
            self.connection
                .negotiated_capabilities()
                .ok_or(OrchestratorError::Dispatch(
                    CommandDispatcherError::NegotiatedCapabilitiesRequired,
                ))?;
        crate::map_frontend_error(kind)
            .encode(
                self.connection.response_packet_codec(),
                sequence_id,
                capabilities,
            )
            .map_err(|error| OrchestratorError::Dispatch(CommandDispatcherError::Response(error)))
    }

    /// Measured on MySQL 8.4.11: a packet that does not read as the handshake
    /// response or the answer the server asked for is answered 1043, `Bad
    /// handshake`, numbered after it, and the connection closed. A refusal
    /// that cannot be queued is dropped: the connection is closing anyway.
    fn queue_sign_in_refusal(&mut self, sequence_id: u8) {
        let capabilities = self
            .connection
            .negotiated_capabilities()
            .unwrap_or(crate::CLIENT_PROTOCOL_41);
        let Ok(frame) = crate::map_frontend_error(crate::FrontendErrorKind::BadHandshake).encode(
            self.connection.response_packet_codec(),
            sequence_id,
            capabilities,
        ) else {
            return;
        };
        let _ = self.write_queue.enqueue_batch([frame]);
    }

    fn authenticate_initial(&mut self) -> Result<(), OrchestratorError> {
        let verification = {
            let request = self.connection.authentication_verification_request()?;
            self.verifier
                .verify_initial_for_connection(&request)
                .map_err(ConnectionStateError::CredentialVerification)?
        };
        let crate::InitialAuthenticationVerification {
            result,
            pending,
            principal,
        } = verification;
        self.pending_authentication = pending;
        let principal = match result {
            crate::InitialAuthenticationResult::FastAuthSuccess => {
                Some(principal.expect("successful fast authentication must mint a principal"))
            }
            crate::InitialAuthenticationResult::FullAuthenticationRequired => {
                assert!(
                    principal.is_none(),
                    "full authentication must not mint a principal before the full response"
                );
                None
            }
            crate::InitialAuthenticationResult::Rejected => {
                assert!(
                    principal.is_none(),
                    "rejected authentication cannot mint a principal"
                );
                None
            }
        };
        if result == crate::InitialAuthenticationResult::FullAuthenticationRequired {
            assert!(
                self.pending_authentication.is_some(),
                "full authentication must retain its pending snapshot"
            );
        } else {
            assert!(
                self.pending_authentication.is_none(),
                "only full authentication may retain a pending snapshot"
            );
        }
        let auth_frame = self
            .connection
            .apply_initial_authentication_result(result)?;
        let mut frames = vec![auth_frame];
        if self.connection.state() == ConnectionState::AuthenticateFast {
            let principal = principal.expect("fast authentication must have a principal");
            match self.install_authenticated_executor(principal) {
                Ok(()) => {
                    let response = {
                        let executor = self
                            .executor
                            .as_mut()
                            .ok_or(OrchestratorError::ExecutorNotInstalled)?;
                        self.connection
                            .send_authentication_ok_with_selector(executor)?
                    };
                    match response {
                        crate::AuthenticationResponse::Ok(frame) => frames.push(frame),
                        crate::AuthenticationResponse::Err { frame, .. } => {
                            self.executor = None;
                            frames.push(frame);
                        }
                    }
                }
                Err(error) => {
                    let response = self
                        .connection
                        .authentication_error_response(authorization_frontend_error(error))?;
                    frames.push(response.frame().to_vec());
                }
            }
        }
        self.write_queue
            .enqueue_batch(frames)
            .map_err(OrchestratorError::WriteQueue)?;
        Ok(())
    }

    fn authenticate_full(&mut self, frame: &[u8]) -> Result<(), OrchestratorError> {
        let verification = {
            let request = self.connection.receive_full_authentication_frame(frame)?;
            let pending = self.pending_authentication.take().ok_or(
                ConnectionStateError::CredentialVerification(
                    crate::CredentialVerificationError::PendingAuthenticationMissing,
                ),
            )?;
            self.verifier
                .verify_full_for_connection(pending, &request)
                .map_err(ConnectionStateError::CredentialVerification)?
        };
        let crate::FullAuthenticationVerification { result, principal } = verification;
        if result != crate::FullAuthenticationResult::Authenticated {
            assert!(
                principal.is_none(),
                "rejected full authentication cannot mint a principal"
            );
        }
        let response = if result == crate::FullAuthenticationResult::Authenticated {
            let principal =
                principal.expect("successful full authentication must mint a principal");
            match self.install_authenticated_executor(principal) {
                Ok(()) => {
                    let executor = self
                        .executor
                        .as_mut()
                        .ok_or(OrchestratorError::ExecutorNotInstalled)?;
                    self.connection
                        .apply_full_authentication_result_with_selector(result, executor)?
                }
                Err(error) => self
                    .connection
                    .authentication_error_response(authorization_frontend_error(error))?,
            }
        } else {
            self.connection.reject_full_authentication()?
        };
        if response.error_kind().is_some() {
            self.executor = None;
        }
        self.write_queue
            .enqueue_batch([response.frame().to_vec()])
            .map_err(OrchestratorError::WriteQueue)
    }

    fn install_authenticated_executor(
        &mut self,
        principal: AuthenticatedPrincipal,
    ) -> Result<(), AuthorizationError> {
        let factory = self
            .executor_factory
            .take()
            .ok_or(AuthorizationError::Unavailable)?;
        let capabilities = self
            .connection
            .negotiated_capabilities()
            .ok_or(AuthorizationError::Unavailable)?;
        let options = CommandExecutionOptions::from_capability_flags(capabilities);
        let mut executor = factory.build_with_options(principal, options)?;
        executor.authorize_connection()?;
        self.executor = Some(executor);
        Ok(())
    }

    fn fail<T>(&mut self, error: OrchestratorError) -> Result<T, OrchestratorError> {
        self.clear_connection_material();
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

    fn clear_connection_material(&mut self) {
        self.pending_authentication = None;
        self.executor_factory = None;
        self.executor = None;
    }
}

fn is_sign_in_state(state: ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::AwaitClientResponse
            | ConnectionState::TlsNegotiated
            | ConnectionState::AwaitAuthSwitchResponse
            | ConnectionState::AuthenticateFull
    )
}

/// The errors that mean the client's packet was malformed or out of turn,
/// as opposed to a failure on this side, which closes without a word.
fn is_bad_handshake(error: &OrchestratorError) -> bool {
    matches!(
        error,
        OrchestratorError::Connection(
            ConnectionStateError::ClientHandshakeResponse(_)
                | ConnectionStateError::PacketCodec(_)
                | ConnectionStateError::AuthPacket(_)
                | ConnectionStateError::CapabilityNotAdvertised { .. }
                | ConnectionStateError::CapabilitiesChangedAfterTls { .. }
                | ConnectionStateError::UnexpectedSequenceId { .. }
                | ConnectionStateError::TlsRequestRequired
                | ConnectionStateError::AuthSwitchResponseTooLong { .. }
        )
    )
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
        AuthenticatedCommandExecutor, AuthenticatedExecutorFactory, ClientHandshakeResponseConfig,
        CommandExecutionResult, CommandExecutor, CommandOkResult, InitialDatabaseSelector,
        InitialHandshakeSettings, StoredCredential, AUTH_PLUGIN_DATA_LENGTH,
        CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_FOUND_ROWS, CLIENT_SSL, FAST_AUTH_RESPONSE_LENGTH,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };
    use sha2::{Digest, Sha256};
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    };

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
            sql: &str,
        ) -> Result<CommandExecutionResult, crate::FrontendErrorKind> {
            if sql == "SELECT twelve_rows" {
                return Ok(CommandExecutionResult::ResultSet(crate::TextResultSet {
                    columns: vec![crate::ColumnDefinitionConfig::new(
                        "value",
                        crate::MYSQL_TYPE_VAR_STRING,
                    )],
                    rows: vec![vec![Some(b"row".to_vec())]; 12],
                    warnings: 0,
                    status_flags: crate::SERVER_STATUS_AUTOCOMMIT,
                }));
            }
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
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            Ok(TestExecutor)
        }
    }

    #[derive(Debug)]
    struct OptionsRecordingFactory {
        options: Arc<Mutex<Option<CommandExecutionOptions>>>,
    }

    impl AuthenticatedExecutorFactory for OptionsRecordingFactory {
        type Executor = TestExecutor;

        fn build(
            self,
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            Ok(TestExecutor)
        }

        fn build_with_options(
            self,
            principal: AuthenticatedPrincipal,
            options: CommandExecutionOptions,
        ) -> Result<Self::Executor, AuthorizationError> {
            *self.options.lock().unwrap() = Some(options);
            self.build(principal)
        }
    }

    #[derive(Debug, Default)]
    struct RejectingDatabaseExecutor;

    impl CommandExecutor for RejectingDatabaseExecutor {
        fn execute_init_db(
            &mut self,
            _database: &str,
        ) -> Result<CommandExecutionResult, crate::FrontendErrorKind> {
            Err(crate::FrontendErrorKind::UnknownDatabase)
        }

        fn execute_query(
            &mut self,
            _sql: &str,
        ) -> Result<CommandExecutionResult, crate::FrontendErrorKind> {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }
    }

    impl InitialDatabaseSelector for RejectingDatabaseExecutor {
        fn select_initial_database(
            &mut self,
            _database: &str,
        ) -> Result<(), crate::FrontendErrorKind> {
            Err(crate::FrontendErrorKind::UnknownDatabase)
        }
    }

    impl AuthenticatedCommandExecutor for RejectingDatabaseExecutor {
        fn authorize_connection(&mut self) -> Result<(), AuthorizationError> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct RejectingDatabaseExecutorFactory;

    impl AuthenticatedExecutorFactory for RejectingDatabaseExecutorFactory {
        type Executor = RejectingDatabaseExecutor;

        fn build(
            self,
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            Ok(RejectingDatabaseExecutor)
        }
    }

    #[derive(Debug)]
    struct AuthorizationGateExecutor {
        result: AuthorizationError,
    }

    impl CommandExecutor for AuthorizationGateExecutor {
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

    impl InitialDatabaseSelector for AuthorizationGateExecutor {
        fn select_initial_database(
            &mut self,
            _database: &str,
        ) -> Result<(), crate::FrontendErrorKind> {
            Ok(())
        }
    }

    impl AuthenticatedCommandExecutor for AuthorizationGateExecutor {
        fn authorize_connection(&mut self) -> Result<(), AuthorizationError> {
            Err(self.result)
        }
    }

    #[derive(Debug)]
    struct AuthorizationGateFactory {
        result: AuthorizationError,
        builds: Arc<AtomicUsize>,
    }

    impl AuthenticatedExecutorFactory for AuthorizationGateFactory {
        type Executor = AuthorizationGateExecutor;

        fn build(
            self,
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            Ok(AuthorizationGateExecutor {
                result: self.result,
            })
        }
    }

    #[derive(Debug)]
    struct CountingFactory {
        builds: Arc<AtomicUsize>,
    }

    impl AuthenticatedExecutorFactory for CountingFactory {
        type Executor = TestExecutor;

        fn build(
            self,
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            Ok(TestExecutor)
        }
    }

    #[derive(Debug)]
    struct OrderedExecutor {
        events: Arc<Mutex<Vec<String>>>,
        authorization_result: Result<(), AuthorizationError>,
    }

    impl CommandExecutor for OrderedExecutor {
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

    impl InitialDatabaseSelector for OrderedExecutor {
        fn select_initial_database(
            &mut self,
            database: &str,
        ) -> Result<(), crate::FrontendErrorKind> {
            self.events
                .lock()
                .unwrap()
                .push(format!("select:{database}"));
            Ok(())
        }
    }

    impl AuthenticatedCommandExecutor for OrderedExecutor {
        fn authorize_connection(&mut self) -> Result<(), AuthorizationError> {
            self.events.lock().unwrap().push("connect".to_owned());
            self.authorization_result
        }
    }

    #[derive(Debug)]
    struct OrderedFactory {
        events: Arc<Mutex<Vec<String>>>,
        authorization_result: Result<(), AuthorizationError>,
    }

    impl AuthenticatedExecutorFactory for OrderedFactory {
        type Executor = OrderedExecutor;

        fn build(
            self,
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            self.events.lock().unwrap().push("build".to_owned());
            Ok(OrderedExecutor {
                events: self.events,
                authorization_result: self.authorization_result,
            })
        }
    }

    #[derive(Debug)]
    struct BuildFailingFactory {
        result: AuthorizationError,
        builds: Arc<AtomicUsize>,
    }

    impl AuthenticatedExecutorFactory for BuildFailingFactory {
        type Executor = TestExecutor;

        fn build(
            self,
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            Err(self.result)
        }
    }

    #[derive(Debug)]
    struct DropRecordingExecutor {
        drops: Arc<AtomicUsize>,
    }

    impl Drop for DropRecordingExecutor {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl CommandExecutor for DropRecordingExecutor {
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

    impl InitialDatabaseSelector for DropRecordingExecutor {
        fn select_initial_database(
            &mut self,
            _database: &str,
        ) -> Result<(), crate::FrontendErrorKind> {
            Ok(())
        }
    }

    impl AuthenticatedCommandExecutor for DropRecordingExecutor {
        fn authorize_connection(&mut self) -> Result<(), AuthorizationError> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct DropRecordingFactory {
        drops: Arc<AtomicUsize>,
    }

    impl AuthenticatedExecutorFactory for DropRecordingFactory {
        type Executor = DropRecordingExecutor;

        fn build(
            self,
            _principal: AuthenticatedPrincipal,
        ) -> Result<Self::Executor, AuthorizationError> {
            Ok(DropRecordingExecutor { drops: self.drops })
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
        client_response_with_capabilities(auth_response, database, 0)
    }

    fn client_response_with_capabilities(
        auth_response: Vec<u8>,
        database: Option<String>,
        extra_capabilities: u32,
    ) -> ClassicFrame {
        ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                | database
                    .as_ref()
                    .map_or(0, |_| crate::CLIENT_CONNECT_WITH_DB)
                | extra_capabilities,
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
    ) -> ClassicConnectionOrchestrator<crate::InMemoryCredentialProvider, TestExecutorFactory> {
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
            TestExecutorFactory,
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
    fn authenticated_factory_receives_negotiated_found_rows_option() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let mut connection_settings = settings();
        connection_settings.capability_flags |= CLIENT_FOUND_ROWS;
        let connection = ClassicConnection::with_test_nonce(
            connection_settings,
            CODEC,
            TransportSecurity::Secure,
            SCRAMBLE,
        )
        .unwrap();
        let queue = PacketWriteQueue::new(CODEC, 128, 8).unwrap();
        let observed = Arc::new(Mutex::new(None));
        let mut orchestrator = ClassicConnectionOrchestrator::from_parts(
            connection,
            CachingSha2Verifier::new(provider),
            OptionsRecordingFactory {
                options: Arc::clone(&observed),
            },
            queue,
        );

        assert_eq!(
            orchestrator.start().unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        orchestrator
            .receive_frame(client_response_with_capabilities(
                fast_response(password).to_vec(),
                None,
                CLIENT_FOUND_ROWS,
            ))
            .unwrap();

        assert_eq!(
            observed.lock().unwrap().as_ref().copied(),
            Some(CommandExecutionOptions::from_capability_flags(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_FOUND_ROWS,
            ))
        );
        assert!(observed.lock().unwrap().unwrap().client_found_rows());
    }

    fn drop_recording_orchestrator(
        drops: Arc<AtomicUsize>,
    ) -> ClassicConnectionOrchestrator<crate::InMemoryCredentialProvider, DropRecordingFactory>
    {
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
            DropRecordingFactory { drops },
            queue,
        );
        orchestrator.start().unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(client_response(fast_response(password).to_vec(), None))
                .unwrap(),
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
    fn connect_authorization_denial_fast_auth_queues_fixed_access_denied() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
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
            AuthorizationGateFactory {
                result: AuthorizationError::Denied,
                builds: builds.clone(),
            },
            queue,
        );
        orchestrator.start().unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(client_response(fast_response(password).to_vec(), None))
                .unwrap(),
            OrchestratorEvent::Closing
        );
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        assert!(orchestrator.executor.is_none());
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        let mut frames = Vec::new();
        while let Some(front) = orchestrator.front_write() {
            frames.push(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        let error = crate::ErrPacket::decode(
            CODEC,
            frames
                .last()
                .expect("authorization must queue an ERR frame"),
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap();
        assert_eq!(error.error_code, 1045);
        assert_eq!(error.sql_state, Some(*b"28000"));
        assert_eq!(error.message, b"access denied");
    }

    #[test]
    fn connect_authorization_unavailability_full_auth_never_queues_final_ok() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(password)),
            )
            .unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
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
            AuthorizationGateFactory {
                result: AuthorizationError::Unavailable,
                builds: builds.clone(),
            },
            queue,
        );
        orchestrator.start().unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(client_response(vec![0; FAST_AUTH_RESPONSE_LENGTH], None))
                .unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        assert!(orchestrator.executor.is_none());
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        assert_eq!(orchestrator.state(), ConnectionState::AuthenticateFull);

        assert_eq!(
            orchestrator
                .receive_frame(ClassicFrame::from_payload(CODEC, 3, b"secret\0").unwrap())
                .unwrap(),
            OrchestratorEvent::Closing
        );
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        assert!(orchestrator.executor.is_none());
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        let mut frames = Vec::new();
        while let Some(front) = orchestrator.front_write() {
            frames.push(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        let error = crate::ErrPacket::decode(
            CODEC,
            frames
                .last()
                .expect("authorization must queue an ERR frame"),
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap();
        assert_eq!(error.error_code, 1045);
        assert_eq!(error.sql_state, Some(*b"28000"));
        assert_eq!(error.message, b"access denied");
        assert!(!frames.iter().any(|frame| {
            crate::PacketCodec::decode(CODEC, frame)
                .map(|packet| packet.payload.first() == Some(&crate::AUTH_OK_HEADER))
                .unwrap_or(false)
        }));
    }

    #[test]
    fn executor_factory_runs_once_after_authentication_and_not_before() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(password)),
            )
            .unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
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
            CountingFactory {
                builds: builds.clone(),
            },
            queue,
        );
        assert!(orchestrator.executor_factory.is_some());
        assert!(orchestrator.executor.is_none());
        orchestrator.start().unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(client_response(vec![0; FAST_AUTH_RESPONSE_LENGTH], None))
                .unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        assert!(orchestrator.executor.is_none());
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        orchestrator
            .receive_frame(ClassicFrame::from_payload(CODEC, 3, b"secret\0").unwrap())
            .unwrap();
        assert_eq!(orchestrator.state(), ConnectionState::Ready);
        assert!(orchestrator.executor.is_some());
        assert!(orchestrator.executor_factory.is_none());
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_fast_auth_builds_the_executor_once_after_authentication() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
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
            CountingFactory {
                builds: builds.clone(),
            },
            queue,
        );
        orchestrator.start().unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 0);

        assert_eq!(
            orchestrator
                .receive_frame(client_response(fast_response(password).to_vec(), None))
                .unwrap(),
            OrchestratorEvent::Ready
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(orchestrator.executor.is_some());
        assert!(orchestrator.executor_factory.is_none());
    }

    #[test]
    fn rejected_full_authentication_never_builds_an_executor() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(password)),
            )
            .unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
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
            CountingFactory {
                builds: builds.clone(),
            },
            queue,
        );
        orchestrator.start().unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(client_response(vec![0; FAST_AUTH_RESPONSE_LENGTH], None))
                .unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        assert_eq!(builds.load(Ordering::SeqCst), 0);

        drain(&mut orchestrator);
        assert_eq!(
            orchestrator
                .receive_frame(ClassicFrame::from_payload(CODEC, 3, b"wrong\0").unwrap())
                .unwrap(),
            OrchestratorEvent::Closing
        );
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        assert!(orchestrator.executor.is_none());
        let refusal = drain(&mut orchestrator);
        assert_eq!(refusal.len(), 1);
        let refusal =
            crate::ErrPacket::decode(CODEC, &refusal[0], crate::CLIENT_PROTOCOL_41).unwrap();
        assert_eq!((refusal.sequence_id, refusal.error_code), (4, 1045));
    }

    /// Measured on MySQL 8.4.11: a response after TLS that does not read as
    /// one is answered 1043 `Bad handshake`, numbered after it.
    #[test]
    fn a_malformed_response_after_tls_is_answered_bad_handshake() {
        let mut orchestrator = orchestrator_after_tls(
            StoredCredential::from_sha256_sha256(true, verifier_material(b"secret")),
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
        );
        assert!(orchestrator
            .receive_frame(ClassicFrame::from_payload(CODEC, 2, &[1, 2, 3]).unwrap())
            .is_err());
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        let refusal = drain(&mut orchestrator);
        assert_eq!(refusal.len(), 1);
        let refusal =
            crate::ErrPacket::decode(CODEC, &refusal[0], crate::CLIENT_PROTOCOL_41).unwrap();
        assert_eq!(
            (refusal.sequence_id, refusal.error_code, refusal.sql_state),
            (3, 1043, Some(*b"08S01"))
        );
    }

    #[test]
    fn executor_factory_failure_queues_fixed_access_denied_without_final_ok() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
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
            BuildFailingFactory {
                result: AuthorizationError::Unavailable,
                builds: builds.clone(),
            },
            queue,
        );
        orchestrator.start().unwrap();

        assert_eq!(
            orchestrator
                .receive_frame(client_response(fast_response(password).to_vec(), None))
                .unwrap(),
            OrchestratorEvent::Closing
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(orchestrator.executor.is_none());

        let mut frames = Vec::new();
        while let Some(front) = orchestrator.front_write() {
            frames.push(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        assert!(!frames.iter().any(|frame| {
            crate::PacketCodec::decode(CODEC, frame)
                .map(|packet| packet.payload.first() == Some(&crate::AUTH_OK_HEADER))
                .unwrap_or(false)
        }));
        let error = crate::ErrPacket::decode(
            CODEC,
            frames
                .last()
                .expect("factory failure must queue an ERR frame"),
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap();
        assert_eq!(error.error_code, 1045);
        assert_eq!(error.sql_state, Some(*b"28000"));
        assert_eq!(error.message, b"access denied");
    }

    #[test]
    fn initial_database_authorization_precedes_selection_and_final_ok() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
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
            OrderedFactory {
                events: events.clone(),
                authorization_result: Ok(()),
            },
            queue,
        );
        orchestrator.start().unwrap();

        assert_eq!(
            orchestrator
                .receive_frame(client_response(
                    fast_response(password).to_vec(),
                    Some("tenant".to_owned()),
                ))
                .unwrap(),
            OrchestratorEvent::Ready
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec!["build", "connect", "select:tenant"]
        );
        let mut last_frame = None;
        while let Some(front) = orchestrator.front_write() {
            last_frame = Some(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        let last_frame = last_frame.expect("successful authentication must queue final OK");
        assert_eq!(last_frame[4], crate::AUTH_OK_HEADER);
    }

    #[test]
    fn denied_connection_authorization_skips_initial_selection_and_final_ok() {
        let password = b"secret";
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
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
            OrderedFactory {
                events: events.clone(),
                authorization_result: Err(AuthorizationError::Denied),
            },
            queue,
        );
        orchestrator.start().unwrap();

        assert_eq!(
            orchestrator
                .receive_frame(client_response(
                    fast_response(password).to_vec(),
                    Some("tenant".to_owned()),
                ))
                .unwrap(),
            OrchestratorEvent::Closing
        );
        assert_eq!(*events.lock().unwrap(), vec!["build", "connect"]);
        assert!(orchestrator.executor.is_none());

        let mut frames = Vec::new();
        while let Some(front) = orchestrator.front_write() {
            frames.push(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        assert!(!frames.iter().any(|frame| {
            crate::PacketCodec::decode(CODEC, frame)
                .map(|packet| packet.payload.first() == Some(&crate::AUTH_OK_HEADER))
                .unwrap_or(false)
        }));
        let error = crate::ErrPacket::decode(
            CODEC,
            frames
                .last()
                .expect("authorization denial must queue an ERR frame"),
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap();
        assert_eq!(error.error_code, 1045);
        assert_eq!(error.sql_state, Some(*b"28000"));
        assert_eq!(error.message, b"access denied");
    }

    #[test]
    fn public_constructor_requires_the_tls_upgrade_capability() {
        let result = ClassicConnectionOrchestrator::new(
            InitialHandshakeSettings::default(),
            CachingSha2Verifier::<crate::DefaultCredentialProvider>::default(),
            TestExecutorFactory,
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
            TestExecutorFactory,
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
    fn complete_frame_auth_uses_one_snapshot_and_retains_the_principal() {
        #[derive(Clone)]
        struct ChangingProvider {
            lookups: Arc<AtomicUsize>,
            changed: Arc<AtomicBool>,
        }

        impl crate::CredentialProvider for ChangingProvider {
            fn lookup(
                &self,
                _username: &str,
            ) -> Result<Option<crate::CredentialSnapshot>, crate::CredentialProviderError>
            {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                if self.changed.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                Ok(Some(crate::CredentialSnapshot::new(
                    crate::AccountId::from_bytes([0x4a; 32]),
                    StoredCredential::from_full_verifier(true, verifier_material(b"secret")),
                )))
            }
        }

        let lookups = Arc::new(AtomicUsize::new(0));
        let changed = Arc::new(AtomicBool::new(false));
        let provider = ChangingProvider {
            lookups: lookups.clone(),
            changed: changed.clone(),
        };
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
            TestExecutorFactory,
            queue,
        );
        orchestrator.start().unwrap();
        assert_eq!(
            orchestrator
                .receive_frame(client_response(vec![0; FAST_AUTH_RESPONSE_LENGTH], None,))
                .unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        assert_eq!(orchestrator.state(), ConnectionState::AuthenticateFull);
        changed.store(true, Ordering::SeqCst);
        orchestrator
            .receive_frame(ClassicFrame::from_payload(CODEC, 3, b"secret\0").unwrap())
            .unwrap();
        assert_eq!(orchestrator.state(), ConnectionState::Ready);
        assert!(orchestrator.executor.is_some());
        assert_eq!(lookups.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ssl_request_is_an_external_upgrade_event_and_transport_close_is_idempotent() {
        let mut orchestrator = ClassicConnectionOrchestrator::with_transport_security(
            settings(),
            TransportSecurity::Plaintext,
            CachingSha2Verifier::<crate::DefaultCredentialProvider>::default(),
            TestExecutorFactory,
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
            TestExecutorFactory,
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

    /// MySqlConnector's handshake, as the framework harness logged it: the
    /// SSLRequest announces 0x011b8a02 and the response after TLS 0x011b8202,
    /// the same word without CLIENT_SSL, answering for
    /// `mysql_native_password`. Measured on MySQL 8.4.11, the server takes the
    /// word and sends an AuthSwitchRequest for `caching_sha2_password` with
    /// the handshake's scramble, numbered 3; the client's answer is 4, and the
    /// fast-auth byte and OK follow as 5 and 6.
    const MYSQLCONNECTOR_SSL_REQUEST_CAPABILITIES: u32 = 0x011b_8a02;
    const MYSQLCONNECTOR_RESPONSE_CAPABILITIES: u32 = 0x011b_8202;

    #[test]
    fn mysqlconnector_is_switched_to_caching_sha2_after_tls_without_client_ssl() {
        let password = b"secret";
        let mut orchestrator = orchestrator_after_tls(
            StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            MYSQLCONNECTOR_SSL_REQUEST_CAPABILITIES,
        );
        assert_eq!(
            orchestrator
                .receive_frame(mysql_native_password_response(
                    MYSQLCONNECTOR_RESPONSE_CAPABILITIES
                ))
                .unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        let switch = drain(&mut orchestrator);
        assert_eq!(switch.len(), 1);
        let switch = CODEC.decode(&switch[0]).unwrap();
        assert_eq!(switch.sequence_id, 3);
        let mut expected = b"\xfecaching_sha2_password\0".to_vec();
        expected.extend_from_slice(&SCRAMBLE);
        expected.push(0);
        assert_eq!(switch.payload, expected);

        assert_eq!(
            orchestrator
                .receive_frame(
                    ClassicFrame::from_payload(CODEC, 4, &fast_response(password)).unwrap()
                )
                .unwrap(),
            OrchestratorEvent::Ready
        );
        let frames = drain(&mut orchestrator);
        assert_eq!(frames.len(), 2);
        let fast = CODEC.decode(&frames[0]).unwrap();
        assert_eq!((fast.sequence_id, fast.payload), (5, &[0x01, 0x03][..]));
        let ok = CODEC.decode(&frames[1]).unwrap();
        assert_eq!((ok.sequence_id, ok.payload[0]), (6, 0x00));
        assert_eq!(
            orchestrator.connection.negotiated_capabilities().unwrap() & CLIENT_SSL,
            CLIENT_SSL
        );
    }

    #[test]
    fn a_switched_client_without_a_cached_verifier_gives_its_password_after_tls() {
        let password = b"secret";
        let mut orchestrator = orchestrator_after_tls(
            StoredCredential::from_full_verifier(true, verifier_material(password)),
            MYSQLCONNECTOR_SSL_REQUEST_CAPABILITIES,
        );
        orchestrator
            .receive_frame(mysql_native_password_response(
                MYSQLCONNECTOR_RESPONSE_CAPABILITIES,
            ))
            .unwrap();
        drain(&mut orchestrator);
        assert_eq!(
            orchestrator
                .receive_frame(
                    ClassicFrame::from_payload(CODEC, 4, &fast_response(password)).unwrap()
                )
                .unwrap(),
            OrchestratorEvent::AwaitingClientFrame
        );
        let full = drain(&mut orchestrator);
        let full = CODEC.decode(&full[0]).unwrap();
        assert_eq!((full.sequence_id, full.payload), (5, &[0x01, 0x04][..]));
        assert_eq!(
            orchestrator
                .receive_frame(ClassicFrame::from_payload(CODEC, 6, b"secret\0").unwrap())
                .unwrap(),
            OrchestratorEvent::Ready
        );
        let ok = drain(&mut orchestrator);
        let ok = CODEC.decode(&ok[0]).unwrap();
        assert_eq!((ok.sequence_id, ok.payload[0]), (7, 0x00));
    }

    #[test]
    fn a_response_after_tls_may_drop_only_client_ssl_from_the_ssl_request() {
        let mut orchestrator = orchestrator_after_tls(
            StoredCredential::from_sha256_sha256(true, verifier_material(b"secret")),
            MYSQLCONNECTOR_SSL_REQUEST_CAPABILITIES,
        );
        let changed = MYSQLCONNECTOR_RESPONSE_CAPABILITIES & !crate::CLIENT_MULTI_STATEMENTS;
        assert!(matches!(
            orchestrator.receive_frame(mysql_native_password_response(changed)),
            Err(OrchestratorError::Connection(
                ConnectionStateError::CapabilitiesChangedAfterTls { .. }
            ))
        ));
    }

    #[test]
    fn a_switch_answer_numbered_out_of_turn_is_refused() {
        let mut orchestrator = orchestrator_after_tls(
            StoredCredential::from_sha256_sha256(true, verifier_material(b"secret")),
            MYSQLCONNECTOR_SSL_REQUEST_CAPABILITIES,
        );
        orchestrator
            .receive_frame(mysql_native_password_response(
                MYSQLCONNECTOR_RESPONSE_CAPABILITIES,
            ))
            .unwrap();
        assert!(matches!(
            orchestrator.receive_frame(
                ClassicFrame::from_payload(CODEC, 3, &fast_response(b"secret")).unwrap()
            ),
            Err(OrchestratorError::Connection(
                ConnectionStateError::UnexpectedSequenceId {
                    expected: 4,
                    actual: 3,
                    ..
                }
            ))
        ));
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
    }

    /// Over a transport secure from the start there is no SSLRequest, so the
    /// response is 1 and the switch 2, as measured on MySQL 8.4.11 over a
    /// plain connection.
    #[test]
    fn a_client_on_a_secure_socket_is_switched_right_after_its_response() {
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
        let queue = PacketWriteQueue::new(CODEC, 256, 8).unwrap();
        let mut orchestrator = ClassicConnectionOrchestrator::from_parts(
            connection,
            CachingSha2Verifier::new(provider),
            TestExecutorFactory,
            queue,
        );
        orchestrator.start().unwrap();
        drain(&mut orchestrator);
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            0,
            crate::DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0x33; 20],
            None::<String>,
            Some("mysql_native_password"),
            None,
        )
        .encode(CODEC, 1)
        .unwrap();
        orchestrator
            .receive_frame(ClassicFrame::new(CODEC, response).unwrap())
            .unwrap();
        let switch = drain(&mut orchestrator);
        assert_eq!(CODEC.decode(&switch[0]).unwrap().sequence_id, 2);
        assert_eq!(
            orchestrator
                .receive_frame(
                    ClassicFrame::from_payload(CODEC, 3, &fast_response(password)).unwrap()
                )
                .unwrap(),
            OrchestratorEvent::Ready
        );
        let frames = drain(&mut orchestrator);
        assert_eq!(CODEC.decode(&frames[1]).unwrap().sequence_id, 5);
    }

    fn orchestrator_after_tls(
        credential: StoredCredential,
        ssl_request_capabilities: u32,
    ) -> ClassicConnectionOrchestrator<crate::InMemoryCredentialProvider, TestExecutorFactory> {
        let mut provider = crate::InMemoryCredentialProvider::new();
        provider.insert("root", credential).unwrap();
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
            TestExecutorFactory,
            queue,
        );
        orchestrator.start().unwrap();
        drain(&mut orchestrator);
        let ssl = crate::ClientSslRequestConfig::new(
            ssl_request_capabilities,
            0x00ff_ffff,
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
        orchestrator
    }

    fn mysql_native_password_response(capabilities: u32) -> ClassicFrame {
        let response = ClientHandshakeResponseConfig::new(
            capabilities,
            0x00ff_ffff,
            crate::DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0x33; 20],
            None::<String>,
            Some("mysql_native_password"),
            Some(vec![(
                "_client_name".to_owned(),
                "MySqlConnector".to_owned(),
            )]),
        )
        .encode(CODEC, 2)
        .unwrap();
        ClassicFrame::new(CODEC, response).unwrap()
    }

    fn drain<P: crate::CredentialProvider, F: AuthenticatedExecutorFactory>(
        orchestrator: &mut ClassicConnectionOrchestrator<P, F>,
    ) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some(front) = orchestrator.front_write() {
            frames.push(front.to_vec());
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        frames
    }

    #[test]
    fn quit_closes_without_queuing_a_response_and_rejects_follow_up_frames() {
        let mut orchestrator = orchestrator(None);
        assert!(orchestrator.executor.is_some());
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
        assert!(orchestrator.executor.is_none());
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
    fn close_drops_the_authenticated_executor_immediately() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut orchestrator = drop_recording_orchestrator(drops.clone());
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        assert_eq!(orchestrator.close().unwrap(), OrchestratorEvent::Closing);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(orchestrator.executor.is_none());
    }

    #[test]
    fn transport_close_drops_the_authenticated_executor_immediately() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut orchestrator = drop_recording_orchestrator(drops.clone());
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        assert_eq!(
            orchestrator.transport_closed().unwrap(),
            OrchestratorEvent::Closed
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(orchestrator.executor.is_none());
    }

    #[test]
    fn initial_database_selector_is_required_before_ready() {
        let orchestrator = orchestrator(Some("tenant".to_owned()));
        assert_eq!(orchestrator.state(), ConnectionState::Ready);
    }

    #[test]
    fn initial_database_error_clears_the_authenticated_principal() {
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
        let queue = PacketWriteQueue::new(CODEC, 256, 8).unwrap();
        let mut orchestrator = ClassicConnectionOrchestrator::from_parts(
            connection,
            CachingSha2Verifier::new(provider),
            RejectingDatabaseExecutorFactory,
            queue,
        );
        orchestrator.start().unwrap();

        assert_eq!(
            orchestrator
                .receive_frame(client_response(
                    fast_response(password).to_vec(),
                    Some("missing".to_owned()),
                ))
                .unwrap(),
            OrchestratorEvent::Closing
        );
        assert!(orchestrator.executor.is_none());
        assert!(orchestrator.pending_authentication.is_none());
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
    fn a_result_longer_than_the_write_queue_is_answered_with_an_error() {
        let mut orchestrator = orchestrator(None);
        while let Some(front) = orchestrator.front_write() {
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        let mut query = vec![crate::COM_QUERY];
        query.extend_from_slice(b"SELECT twelve_rows");
        let query = ClassicFrame::from_payload(CODEC, crate::COMMAND_SEQUENCE_ID, &query).unwrap();
        assert_eq!(
            orchestrator.receive_frame(query).unwrap(),
            OrchestratorEvent::Ready
        );
        let response = orchestrator.front_write().unwrap().to_vec();
        let error = crate::ErrPacket::decode(CODEC, &response, crate::CLIENT_PROTOCOL_41).unwrap();
        assert_eq!(error.sequence_id, 1);
        assert_eq!(error.error_code, 1235);
        let len = response.len();
        orchestrator.advance_write(len).unwrap();
        assert_eq!(orchestrator.front_write(), None);
    }

    #[test]
    fn a_command_reaching_max_allowed_packet_is_answered_1153_and_closes() {
        let mut orchestrator = orchestrator(None);
        while let Some(front) = orchestrator.front_write() {
            let len = front.len();
            orchestrator.advance_write(len).unwrap();
        }
        assert_eq!(
            orchestrator.refuse_packet_too_large(4).unwrap(),
            OrchestratorEvent::Closing
        );
        let response = orchestrator.front_write().unwrap().to_vec();
        let error = crate::ErrPacket::decode(CODEC, &response, crate::CLIENT_PROTOCOL_41).unwrap();
        assert_eq!(error.sequence_id, 5);
        assert_eq!(error.error_code, 1153);
        assert_eq!(error.sql_state, Some(*b"08S01"));
        assert_eq!(
            error.message,
            b"Got a packet bigger than 'max_allowed_packet' bytes"
        );
    }

    #[test]
    fn a_payload_too_long_before_sign_in_closes_without_an_answer() {
        let mut orchestrator = ClassicConnectionOrchestrator::with_transport_security(
            settings(),
            TransportSecurity::Secure,
            CachingSha2Verifier::<crate::DefaultCredentialProvider>::default(),
            TestExecutorFactory,
            4096,
            8,
        )
        .unwrap();
        orchestrator.start().unwrap();
        let greeting = orchestrator.front_write().unwrap().len();
        orchestrator.advance_write(greeting).unwrap();
        assert_eq!(
            orchestrator.refuse_packet_too_large(1),
            Err(OrchestratorError::PacketTooLargeBeforeSignIn)
        );
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        assert_eq!(orchestrator.front_write(), None);
    }

    #[test]
    fn queue_write_errors_are_reported_without_accepting_partial_frames() {
        let mut orchestrator = ClassicConnectionOrchestrator::with_transport_security(
            settings(),
            TransportSecurity::Secure,
            CachingSha2Verifier::<crate::DefaultCredentialProvider>::default(),
            TestExecutorFactory,
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
