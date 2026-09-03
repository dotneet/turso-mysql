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
    CommandDispatcher, CommandDispatcherError, CommandExecutionOptions, ConnectionState,
    ConnectionStateError, CredentialProvider, InitialHandshakeSettings, PacketCodec,
    PacketCodecError, PacketWriteQueue, PacketWriteQueueError, PendingAuthentication,
    TransportSecurity, CLIENT_SSL,
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
    /// Authentication succeeded but the one-shot executor factory was missing.
    ExecutorFactoryMissing,
    /// A ready command arrived without an executor installed after auth.
    ExecutorNotInstalled,
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
                .max(crate::MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH),
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
            Ok(event) => {
                if matches!(
                    event,
                    OrchestratorEvent::Closing | OrchestratorEvent::Closed
                ) {
                    self.clear_connection_material();
                }
                Ok(event)
            }
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
                let executor = self
                    .executor
                    .as_mut()
                    .ok_or(OrchestratorError::ExecutorNotInstalled)?;
                let frames = self
                    .dispatcher
                    .dispatch(&mut self.connection, executor, frame)?;
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

        assert!(matches!(
            orchestrator.receive_frame(ClassicFrame::from_payload(CODEC, 3, b"wrong\0").unwrap()),
            Err(OrchestratorError::Connection(
                ConnectionStateError::AuthenticationRejected
            ))
        ));
        assert_eq!(orchestrator.state(), ConnectionState::Closing);
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        assert!(orchestrator.executor.is_none());
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
