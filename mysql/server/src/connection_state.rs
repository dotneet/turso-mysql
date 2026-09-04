//! Explicit state transitions for one classic MySQL connection.
//!
//! This module deliberately stops at the boundary between protocol events and
//! the code that owns transport or credentials. TLS completion and successful
//! authentication are external events; no socket, crypto, or credential store
//! is hidden behind this state machine.

use std::{error::Error, fmt};

use crate::{
    map_frontend_error, AuthMoreData, AuthMoreDataKind, AuthOkPacketConfig, AuthPacketError,
    ClientAuthResponse, ClientHandshakeResponse, ClientHandshakeResponseError, ClientSslRequest,
    ClientSslRequestError, CredentialVerificationError, FrontendErrorKind, HandshakeNonceSource,
    InitialHandshakeConfig, InitialHandshakeError, InitialHandshakeNonceError,
    InitialHandshakeSettings, OsHandshakeNonceSource, Packet, PacketCodec, PacketCodecError,
    ResponsePacketError, AUTH_PLUGIN_DATA_LENGTH, CLIENT_SSL, CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
    MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
    MIN_SERVER_RESPONSE_PAYLOAD_LENGTH,
};

#[cfg(test)]
use crate::{CachingSha2Verifier, CredentialProvider};

/// The authentication plugin implemented by this state machine.
pub const CACHING_SHA2_PASSWORD_PLUGIN: &str = "caching_sha2_password";

/// The maximum payload accepted by the command decoder.
pub const MAX_COMMAND_PAYLOAD_LENGTH: usize = 4096;
/// The sequence number that starts each classic command packet.
pub const COMMAND_SEQUENCE_ID: u8 = 0;
/// Sequence number of an ordinary client handshake response or SSLRequest.
pub const CLIENT_HANDSHAKE_SEQUENCE_ID: u8 = 1;
/// Sequence number of the client response sent after TLS negotiation.
pub const TLS_CLIENT_HANDSHAKE_SEQUENCE_ID: u8 = 2;

/// Classic command identifier for a text query.
pub const COM_QUERY: u8 = 0x03;
/// Classic command identifier for selecting the default database.
pub const COM_INIT_DB: u8 = 0x02;
/// Classic command identifier for a connection ping.
pub const COM_PING: u8 = 0x0e;
/// Classic command identifier for closing the connection.
pub const COM_QUIT: u8 = 0x01;
/// Classic command identifier for prepared-statement creation.
pub const COM_STMT_PREPARE: u8 = 0x16;
/// Classic command identifier for prepared-statement execution.
pub const COM_STMT_EXECUTE: u8 = 0x17;
/// Classic command identifier for prepared-statement long data.
pub const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
/// Classic command identifier for prepared-statement close.
pub const COM_STMT_CLOSE: u8 = 0x19;
/// Classic command identifier for prepared-statement reset.
pub const COM_STMT_RESET: u8 = 0x1a;
/// Cursor mode that does not request a server-side cursor.
pub const CURSOR_TYPE_NO_CURSOR: u8 = 0;

const STMT_EXECUTE_FIXED_BODY_LENGTH: usize = 4 + 1 + 4;

/// A command decoded from one bounded classic protocol packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassicCommand<'a> {
    /// A text query. Execution belongs to a higher layer.
    Query { sql: &'a str },
    /// A request to create a server-side prepared statement.
    StmtPrepare { sql: &'a str },
    /// A request to execute a server-side prepared statement.
    ///
    /// The parameter payload remains borrowed and opaque. Its layout depends
    /// on the parameter count and types retained by the statement registry.
    StmtExecute {
        /// Connection-local identifier of the prepared statement.
        statement_id: u32,
        /// Cursor mode requested by the client.
        flags: u8,
        /// Number of executions requested by the client.
        iteration_count: u32,
        /// Unparsed parameter-binding bytes following the fixed header.
        parameter_payload: &'a [u8],
    },
    /// A request to close a server-side prepared statement.
    StmtClose { statement_id: u32 },
    /// A request to reset a server-side prepared statement.
    StmtReset { statement_id: u32 },
    /// A request to select the connection's default database.
    InitDb { database: &'a str },
    /// A connection liveness check.
    Ping,
    /// A request to close the connection.
    Quit,
}

/// A decoded command packet, including its original sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassicCommandPacket<'a> {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Decoded command and its borrowed text, when applicable.
    pub command: ClassicCommand<'a>,
}

/// Whether the caller has already established a secure transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSecurity {
    /// The transport is already protected, for example a local socket or a
    /// channel whose TLS setup happened before this state machine was created.
    Secure,
    /// The transport is not protected yet. A client requesting `CLIENT_SSL`
    /// must complete the explicit TLS-upgrade transition before authentication.
    Plaintext,
}

impl Default for TransportSecurity {
    fn default() -> Self {
        Self::Plaintext
    }
}

/// The protocol phase of one classic MySQL connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// The server has not emitted its initial handshake yet.
    SendInitialHandshake,
    /// The initial handshake was emitted and a client response is expected.
    AwaitClientResponse,
    /// The client requested TLS and the external TLS handshake must complete.
    TlsUpgradeRequired,
    /// TLS completed; authentication may now be started.
    TlsNegotiated,
    /// The server is waiting for an external verified-authentication event.
    AuthenticateCachingSha2Password,
    /// The cached verifier accepted the response and the server must send OK.
    AuthenticateFast,
    /// The server requested full authentication and awaits the client response.
    AuthenticateFull,
    /// A full response is borrowed by an external verifier.
    AuthenticateFullVerification,
    /// Authentication was externally verified and commands may be accepted.
    Ready,
    /// Closing has started and no new protocol work may begin.
    Closing,
    /// The connection is fully closed.
    Closed,
}

/// An event used in typed invalid-transition errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// Emit the server's initial handshake.
    SendInitialHandshake,
    /// Process a client handshake response.
    ReceiveClientResponse,
    /// Process the fixed-size client SSLRequest packet.
    ReceiveSslRequest,
    /// Report that the external TLS handshake completed.
    TlsNegotiated,
    /// Start the authentication phase after TLS negotiation.
    BeginAuthentication,
    /// Apply an external decision to the initial authentication response.
    InitialAuthenticationResult,
    /// Receive a full authentication response from the client.
    ReceiveClientAuthResponse,
    /// Apply an external decision to the full authentication response.
    FullAuthenticationResult,
    /// Send the authentication OK packet after verification.
    SendAuthenticationOk,
    /// Accept a classic protocol command.
    Command,
    /// Begin closing the connection.
    BeginClose,
    /// Report that transport shutdown completed.
    Closed,
}

/// Which credential material an external verifier is being asked to inspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationVerificationStage {
    /// The bounded scramble from the client handshake response.
    InitialHandshakeResponse,
    /// The cleartext response received only after a secure full-auth request.
    FullAuthenticationResponse,
}

/// A temporary, borrowed request for external credential verification.
///
/// The state machine never stores the full-authentication bytes. A caller
/// should finish verification before the borrowed input frame is discarded.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialVerificationRequest<'a> {
    /// Username from the accepted client handshake response.
    pub username: String,
    /// The only authentication plugin accepted by this state machine.
    pub plugin_name: &'static str,
    /// Server scramble needed by a verifier for the handshake response.
    pub server_auth_plugin_data: [u8; AUTH_PLUGIN_DATA_LENGTH],
    /// Handshake scramble or full-authentication bytes, depending on `stage`.
    pub auth_response: &'a [u8],
    /// Authentication phase represented by `auth_response`.
    pub stage: AuthenticationVerificationStage,
    /// Transport security established before this request was created.
    pub transport_security: TransportSecurity,
    /// Only the state machine may mark the packet layout as validated.
    pub(crate) frame_validated: bool,
}

impl fmt::Debug for CredentialVerificationRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialVerificationRequest")
            .field("username", &self.username)
            .field("plugin_name", &self.plugin_name)
            .field("auth_response", &"<redacted>")
            .field("stage", &self.stage)
            .finish()
    }
}

/// External decision for the client's initial handshake authentication data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialAuthenticationResult {
    /// The cached verifier accepted the handshake response.
    FastAuthSuccess,
    /// The cache was unavailable; request a full response over secure transport.
    FullAuthenticationRequired,
    /// The external verifier rejected the credentials.
    Rejected,
}

/// External decision for a client's secure full-authentication response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullAuthenticationResult {
    /// The external verifier accepted the full response.
    Authenticated,
    /// The external verifier rejected the credentials.
    Rejected,
}

/// Selects a logical database after credentials have been accepted.
///
/// The selector belongs to the server or frontend owner. This protocol crate
/// only passes the bounded logical name to it and never interprets that name
/// as a filesystem path.
pub trait InitialDatabaseSelector {
    /// Selects the database requested in the client handshake response.
    fn select_initial_database(&mut self, database: &str) -> Result<(), FrontendErrorKind>;
}

impl<F> InitialDatabaseSelector for F
where
    F: FnMut(&str) -> Result<(), FrontendErrorKind>,
{
    fn select_initial_database(&mut self, database: &str) -> Result<(), FrontendErrorKind> {
        self(database)
    }
}

/// The final server packet produced after authentication and optional initial
/// database selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticationResponse {
    /// Authentication and initial database selection succeeded.
    Ok(Vec<u8>),
    /// Database selection failed. The frame is safe to send to the client;
    /// backend details are represented only by the typed error category.
    Err {
        /// Typed frontend category mapped into the ERR packet.
        kind: FrontendErrorKind,
        /// Bounded protocol ERR frame, already using the authentication sequence.
        frame: Vec<u8>,
    },
}

impl AuthenticationResponse {
    /// Returns the packet frame that the caller should send.
    pub fn frame(&self) -> &[u8] {
        match self {
            Self::Ok(frame) => frame,
            Self::Err { frame, .. } => frame,
        }
    }

    /// Returns the typed frontend error when database selection failed.
    pub const fn error_kind(&self) -> Option<FrontendErrorKind> {
        match self {
            Self::Ok(_) => None,
            Self::Err { kind, .. } => Some(*kind),
        }
    }
}

/// The state machine for one bounded classic-protocol connection.
///
/// This type is deliberately not `Clone`: its handshake contains a
/// per-connection authentication nonce that must never be copied into a
/// second connection.
#[derive(Debug, PartialEq, Eq)]
pub struct ClassicConnection {
    state: ConnectionState,
    initial_handshake: InitialHandshakeConfig,
    packet_codec: PacketCodec,
    response_packet_codec: PacketCodec,
    transport_security: TransportSecurity,
    client_response: Option<ClientHandshakeResponse>,
    initial_database: Option<String>,
    ssl_request: Option<ClientSslRequest>,
    negotiated_capabilities: Option<u32>,
    auth_server_sequence_id: Option<u8>,
    auth_client_sequence_id: Option<u8>,
}

impl ClassicConnection {
    /// Creates a connection for a plaintext transport.
    ///
    /// Authentication cannot proceed until the caller explicitly selects
    /// [`TransportSecurity::Secure`] or completes the TLS transition.
    pub fn new(settings: InitialHandshakeSettings) -> Result<Self, ConnectionStateError> {
        Self::with_transport_security(settings, TransportSecurity::default())
    }

    /// Creates a connection with an explicit transport-security declaration.
    pub fn with_transport_security(
        settings: InitialHandshakeSettings,
        transport_security: TransportSecurity,
    ) -> Result<Self, ConnectionStateError> {
        let packet_codec = PacketCodec::new(
            MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH.max(MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH),
        )?;
        Self::with_codec(settings, packet_codec, transport_security)
    }

    /// Creates a connection with an existing packet codec and transport state.
    pub fn with_codec(
        settings: InitialHandshakeSettings,
        packet_codec: PacketCodec,
        transport_security: TransportSecurity,
    ) -> Result<Self, ConnectionStateError> {
        let mut nonce_source = OsHandshakeNonceSource;
        Self::with_nonce_source(
            settings,
            packet_codec,
            transport_security,
            &mut nonce_source,
        )
    }

    fn with_nonce_source<S: HandshakeNonceSource>(
        settings: InitialHandshakeSettings,
        packet_codec: PacketCodec,
        transport_security: TransportSecurity,
        nonce_source: &mut S,
    ) -> Result<Self, ConnectionStateError> {
        let mut auth_plugin_data = [0; AUTH_PLUGIN_DATA_LENGTH];
        nonce_source
            .fill_nonce(&mut auth_plugin_data)
            .map_err(ConnectionStateError::from)?;
        let initial_handshake = settings.with_auth_plugin_data(auth_plugin_data);
        Self::with_config(initial_handshake, packet_codec, transport_security)
    }

    #[cfg(test)]
    pub(crate) fn with_test_nonce(
        settings: InitialHandshakeSettings,
        packet_codec: PacketCodec,
        transport_security: TransportSecurity,
        auth_plugin_data: [u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> Result<Self, ConnectionStateError> {
        let initial_handshake = settings.with_auth_plugin_data(auth_plugin_data);
        Self::with_config(initial_handshake, packet_codec, transport_security)
    }

    #[cfg(test)]
    pub(crate) fn with_test_nonce_source<S: HandshakeNonceSource>(
        settings: InitialHandshakeSettings,
        packet_codec: PacketCodec,
        transport_security: TransportSecurity,
        nonce_source: &mut S,
    ) -> Result<Self, ConnectionStateError> {
        Self::with_nonce_source(settings, packet_codec, transport_security, nonce_source)
    }

    fn with_config(
        initial_handshake: InitialHandshakeConfig,
        packet_codec: PacketCodec,
        transport_security: TransportSecurity,
    ) -> Result<Self, ConnectionStateError> {
        initial_handshake.validate()?;
        if packet_codec.max_payload_len() < MIN_SERVER_RESPONSE_PAYLOAD_LENGTH as usize {
            return Err(ConnectionStateError::ResponsePayloadLimitTooSmall {
                limit: packet_codec.max_payload_len(),
                minimum: MIN_SERVER_RESPONSE_PAYLOAD_LENGTH as usize,
            });
        }
        if initial_handshake.auth_plugin_name != CACHING_SHA2_PASSWORD_PLUGIN {
            return Err(ConnectionStateError::UnsupportedAuthenticationPlugin {
                plugin: Some(initial_handshake.auth_plugin_name),
            });
        }
        Ok(Self {
            state: ConnectionState::SendInitialHandshake,
            initial_handshake,
            packet_codec,
            response_packet_codec: packet_codec,
            transport_security,
            client_response: None,
            initial_database: None,
            ssl_request: None,
            negotiated_capabilities: None,
            auth_server_sequence_id: None,
            auth_client_sequence_id: None,
        })
    }

    /// Returns the current protocol state.
    pub const fn state(&self) -> ConnectionState {
        self.state
    }

    /// Returns the negotiated client/server capability intersection.
    pub const fn negotiated_capabilities(&self) -> Option<u32> {
        self.negotiated_capabilities
    }

    pub(crate) const fn response_packet_codec(&self) -> PacketCodec {
        self.response_packet_codec
    }

    /// Returns the decoded client response after it has been accepted.
    pub fn client_response(&self) -> Option<&ClientHandshakeResponse> {
        self.client_response.as_ref()
    }

    /// Returns the logical database requested in the client handshake.
    pub fn initial_database(&self) -> Option<&str> {
        self.initial_database.as_deref()
    }

    /// Encodes the initial handshake and moves to [`ConnectionState::AwaitClientResponse`].
    pub fn send_initial_handshake(&mut self) -> Result<Vec<u8>, ConnectionStateError> {
        self.require_state(
            ConnectionState::SendInitialHandshake,
            ConnectionEvent::SendInitialHandshake,
        )?;
        let frame = self.initial_handshake.encode(self.packet_codec, 0)?;
        self.state = ConnectionState::AwaitClientResponse;
        Ok(frame)
    }

    /// Decodes and processes one client handshake response frame.
    pub fn receive_client_handshake_frame(
        &mut self,
        frame: &[u8],
    ) -> Result<(), ConnectionStateError> {
        match self.state {
            ConnectionState::AwaitClientResponse => {
                let packet = self.packet_codec.decode(frame)?;
                if packet.payload.len() == CLIENT_SSL_REQUEST_PAYLOAD_LENGTH {
                    let request = ClientSslRequest::decode(self.packet_codec, frame)?;
                    return self.receive_client_ssl_request(request);
                }
            }
            ConnectionState::TlsNegotiated => {}
            state => {
                return Err(ConnectionStateError::InvalidTransition {
                    state,
                    event: ConnectionEvent::ReceiveClientResponse,
                });
            }
        }
        let response = self.packet_codec.decode_client_handshake_response(frame)?;
        self.receive_client_handshake_response(response)
    }

    /// Decodes and processes the fixed-size SSLRequest sent before TLS.
    pub fn receive_client_ssl_request_frame(
        &mut self,
        frame: &[u8],
    ) -> Result<(), ConnectionStateError> {
        self.require_state(
            ConnectionState::AwaitClientResponse,
            ConnectionEvent::ReceiveSslRequest,
        )?;
        let request = self.packet_codec.decode_client_ssl_request(frame)?;
        self.receive_client_ssl_request(request)
    }

    /// Processes an already-decoded SSLRequest before TLS negotiation.
    pub fn receive_client_ssl_request(
        &mut self,
        request: ClientSslRequest,
    ) -> Result<(), ConnectionStateError> {
        self.require_state(
            ConnectionState::AwaitClientResponse,
            ConnectionEvent::ReceiveSslRequest,
        )?;
        request.to_config().validate()?;
        self.validate_sequence(
            request.sequence_id,
            CLIENT_HANDSHAKE_SEQUENCE_ID,
            ConnectionEvent::ReceiveSslRequest,
        )?;
        self.validate_client_capabilities(request.capability_flags)?;
        let negotiated_capabilities =
            request.capability_flags & self.initial_handshake.capability_flags;
        self.set_response_packet_limit(request.max_packet_size)?;
        self.ssl_request = Some(request);
        self.negotiated_capabilities = Some(negotiated_capabilities);
        self.state = ConnectionState::TlsUpgradeRequired;
        Ok(())
    }

    /// Processes an already-decoded client handshake response.
    pub fn receive_client_handshake_response(
        &mut self,
        response: ClientHandshakeResponse,
    ) -> Result<(), ConnectionStateError> {
        let expected_sequence_id = match self.state {
            ConnectionState::AwaitClientResponse => CLIENT_HANDSHAKE_SEQUENCE_ID,
            ConnectionState::TlsNegotiated => TLS_CLIENT_HANDSHAKE_SEQUENCE_ID,
            state => {
                return Err(ConnectionStateError::InvalidTransition {
                    state,
                    event: ConnectionEvent::ReceiveClientResponse,
                });
            }
        };
        self.validate_sequence(
            response.sequence_id,
            expected_sequence_id,
            ConnectionEvent::ReceiveClientResponse,
        )?;
        let response_sequence_id = response.sequence_id;
        response.to_config().validate()?;
        let client_capabilities = response.capability_flags;
        self.validate_client_capabilities(client_capabilities)?;
        if response.auth_plugin_name.as_deref() != Some(CACHING_SHA2_PASSWORD_PLUGIN) {
            return Err(ConnectionStateError::UnsupportedAuthenticationPlugin {
                plugin: response.auth_plugin_name,
            });
        }

        let initial_response = self.state == ConnectionState::AwaitClientResponse;
        if initial_response {
            if client_capabilities & CLIENT_SSL != 0 {
                return Err(ConnectionStateError::TlsRequestRequired);
            }
            if self.transport_security != TransportSecurity::Secure {
                return Err(ConnectionStateError::SecureTransportRequired);
            }
        } else {
            let Some(ssl_request) = self.ssl_request.as_ref() else {
                return Err(ConnectionStateError::SslRequestRequired);
            };
            if client_capabilities & CLIENT_SSL == 0 {
                return Err(ConnectionStateError::TlsResponseMissingSslCapability);
            }
            if client_capabilities != ssl_request.capability_flags {
                return Err(ConnectionStateError::CapabilitiesChangedAfterTls {
                    ssl_request: ssl_request.capability_flags,
                    response: client_capabilities,
                });
            }
        }
        self.set_response_packet_limit(response.max_packet_size)?;
        if initial_response {
            self.state = ConnectionState::AuthenticateCachingSha2Password;
            self.negotiated_capabilities =
                Some(client_capabilities & self.initial_handshake.capability_flags);
            self.auth_server_sequence_id = Some(response_sequence_id.wrapping_add(1));
        }
        self.initial_database.clone_from(&response.database);
        self.client_response = Some(response);
        Ok(())
    }

    /// Reports completion of the external TLS handshake.
    pub fn tls_upgrade_complete(&mut self) -> Result<(), ConnectionStateError> {
        self.require_state(
            ConnectionState::TlsUpgradeRequired,
            ConnectionEvent::TlsNegotiated,
        )?;
        self.transport_security = TransportSecurity::Secure;
        self.state = ConnectionState::TlsNegotiated;
        Ok(())
    }

    /// Moves from negotiated TLS to the caching-SHA-2 authentication phase.
    pub fn begin_authentication(&mut self) -> Result<(), ConnectionStateError> {
        self.require_state(
            ConnectionState::TlsNegotiated,
            ConnectionEvent::BeginAuthentication,
        )?;
        if self.client_response.is_none() {
            return Err(ConnectionStateError::ClientResponseRequired);
        }
        if self.transport_security != TransportSecurity::Secure {
            return Err(ConnectionStateError::SecureTransportRequired);
        }
        let response = self
            .client_response
            .as_ref()
            .ok_or(ConnectionStateError::ClientResponseRequired)?;
        self.auth_server_sequence_id = Some(response.sequence_id.wrapping_add(1));
        self.state = ConnectionState::AuthenticateCachingSha2Password;
        Ok(())
    }

    /// Returns the accepted handshake authentication data for an external verifier.
    pub fn authentication_verification_request(
        &self,
    ) -> Result<CredentialVerificationRequest<'_>, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateCachingSha2Password,
            ConnectionEvent::InitialAuthenticationResult,
        )?;
        let response = self
            .client_response
            .as_ref()
            .ok_or(ConnectionStateError::ClientResponseRequired)?;
        Ok(CredentialVerificationRequest {
            username: response.username.clone(),
            plugin_name: CACHING_SHA2_PASSWORD_PLUGIN,
            server_auth_plugin_data: self.initial_handshake.auth_plugin_data,
            auth_response: &response.auth_response,
            stage: AuthenticationVerificationStage::InitialHandshakeResponse,
            transport_security: self.transport_security,
            frame_validated: true,
        })
    }

    /// Applies the external decision for the handshake authentication data.
    ///
    /// A success returns the bounded server packet that must be sent next. A
    /// full-authentication decision returns the `AuthMoreData` request and
    /// waits for the client's cleartext response over the already secure
    /// transport. No credential verification is performed here.
    pub(crate) fn apply_initial_authentication_result(
        &mut self,
        result: InitialAuthenticationResult,
    ) -> Result<Vec<u8>, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateCachingSha2Password,
            ConnectionEvent::InitialAuthenticationResult,
        )?;
        let kind = match result {
            InitialAuthenticationResult::FastAuthSuccess => AuthMoreDataKind::FastAuthSuccess,
            InitialAuthenticationResult::FullAuthenticationRequired => {
                if self.transport_security != TransportSecurity::Secure {
                    return Err(ConnectionStateError::SecureTransportRequired);
                }
                AuthMoreDataKind::FullAuthenticationRequired
            }
            InitialAuthenticationResult::Rejected => {
                self.state = ConnectionState::Closing;
                return Err(ConnectionStateError::AuthenticationRejected);
            }
        };
        let sequence_id = self.auth_server_sequence_id()?;
        let frame = AuthMoreData::encode(self.response_packet_codec, sequence_id, kind)?;
        self.auth_server_sequence_id = Some(sequence_id.wrapping_add(1));
        match kind {
            AuthMoreDataKind::FastAuthSuccess => {
                self.state = ConnectionState::AuthenticateFast;
            }
            AuthMoreDataKind::FullAuthenticationRequired => {
                self.auth_client_sequence_id = Some(sequence_id.wrapping_add(1));
                self.state = ConnectionState::AuthenticateFull;
            }
        }
        Ok(frame)
    }

    /// Verifies the handshake request and applies its result to this connection.
    ///
    /// Provider failures close the connection without turning backend details
    /// into a client-visible protocol response. A fast success still leaves the
    /// connection in [`ConnectionState::AuthenticateFast`] until
    /// [`Self::send_authentication_ok`] emits the final OK packet.
    #[cfg(test)]
    pub(crate) fn verify_and_apply_initial_authentication<P: CredentialProvider>(
        &mut self,
        verifier: &CachingSha2Verifier<P>,
    ) -> Result<Vec<u8>, ConnectionStateError> {
        let result = {
            let request = self.authentication_verification_request()?;
            verifier.verify_initial(&request)
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.state = ConnectionState::Closing;
                return Err(ConnectionStateError::CredentialVerification(error));
            }
        };
        self.apply_initial_authentication_result(result)
    }

    /// Sends the final OK packet after cached authentication succeeds.
    ///
    /// This compatibility method remains valid for handshakes without an
    /// initial database. A handshake that requested a database must use
    /// [`Self::send_authentication_ok_with_selector`] so selection is proved
    /// before the connection becomes ready.
    pub fn send_authentication_ok(&mut self) -> Result<Vec<u8>, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateFast,
            ConnectionEvent::SendAuthenticationOk,
        )?;
        if self.initial_database.is_some() {
            self.state = ConnectionState::Closing;
            return Err(ConnectionStateError::InitialDatabaseSelectorRequired);
        }
        self.encode_authentication_ok()
    }

    /// Sends the final authentication response after selecting the requested
    /// initial database.
    ///
    /// A database-selection failure returns a safe, typed ERR frame and moves
    /// the connection to [`ConnectionState::Closing`]. The selector is never
    /// called when the handshake did not request an initial database.
    pub fn send_authentication_ok_with_selector<S: InitialDatabaseSelector>(
        &mut self,
        selector: &mut S,
    ) -> Result<AuthenticationResponse, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateFast,
            ConnectionEvent::SendAuthenticationOk,
        )?;
        if let Some(database) = self.initial_database.clone() {
            return match selector.select_initial_database(&database) {
                Ok(()) => Ok(AuthenticationResponse::Ok(self.encode_authentication_ok()?)),
                Err(kind) => self.authentication_error_response(kind),
            };
        }
        Ok(AuthenticationResponse::Ok(self.encode_authentication_ok()?))
    }

    fn encode_authentication_ok(&mut self) -> Result<Vec<u8>, ConnectionStateError> {
        let sequence_id = self.auth_server_sequence_id()?;
        let frame =
            AuthOkPacketConfig::default().encode(self.response_packet_codec, sequence_id)?;
        self.auth_server_sequence_id = Some(sequence_id.wrapping_add(1));
        self.state = ConnectionState::Ready;
        Ok(frame)
    }

    pub(crate) fn authentication_error_response(
        &mut self,
        kind: FrontendErrorKind,
    ) -> Result<AuthenticationResponse, ConnectionStateError> {
        let sequence_id = self.auth_server_sequence_id()?;
        let capabilities = self
            .negotiated_capabilities
            .ok_or(ConnectionStateError::ClientResponseRequired)?;
        self.auth_server_sequence_id = Some(sequence_id.wrapping_add(1));
        self.state = ConnectionState::Closing;
        let frame = map_frontend_error(kind).encode(
            self.response_packet_codec,
            sequence_id,
            capabilities,
        )?;
        Ok(AuthenticationResponse::Err { kind, frame })
    }

    /// Decodes the client's bounded full-authentication response.
    ///
    /// The returned request borrows only the supplied frame's password bytes;
    /// the connection retains neither the cleartext bytes nor the packet.
    pub fn receive_full_authentication_frame<'a>(
        &mut self,
        frame: &'a [u8],
    ) -> Result<CredentialVerificationRequest<'a>, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateFull,
            ConnectionEvent::ReceiveClientAuthResponse,
        )?;
        let response = ClientAuthResponse::decode(self.packet_codec, frame)?;
        let expected_sequence_id = self
            .auth_client_sequence_id
            .ok_or(ConnectionStateError::ClientResponseRequired)?;
        self.validate_sequence(
            response.sequence_id,
            expected_sequence_id,
            ConnectionEvent::ReceiveClientAuthResponse,
        )?;
        let username = self
            .client_response
            .as_ref()
            .ok_or(ConnectionStateError::ClientResponseRequired)?
            .username
            .clone();
        self.auth_server_sequence_id = Some(response.sequence_id.wrapping_add(1));
        self.state = ConnectionState::AuthenticateFullVerification;
        Ok(CredentialVerificationRequest {
            username,
            plugin_name: CACHING_SHA2_PASSWORD_PLUGIN,
            server_auth_plugin_data: self.initial_handshake.auth_plugin_data,
            auth_response: response.auth_response,
            stage: AuthenticationVerificationStage::FullAuthenticationResponse,
            transport_security: self.transport_security,
            frame_validated: true,
        })
    }

    /// Applies an external decision for a secure full-authentication response.
    #[cfg(test)]
    pub(crate) fn apply_full_authentication_result(
        &mut self,
        result: FullAuthenticationResult,
    ) -> Result<Vec<u8>, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateFullVerification,
            ConnectionEvent::FullAuthenticationResult,
        )?;
        if result == FullAuthenticationResult::Authenticated && self.initial_database.is_some() {
            self.state = ConnectionState::Closing;
            return Err(ConnectionStateError::InitialDatabaseSelectorRequired);
        }
        self.apply_full_authentication_result_unchecked(result)
    }

    /// Applies a successful full-authentication result after selecting the
    /// requested initial database.
    pub fn apply_full_authentication_result_with_selector<S: InitialDatabaseSelector>(
        &mut self,
        result: FullAuthenticationResult,
        selector: &mut S,
    ) -> Result<AuthenticationResponse, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateFullVerification,
            ConnectionEvent::FullAuthenticationResult,
        )?;
        if result == FullAuthenticationResult::Rejected {
            self.state = ConnectionState::Closing;
            return Err(ConnectionStateError::AuthenticationRejected);
        }
        if let Some(database) = self.initial_database.clone() {
            return match selector.select_initial_database(&database) {
                Ok(()) => Ok(AuthenticationResponse::Ok(
                    self.apply_full_authentication_result_unchecked(result)?,
                )),
                Err(kind) => self.authentication_error_response(kind),
            };
        }
        Ok(AuthenticationResponse::Ok(
            self.apply_full_authentication_result_unchecked(result)?,
        ))
    }

    /// Rejects a verified full-authentication response before an executor is
    /// installed. This keeps the pre-authentication path free of a database
    /// selector while preserving the existing close-on-rejection transition.
    pub(crate) fn reject_full_authentication(
        &mut self,
    ) -> Result<AuthenticationResponse, ConnectionStateError> {
        self.require_state(
            ConnectionState::AuthenticateFullVerification,
            ConnectionEvent::FullAuthenticationResult,
        )?;
        self.state = ConnectionState::Closing;
        Err(ConnectionStateError::AuthenticationRejected)
    }

    fn apply_full_authentication_result_unchecked(
        &mut self,
        result: FullAuthenticationResult,
    ) -> Result<Vec<u8>, ConnectionStateError> {
        if result == FullAuthenticationResult::Rejected {
            self.state = ConnectionState::Closing;
            return Err(ConnectionStateError::AuthenticationRejected);
        }
        let sequence_id = self.auth_server_sequence_id()?;
        let frame =
            AuthOkPacketConfig::default().encode(self.response_packet_codec, sequence_id)?;
        self.auth_server_sequence_id = Some(sequence_id.wrapping_add(1));
        self.state = ConnectionState::Ready;
        Ok(frame)
    }

    /// Receives, verifies, and applies a secure full-authentication response.
    #[cfg(test)]
    pub(crate) fn verify_and_apply_full_authentication<P: CredentialProvider>(
        &mut self,
        frame: &[u8],
        verifier: &CachingSha2Verifier<P>,
    ) -> Result<Vec<u8>, ConnectionStateError> {
        let result = {
            let request = self.receive_full_authentication_frame(frame)?;
            verifier.verify_full(&request)
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.state = ConnectionState::Closing;
                return Err(ConnectionStateError::CredentialVerification(error));
            }
        };
        self.apply_full_authentication_result(result)
    }

    /// Verifies a full-authentication response and completes authentication
    /// only after selecting the requested initial database.
    #[cfg(test)]
    pub(crate) fn verify_and_apply_full_authentication_with_selector<
        P: CredentialProvider,
        S: InitialDatabaseSelector,
    >(
        &mut self,
        frame: &[u8],
        verifier: &CachingSha2Verifier<P>,
        selector: &mut S,
    ) -> Result<AuthenticationResponse, ConnectionStateError> {
        let result = {
            let request = self.receive_full_authentication_frame(frame)?;
            verifier.verify_full(&request)
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.state = ConnectionState::Closing;
                return Err(ConnectionStateError::CredentialVerification(error));
            }
        };
        self.apply_full_authentication_result_with_selector(result, selector)
    }

    /// Decodes one command packet while in [`ConnectionState::Ready`].
    ///
    /// The returned command borrows the supplied frame. Decoding does not
    /// execute a query or change session data. A valid `COM_QUIT` packet is
    /// the one exception to the state transition: it moves the connection to
    /// [`ConnectionState::Closing`].
    pub fn receive_command_frame<'a>(
        &mut self,
        frame: &'a [u8],
    ) -> Result<ClassicCommandPacket<'a>, ConnectionStateError> {
        self.ensure_ready()?;
        let packet = self
            .packet_codec
            .decode(frame)
            .map_err(CommandPacketError::from)?;
        let command = decode_command_packet(packet).map_err(ConnectionStateError::from)?;
        let is_quit = matches!(command.command, ClassicCommand::Quit);
        if is_quit {
            self.state = ConnectionState::Closing;
        }
        Ok(command)
    }

    /// Decodes one already-framed command packet while ready.
    pub fn receive_command_packet<'a>(
        &mut self,
        packet: Packet<'a>,
    ) -> Result<ClassicCommandPacket<'a>, ConnectionStateError> {
        self.ensure_ready()?;
        let command = decode_command_packet(packet).map_err(ConnectionStateError::from)?;
        if matches!(command.command, ClassicCommand::Quit) {
            self.state = ConnectionState::Closing;
        }
        Ok(command)
    }

    /// Returns `Ok(())` only when commands may be processed.
    pub fn ensure_ready(&self) -> Result<(), ConnectionStateError> {
        if self.state != ConnectionState::Ready {
            return Err(ConnectionStateError::CommandBeforeReady { state: self.state });
        }
        Ok(())
    }

    /// Starts connection shutdown.
    pub fn begin_close(&mut self) -> Result<(), ConnectionStateError> {
        if matches!(
            self.state,
            ConnectionState::Closing | ConnectionState::Closed
        ) {
            return Err(ConnectionStateError::InvalidTransition {
                state: self.state,
                event: ConnectionEvent::BeginClose,
            });
        }
        self.state = ConnectionState::Closing;
        Ok(())
    }

    /// Reports that transport shutdown completed.
    pub fn finish_close(&mut self) -> Result<(), ConnectionStateError> {
        self.require_state(ConnectionState::Closing, ConnectionEvent::Closed)?;
        self.state = ConnectionState::Closed;
        Ok(())
    }

    fn require_state(
        &self,
        expected: ConnectionState,
        event: ConnectionEvent,
    ) -> Result<(), ConnectionStateError> {
        if self.state != expected {
            return Err(ConnectionStateError::InvalidTransition {
                state: self.state,
                event,
            });
        }
        Ok(())
    }

    fn validate_client_capabilities(
        &self,
        client_capabilities: u32,
    ) -> Result<(), ConnectionStateError> {
        let server_capabilities = self.initial_handshake.capability_flags;
        let unsupported = client_capabilities & !server_capabilities;
        if unsupported != 0 {
            return Err(ConnectionStateError::CapabilityNotAdvertised {
                server: server_capabilities,
                client: client_capabilities,
                unsupported,
            });
        }
        Ok(())
    }

    fn validate_sequence(
        &self,
        actual: u8,
        expected: u8,
        event: ConnectionEvent,
    ) -> Result<(), ConnectionStateError> {
        if actual != expected {
            return Err(ConnectionStateError::UnexpectedSequenceId {
                event,
                expected,
                actual,
            });
        }
        Ok(())
    }

    fn auth_server_sequence_id(&self) -> Result<u8, ConnectionStateError> {
        self.auth_server_sequence_id
            .ok_or(ConnectionStateError::ClientResponseRequired)
    }

    fn set_response_packet_limit(
        &mut self,
        client_max_packet_size: u32,
    ) -> Result<(), ConnectionStateError> {
        let requested_limit = if client_max_packet_size == 0 {
            self.packet_codec.max_payload_len()
        } else {
            usize::try_from(client_max_packet_size).unwrap_or(usize::MAX)
        };
        let response_limit = self.packet_codec.max_payload_len().min(requested_limit);
        self.response_packet_codec = PacketCodec::new(response_limit)?;
        Ok(())
    }
}

fn decode_command_packet<'a>(
    packet: Packet<'a>,
) -> Result<ClassicCommandPacket<'a>, CommandPacketError> {
    if packet.sequence_id != COMMAND_SEQUENCE_ID {
        return Err(CommandPacketError::UnexpectedSequenceId {
            expected: COMMAND_SEQUENCE_ID,
            actual: packet.sequence_id,
        });
    }
    if packet.payload.len() > MAX_COMMAND_PAYLOAD_LENGTH {
        return Err(CommandPacketError::PayloadTooLarge {
            length: packet.payload.len(),
            limit: MAX_COMMAND_PAYLOAD_LENGTH,
        });
    }
    let (&command, body) = packet
        .payload
        .split_first()
        .ok_or(CommandPacketError::EmptyPayload)?;
    let command = match command {
        COM_QUERY => ClassicCommand::Query {
            sql: decode_command_text(body, command, "query")?,
        },
        COM_INIT_DB => ClassicCommand::InitDb {
            database: decode_command_text(body, command, "database")?,
        },
        COM_STMT_PREPARE => ClassicCommand::StmtPrepare {
            sql: decode_command_text(body, command, "query")?,
        },
        COM_STMT_EXECUTE => {
            let (statement_id, flags, iteration_count, parameter_payload) =
                decode_stmt_execute(body, command)?;
            ClassicCommand::StmtExecute {
                statement_id,
                flags,
                iteration_count,
                parameter_payload,
            }
        }
        COM_STMT_CLOSE => ClassicCommand::StmtClose {
            statement_id: decode_statement_id(body, command)?,
        },
        COM_STMT_RESET => ClassicCommand::StmtReset {
            statement_id: decode_statement_id(body, command)?,
        },
        COM_PING => {
            validate_exact_body_length(body, command, 0)?;
            ClassicCommand::Ping
        }
        COM_QUIT => {
            validate_exact_body_length(body, command, 0)?;
            ClassicCommand::Quit
        }
        COM_STMT_SEND_LONG_DATA => {
            return Err(CommandPacketError::UnsupportedPreparedStatement { command });
        }
        command => return Err(CommandPacketError::UnsupportedCommand { command }),
    };
    Ok(ClassicCommandPacket {
        sequence_id: packet.sequence_id,
        command,
    })
}

fn decode_stmt_execute(
    body: &[u8],
    command: u8,
) -> Result<(u32, u8, u32, &[u8]), CommandPacketError> {
    if body.len() < STMT_EXECUTE_FIXED_BODY_LENGTH {
        return Err(CommandPacketError::InvalidPayloadLength {
            command,
            expected: STMT_EXECUTE_FIXED_BODY_LENGTH + 1,
            actual: body.len() + 1,
        });
    }
    let statement_id = u32::from_le_bytes(
        body[..4]
            .try_into()
            .expect("statement execute body length was validated above"),
    );
    let flags = body[4];
    if flags != CURSOR_TYPE_NO_CURSOR {
        return Err(CommandPacketError::UnsupportedStmtExecuteFlags { flags });
    }
    let iteration_count = u32::from_le_bytes(
        body[5..STMT_EXECUTE_FIXED_BODY_LENGTH]
            .try_into()
            .expect("statement execute body length was validated above"),
    );
    if iteration_count != 1 {
        return Err(CommandPacketError::InvalidStmtExecuteIterationCount { iteration_count });
    }
    Ok((
        statement_id,
        flags,
        iteration_count,
        &body[STMT_EXECUTE_FIXED_BODY_LENGTH..],
    ))
}

fn decode_statement_id(body: &[u8], command: u8) -> Result<u32, CommandPacketError> {
    validate_exact_body_length(body, command, 4)?;
    Ok(u32::from_le_bytes(
        body.try_into()
            .expect("statement id length was validated above"),
    ))
}

fn decode_command_text<'a>(
    body: &'a [u8],
    command: u8,
    field: &'static str,
) -> Result<&'a str, CommandPacketError> {
    if body.is_empty() {
        return Err(CommandPacketError::EmptyText { command, field });
    }
    if let Some(offset) = body.iter().position(|byte| *byte == 0) {
        return Err(CommandPacketError::EmbeddedNul {
            command,
            field,
            offset,
        });
    }
    std::str::from_utf8(body).map_err(|_| CommandPacketError::InvalidUtf8 { command, field })
}

fn validate_exact_body_length(
    body: &[u8],
    command: u8,
    expected: usize,
) -> Result<(), CommandPacketError> {
    if body.len() != expected {
        return Err(CommandPacketError::InvalidPayloadLength {
            command,
            expected: expected + 1,
            actual: body.len() + 1,
        });
    }
    Ok(())
}

/// Errors returned by a classic connection state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStateError {
    /// An event was not valid for the current state.
    InvalidTransition {
        /// State before the invalid event.
        state: ConnectionState,
        /// Event that was rejected.
        event: ConnectionEvent,
    },
    /// Initial-handshake framing or validation failed.
    InitialHandshake(InitialHandshakeError),
    /// A fresh per-connection authentication nonce could not be obtained.
    InitialHandshakeNonce(InitialHandshakeNonceError),
    /// Client-handshake framing or validation failed.
    ClientHandshakeResponse(ClientHandshakeResponseError),
    /// The client requested a capability absent from the server handshake.
    CapabilityNotAdvertised {
        /// Capabilities sent by the server.
        server: u32,
        /// Capabilities sent by the client.
        client: u32,
        /// Client bits absent from the server set.
        unsupported: u32,
    },
    /// This state machine accepts only `caching_sha2_password`.
    UnsupportedAuthenticationPlugin {
        /// Plugin name sent by the peer, or the server's configured name.
        plugin: Option<String>,
    },
    /// Authentication cannot begin on an unprotected transport.
    SecureTransportRequired,
    /// A command arrived before external authentication verification.
    CommandBeforeReady {
        /// State in which the command was received.
        state: ConnectionState,
    },
    /// Command packet framing or payload validation failed.
    Command(CommandPacketError),
    /// Packet codec construction or an injected codec operation failed.
    PacketCodec(PacketCodecError),
    /// A typed authentication ERR response could not be encoded.
    ResponsePacket(ResponsePacketError),
    /// The server codec cannot carry the smallest required response packet.
    ResponsePayloadLimitTooSmall { limit: usize, minimum: usize },
    /// SSLRequest framing or validation failed.
    SslRequest(ClientSslRequestError),
    /// Authentication packet framing or validation failed.
    AuthPacket(AuthPacketError),
    /// A packet arrived with a sequence number for another protocol phase.
    UnexpectedSequenceId {
        /// Phase that received the packet.
        event: ConnectionEvent,
        /// Required sequence number.
        expected: u8,
        /// Sequence number received in the packet header.
        actual: u8,
    },
    /// A full client response requested TLS without a preceding SSLRequest.
    TlsRequestRequired,
    /// A post-TLS client response omitted the TLS capability.
    TlsResponseMissingSslCapability,
    /// The full response changed capabilities from the preceding SSLRequest.
    CapabilitiesChangedAfterTls { ssl_request: u32, response: u32 },
    /// Authentication was requested before the full post-TLS client response.
    ClientResponseRequired,
    /// A post-TLS response was received without the preceding SSLRequest.
    SslRequestRequired,
    /// The external verifier rejected the credentials.
    AuthenticationRejected,
    /// The handshake requested a database, but the compatibility API without
    /// a selector was used to complete authentication.
    InitialDatabaseSelectorRequired,
    /// The credential provider or verifier failed before a protocol decision.
    CredentialVerification(CredentialVerificationError),
}

/// Errors returned by the bounded command packet decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPacketError {
    /// Packet framing rejected the frame.
    PacketCodec(PacketCodecError),
    /// The command packet had no command identifier.
    EmptyPayload,
    /// The packet's payload exceeds the command decoder's independent bound.
    PayloadTooLarge {
        /// Supplied payload length.
        length: usize,
        /// Maximum accepted payload length.
        limit: usize,
    },
    /// Command packets must start a command exchange at sequence zero.
    UnexpectedSequenceId {
        /// Required sequence number.
        expected: u8,
        /// Sequence number received in the packet header.
        actual: u8,
    },
    /// A fixed-size command had bytes beyond its command identifier.
    InvalidPayloadLength {
        /// Command identifier.
        command: u8,
        /// Expected total packet payload length, including the identifier.
        expected: usize,
        /// Actual total packet payload length, including the identifier.
        actual: usize,
    },
    /// A `COM_STMT_EXECUTE` flags byte requests an unsupported cursor mode.
    UnsupportedStmtExecuteFlags {
        /// Flags received from the client.
        flags: u8,
    },
    /// A `COM_STMT_EXECUTE` packet requested more or fewer than one iteration.
    InvalidStmtExecuteIterationCount {
        /// Iteration count received from the client.
        iteration_count: u32,
    },
    /// A text command had no text after its identifier.
    EmptyText {
        /// Command identifier.
        command: u8,
        /// Name of the text field.
        field: &'static str,
    },
    /// A text command contained a NUL byte.
    EmbeddedNul {
        /// Command identifier.
        command: u8,
        /// Name of the text field.
        field: &'static str,
        /// Offset within the text body.
        offset: usize,
    },
    /// A text command was not valid UTF-8.
    InvalidUtf8 {
        /// Command identifier.
        command: u8,
        /// Name of the text field.
        field: &'static str,
    },
    /// A prepared-statement command is identified but not implemented here.
    UnsupportedPreparedStatement {
        /// Prepared-statement command identifier.
        command: u8,
    },
    /// The command identifier is outside this decoder's supported set.
    UnsupportedCommand {
        /// Unsupported command identifier.
        command: u8,
    },
}

impl From<InitialHandshakeError> for ConnectionStateError {
    fn from(error: InitialHandshakeError) -> Self {
        Self::InitialHandshake(error)
    }
}

impl From<InitialHandshakeNonceError> for ConnectionStateError {
    fn from(error: InitialHandshakeNonceError) -> Self {
        Self::InitialHandshakeNonce(error)
    }
}

impl From<ClientHandshakeResponseError> for ConnectionStateError {
    fn from(error: ClientHandshakeResponseError) -> Self {
        Self::ClientHandshakeResponse(error)
    }
}

impl From<CommandPacketError> for ConnectionStateError {
    fn from(error: CommandPacketError) -> Self {
        Self::Command(error)
    }
}

impl From<ClientSslRequestError> for ConnectionStateError {
    fn from(error: ClientSslRequestError) -> Self {
        Self::SslRequest(error)
    }
}

impl From<AuthPacketError> for ConnectionStateError {
    fn from(error: AuthPacketError) -> Self {
        Self::AuthPacket(error)
    }
}

impl From<PacketCodecError> for ConnectionStateError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl From<ResponsePacketError> for ConnectionStateError {
    fn from(error: ResponsePacketError) -> Self {
        Self::ResponsePacket(error)
    }
}

impl fmt::Display for ConnectionStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { state, event } => {
                write!(f, "cannot apply {event:?} while connection is in {state:?}")
            }
            Self::InitialHandshake(error) => write!(f, "initial handshake error: {error}"),
            Self::InitialHandshakeNonce(error) => {
                write!(f, "initial handshake nonce error: {error}")
            }
            Self::ClientHandshakeResponse(error) => {
                write!(f, "client handshake response error: {error}")
            }
            Self::CapabilityNotAdvertised {
                server,
                client,
                unsupported,
            } => write!(
                f,
                "client capabilities 0x{client:08x} include bits 0x{unsupported:08x} absent from server capabilities 0x{server:08x}"
            ),
            Self::UnsupportedAuthenticationPlugin { plugin } => {
                write!(f, "unsupported authentication plugin {plugin:?}")
            }
            Self::SecureTransportRequired => {
                f.write_str("authentication requires a secure transport")
            }
            Self::CommandBeforeReady { state } => {
                write!(
                    f,
                    "commands are not allowed while connection is in {state:?}"
                )
            }
            Self::Command(error) => write!(f, "command packet error: {error}"),
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::ResponsePacket(error) => write!(f, "response packet error: {error}"),
            Self::ResponsePayloadLimitTooSmall { limit, minimum } => write!(
                f,
                "server response payload limit {limit} is below required minimum {minimum}"
            ),
            Self::SslRequest(error) => write!(f, "SSLRequest error: {error}"),
            Self::AuthPacket(error) => write!(f, "authentication packet error: {error}"),
            Self::UnexpectedSequenceId {
                event,
                expected,
                actual,
            } => write!(
                f,
                "{event:?} packet sequence id is {actual}, expected {expected}"
            ),
            Self::TlsRequestRequired => {
                f.write_str("CLIENT_SSL requires a preceding SSLRequest packet")
            }
            Self::TlsResponseMissingSslCapability => {
                f.write_str("post-TLS client response must retain CLIENT_SSL")
            }
            Self::CapabilitiesChangedAfterTls {
                ssl_request,
                response,
            } => write!(
                f,
                "client capabilities changed after TLS: SSLRequest 0x{ssl_request:08x}, response 0x{response:08x}"
            ),
            Self::ClientResponseRequired => {
                f.write_str("the full client response is required before authentication")
            }
            Self::SslRequestRequired => {
                f.write_str("the post-TLS client response requires a preceding SSLRequest")
            }
            Self::AuthenticationRejected => f.write_str("authentication was rejected"),
            Self::InitialDatabaseSelectorRequired => f.write_str(
                "initial database selection is required before authentication completes",
            ),
            Self::CredentialVerification(error) => {
                write!(f, "credential verification failed: {error}")
            }
        }
    }
}

impl Error for ConnectionStateError {}

impl From<PacketCodecError> for CommandPacketError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl fmt::Display for CommandPacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::EmptyPayload => f.write_str("command packet payload is empty"),
            Self::PayloadTooLarge { length, limit } => {
                write!(f, "command payload {length} exceeds limit {limit}")
            }
            Self::UnexpectedSequenceId { expected, actual } => write!(
                f,
                "command packet sequence id is {actual}, expected {expected}"
            ),
            Self::InvalidPayloadLength {
                command,
                expected,
                actual,
            } => write!(
                f,
                "command 0x{command:02x} payload length is {actual}, expected {expected}"
            ),
            Self::UnsupportedStmtExecuteFlags { flags } => write!(
                f,
                "COM_STMT_EXECUTE flags 0x{flags:02x} are unsupported; only CURSOR_TYPE_NO_CURSOR is accepted"
            ),
            Self::InvalidStmtExecuteIterationCount { iteration_count } => write!(
                f,
                "COM_STMT_EXECUTE iteration count is {iteration_count}, expected 1"
            ),
            Self::EmptyText { command, field } => {
                write!(f, "command 0x{command:02x} {field} must not be empty")
            }
            Self::EmbeddedNul {
                command,
                field,
                offset,
            } => write!(
                f,
                "command 0x{command:02x} {field} contains an embedded NUL at byte {offset}"
            ),
            Self::InvalidUtf8 { command, field } => {
                write!(f, "command 0x{command:02x} {field} is not valid UTF-8")
            }
            Self::UnsupportedPreparedStatement { command } => write!(
                f,
                "prepared-statement command 0x{command:02x} is not supported"
            ),
            Self::UnsupportedCommand { command } => {
                write!(f, "command 0x{command:02x} is not supported")
            }
        }
    }
}

impl Error for CommandPacketError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AuthOkPacket, CachingSha2Verifier, ClientHandshakeResponseConfig, ClientSslRequestConfig,
        CredentialProvider, CredentialProviderError, CredentialVerificationError, ErrPacket,
        FrontendErrorKind, InMemoryCredentialProvider, InitialHandshake, StoredCredential,
        CLIENT_CONNECT_WITH_DB, DEFAULT_UTF8MB4_COLLATION, MAX_FULL_AUTH_RESPONSE_LENGTH,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES, REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES,
    };
    use sha2::{Digest, Sha256};

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
    };

    fn server_config() -> InitialHandshakeSettings {
        InitialHandshakeSettings {
            capability_flags: REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES,
            ..InitialHandshakeSettings::default()
        }
    }

    fn server_config_with_nonce() -> InitialHandshakeSettings {
        InitialHandshakeSettings {
            capability_flags: REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES,
            ..InitialHandshakeSettings::default()
        }
    }

    fn server_config_with_database() -> InitialHandshakeSettings {
        InitialHandshakeSettings {
            capability_flags: REQUIRED_INITIAL_HANDSHAKE_CAPABILITIES | CLIENT_CONNECT_WITH_DB,
            ..InitialHandshakeSettings::default()
        }
    }

    fn client_response(capability_flags: u32) -> ClientHandshakeResponse {
        client_response_with_sequence(capability_flags, CLIENT_HANDSHAKE_SEQUENCE_ID)
    }

    fn client_response_with_sequence(
        capability_flags: u32,
        sequence_id: u8,
    ) -> ClientHandshakeResponse {
        let config = ClientHandshakeResponseConfig::new(
            capability_flags,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        );
        ClientHandshakeResponse::decode(CODEC, &config.encode(CODEC, sequence_id).unwrap()).unwrap()
    }

    fn ssl_request(capability_flags: u32) -> ClientSslRequest {
        let config = ClientSslRequestConfig::new(capability_flags, 0, DEFAULT_UTF8MB4_COLLATION);
        ClientSslRequest::decode(CODEC, &config.encode(CODEC, 1).unwrap()).unwrap()
    }

    fn ssl_request_frame(capability_flags: u32, sequence_id: u8) -> Vec<u8> {
        ClientSslRequestConfig::new(capability_flags, 0, DEFAULT_UTF8MB4_COLLATION)
            .encode(CODEC, sequence_id)
            .unwrap()
    }

    fn client_response_frame(capability_flags: u32, sequence_id: u8) -> Vec<u8> {
        ClientHandshakeResponseConfig::new(
            capability_flags,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(CODEC, sequence_id)
        .unwrap()
    }

    fn ready_connection() -> ClassicConnection {
        let mut connection = secure_handshake_connection();
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        connection.send_authentication_ok().unwrap();
        connection
    }

    fn secure_handshake_connection() -> ClassicConnection {
        secure_handshake_connection_with_nonce_and_auth(
            [0xa5; AUTH_PLUGIN_DATA_LENGTH],
            [0; 32].as_slice(),
        )
    }

    fn secure_handshake_connection_with_nonce_and_auth(
        auth_plugin_data: [u8; AUTH_PLUGIN_DATA_LENGTH],
        auth_response: &[u8],
    ) -> ClassicConnection {
        let mut connection = ClassicConnection::with_test_nonce(
            server_config_with_nonce(),
            CODEC,
            TransportSecurity::Secure,
            auth_plugin_data,
        )
        .unwrap();
        connection.send_initial_handshake().unwrap();
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            auth_response.to_vec(),
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(CODEC, CLIENT_HANDSHAKE_SEQUENCE_ID)
        .unwrap();
        connection
            .receive_client_handshake_frame(&response)
            .unwrap();
        connection
    }

    fn secure_handshake_connection_with_database(database: &str) -> ClassicConnection {
        let mut connection = ClassicConnection::with_test_nonce(
            server_config_with_database(),
            CODEC,
            TransportSecurity::Secure,
            [0xa6; AUTH_PLUGIN_DATA_LENGTH],
        )
        .unwrap();
        connection.send_initial_handshake().unwrap();
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_CONNECT_WITH_DB,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0; 32],
            Some(database),
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(CODEC, CLIENT_HANDSHAKE_SEQUENCE_ID)
        .unwrap();
        connection
            .receive_client_handshake_frame(&response)
            .unwrap();
        connection
    }

    #[derive(Debug)]
    struct TestDatabaseSelector {
        result: Result<(), FrontendErrorKind>,
        calls: Vec<String>,
    }

    impl TestDatabaseSelector {
        fn accepting() -> Self {
            Self {
                result: Ok(()),
                calls: Vec::new(),
            }
        }

        fn rejecting(kind: FrontendErrorKind) -> Self {
            Self {
                result: Err(kind),
                calls: Vec::new(),
            }
        }
    }

    impl InitialDatabaseSelector for TestDatabaseSelector {
        fn select_initial_database(&mut self, database: &str) -> Result<(), FrontendErrorKind> {
            self.calls.push(database.to_owned());
            self.result
        }
    }

    #[test]
    fn production_connections_get_distinct_nonces_from_reused_settings() {
        let settings = server_config();
        let mut first =
            ClassicConnection::with_transport_security(settings.clone(), TransportSecurity::Secure)
                .unwrap();
        let mut second =
            ClassicConnection::with_transport_security(settings, TransportSecurity::Secure)
                .unwrap();
        let first_handshake =
            InitialHandshake::decode(CODEC, &first.send_initial_handshake().unwrap()).unwrap();
        let second_handshake =
            InitialHandshake::decode(CODEC, &second.send_initial_handshake().unwrap()).unwrap();
        assert!(first_handshake
            .auth_plugin_data
            .iter()
            .any(|byte| *byte != 0));
        assert!(second_handshake
            .auth_plugin_data
            .iter()
            .any(|byte| *byte != 0));
        assert_ne!(
            first_handshake.auth_plugin_data,
            second_handshake.auth_plugin_data
        );
    }

    #[test]
    fn nonce_source_failure_prevents_connection_creation_and_handshake_output() {
        #[derive(Debug)]
        struct FailingNonceSource;

        impl HandshakeNonceSource for FailingNonceSource {
            fn fill_nonce(
                &mut self,
                _nonce: &mut [u8; AUTH_PLUGIN_DATA_LENGTH],
            ) -> Result<(), InitialHandshakeNonceError> {
                Err(InitialHandshakeNonceError::OsRandomUnavailable)
            }
        }

        let mut source = FailingNonceSource;
        assert_eq!(
            ClassicConnection::with_test_nonce_source(
                server_config(),
                CODEC,
                TransportSecurity::Secure,
                &mut source,
            ),
            Err(ConnectionStateError::InitialHandshakeNonce(
                InitialHandshakeNonceError::OsRandomUnavailable
            ))
        );
    }

    fn sha256_digest(input: &[u8]) -> [u8; 32] {
        let digest = Sha256::digest(input);
        let mut output = [0; 32];
        output.copy_from_slice(&digest);
        output
    }

    fn verifier_material(password: &[u8]) -> [u8; 32] {
        let stage_one = sha256_digest(password);
        sha256_digest(&stage_one)
    }

    fn fast_auth_response(password: &[u8], scramble: &[u8; AUTH_PLUGIN_DATA_LENGTH]) -> [u8; 32] {
        let stage_one = sha256_digest(password);
        let stage_two = sha256_digest(&stage_one);
        let stage_three = sha256_digest(&stage_two);
        let mut challenge = [0; 32 + AUTH_PLUGIN_DATA_LENGTH];
        challenge[..32].copy_from_slice(&stage_three);
        challenge[32..].copy_from_slice(scramble);
        let mask = sha256_digest(&challenge);
        let mut response = [0; 32];
        for (output, (&password_hash, &mask_byte)) in
            response.iter_mut().zip(stage_one.iter().zip(mask.iter()))
        {
            *output = password_hash ^ mask_byte;
        }
        response
    }

    #[test]
    fn connection_and_handshake_debug_redact_authentication_bytes() {
        let token = b"distinctive-auth-token".to_vec();
        let response_config = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "root",
            token,
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        );
        let response = ClientHandshakeResponse::decode(
            CODEC,
            &response_config
                .encode(CODEC, CLIENT_HANDSHAKE_SEQUENCE_ID)
                .unwrap(),
        )
        .unwrap();
        let mut connection =
            ClassicConnection::with_transport_security(server_config(), TransportSecurity::Secure)
                .unwrap();
        let response_debug = format!("{response:?}");
        connection.send_initial_handshake().unwrap();
        connection
            .receive_client_handshake_response(response)
            .unwrap();

        let connection_debug = format!("{connection:?}");
        let config_debug = format!("{response_config:?}");
        assert!(!connection_debug.contains("distinctive-auth-token"));
        assert!(!response_debug.contains("distinctive-auth-token"));
        assert!(!config_debug.contains("distinctive-auth-token"));
    }

    #[test]
    fn follows_secure_handshake_to_ready_and_rejects_early_commands() {
        let mut connection =
            ClassicConnection::with_transport_security(server_config(), TransportSecurity::Secure)
                .unwrap();
        assert_eq!(connection.state(), ConnectionState::SendInitialHandshake);
        let ping = CODEC.encode(COMMAND_SEQUENCE_ID, b"\x0e").unwrap();
        assert!(matches!(
            connection.receive_command_frame(&ping),
            Err(ConnectionStateError::CommandBeforeReady {
                state: ConnectionState::SendInitialHandshake
            })
        ));

        let frame = connection.send_initial_handshake().unwrap();
        let handshake = InitialHandshake::decode(CODEC, &frame).unwrap();
        assert_eq!(handshake.sequence_id, 0);
        assert_eq!(handshake.connection_id, server_config().connection_id);
        assert!(handshake.auth_plugin_data.iter().any(|byte| *byte != 0));
        assert_eq!(connection.state(), ConnectionState::AwaitClientResponse);
        connection
            .receive_client_handshake_response(client_response(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            ))
            .unwrap();
        assert_eq!(
            connection.state(),
            ConnectionState::AuthenticateCachingSha2Password
        );
        assert!(connection.ensure_ready().is_err());
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        assert!(connection.ensure_ready().is_err());
        assert!(matches!(
            connection.receive_command_frame(&ping),
            Err(ConnectionStateError::CommandBeforeReady {
                state: ConnectionState::AuthenticateFast
            })
        ));
        connection.send_authentication_ok().unwrap();
        assert_eq!(connection.state(), ConnectionState::Ready);
        assert!(connection.ensure_ready().is_ok());
    }

    #[test]
    fn fast_auth_requires_external_verification_then_sends_auth_more_data_and_ok() {
        let mut connection = secure_handshake_connection();
        let request = connection.authentication_verification_request().unwrap();
        assert_eq!(request.username, "root");
        assert_eq!(
            request.stage,
            AuthenticationVerificationStage::InitialHandshakeResponse
        );
        assert_eq!(request.plugin_name, CACHING_SHA2_PASSWORD_PLUGIN);
        assert!(request
            .server_auth_plugin_data
            .iter()
            .any(|byte| *byte != 0));
        assert_eq!(request.auth_response, [0; 32]);

        let more_data = connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        assert_eq!(
            AuthMoreData::decode(CODEC, &more_data).unwrap(),
            AuthMoreData {
                sequence_id: 2,
                kind: AuthMoreDataKind::FastAuthSuccess,
            }
        );
        assert_eq!(connection.state(), ConnectionState::AuthenticateFast);

        let ok = connection.send_authentication_ok().unwrap();
        let ok_packet = AuthOkPacket::decode(CODEC, &ok).unwrap();
        assert_eq!(ok_packet.sequence_id, 3);
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn fast_auth_selects_initial_database_before_ok_and_returns_typed_err_on_failure() {
        let mut success = secure_handshake_connection_with_database("reports");
        assert_eq!(success.initial_database(), Some("reports"));
        success
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();

        let mut selector = TestDatabaseSelector::accepting();
        let response = success
            .send_authentication_ok_with_selector(&mut selector)
            .unwrap();
        assert_eq!(selector.calls, vec!["reports".to_owned()]);
        let AuthenticationResponse::Ok(frame) = response else {
            panic!("successful database selection must produce OK");
        };
        assert_eq!(AuthOkPacket::decode(CODEC, &frame).unwrap().sequence_id, 3);
        assert_eq!(success.state(), ConnectionState::Ready);

        let mut failure = secure_handshake_connection_with_database("private_reports");
        failure
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        assert_eq!(
            failure.send_authentication_ok(),
            Err(ConnectionStateError::InitialDatabaseSelectorRequired)
        );
        assert_eq!(failure.state(), ConnectionState::Closing);

        let mut failure = secure_handshake_connection_with_database("private_reports");
        failure
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        let mut selector = TestDatabaseSelector::rejecting(FrontendErrorKind::UnknownDatabase);
        let response = failure
            .send_authentication_ok_with_selector(&mut selector)
            .unwrap();
        assert_eq!(selector.calls, vec!["private_reports".to_owned()]);
        let AuthenticationResponse::Err { kind, frame } = response else {
            panic!("failed database selection must produce ERR");
        };
        assert_eq!(kind, FrontendErrorKind::UnknownDatabase);
        let error = ErrPacket::decode(
            CODEC,
            &frame,
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_CONNECT_WITH_DB,
        )
        .unwrap();
        assert_eq!(error.sequence_id, 3);
        assert_eq!(error.error_code, 1049);
        assert_eq!(error.message, b"unknown database");
        assert!(!error
            .message
            .windows(b"private_reports".len())
            .any(|window| { window == b"private_reports" }));
        assert_eq!(failure.state(), ConnectionState::Closing);
    }

    #[test]
    fn verifier_and_apply_complete_a_fast_cache_hit_then_require_final_ok() {
        let password = b"secret";
        let nonce = [0x11; AUTH_PLUGIN_DATA_LENGTH];
        let auth_response = fast_auth_response(password, &nonce);
        let mut connection = secure_handshake_connection_with_nonce_and_auth(nonce, &auth_response);
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);

        let more_data = connection
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        assert_eq!(
            AuthMoreData::decode(CODEC, &more_data).unwrap().kind,
            AuthMoreDataKind::FastAuthSuccess
        );
        assert_eq!(connection.state(), ConnectionState::AuthenticateFast);
        let ok = connection.send_authentication_ok().unwrap();
        assert_eq!(AuthOkPacket::decode(CODEC, &ok).unwrap().sequence_id, 3);
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn verifier_and_apply_turn_a_secure_cache_miss_into_full_authentication() {
        let password = b"secret";
        let nonce = [0x12; AUTH_PLUGIN_DATA_LENGTH];
        let mut connection = secure_handshake_connection_with_nonce_and_auth(nonce, &[0; 32]);
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);

        let more_data = connection
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        assert_eq!(
            AuthMoreData::decode(CODEC, &more_data).unwrap().kind,
            AuthMoreDataKind::FullAuthenticationRequired
        );
        assert_eq!(connection.state(), ConnectionState::AuthenticateFull);
        let full_response = CODEC.encode_client_auth_response(3, password).unwrap();
        let ok = connection
            .verify_and_apply_full_authentication(&full_response, &verifier)
            .unwrap();
        assert_eq!(AuthOkPacket::decode(CODEC, &ok).unwrap().sequence_id, 4);
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn full_auth_selects_initial_database_before_ok_and_old_api_cannot_bypass_it() {
        let password = b"secret";
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);

        let mut success = secure_handshake_connection_with_database("reports");
        success
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        let full_response = CODEC.encode_client_auth_response(3, password).unwrap();
        let mut selector = TestDatabaseSelector::accepting();
        let response = success
            .verify_and_apply_full_authentication_with_selector(
                &full_response,
                &verifier,
                &mut selector,
            )
            .unwrap();
        assert_eq!(selector.calls, vec!["reports".to_owned()]);
        let AuthenticationResponse::Ok(frame) = response else {
            panic!("successful database selection must produce OK");
        };
        assert_eq!(AuthOkPacket::decode(CODEC, &frame).unwrap().sequence_id, 4);
        assert_eq!(success.state(), ConnectionState::Ready);

        let mut failure = secure_handshake_connection_with_database("missing_reports");
        failure
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        let full_response = CODEC.encode_client_auth_response(3, password).unwrap();
        let mut selector = TestDatabaseSelector::rejecting(FrontendErrorKind::UnknownDatabase);
        let response = failure
            .verify_and_apply_full_authentication_with_selector(
                &full_response,
                &verifier,
                &mut selector,
            )
            .unwrap();
        assert_eq!(selector.calls, vec!["missing_reports".to_owned()]);
        let AuthenticationResponse::Err { kind, frame } = response else {
            panic!("failed database selection must produce ERR");
        };
        assert_eq!(kind, FrontendErrorKind::UnknownDatabase);
        let error = ErrPacket::decode(
            CODEC,
            &frame,
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_CONNECT_WITH_DB,
        )
        .unwrap();
        assert_eq!(error.sequence_id, 4);
        assert_eq!(error.error_code, 1049);
        assert_eq!(error.message, b"unknown database");
        assert_eq!(failure.state(), ConnectionState::Closing);

        let mut bypass = secure_handshake_connection_with_database("reports");
        bypass
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        let full_response = CODEC.encode_client_auth_response(3, password).unwrap();
        assert_eq!(
            bypass.verify_and_apply_full_authentication(&full_response, &verifier),
            Err(ConnectionStateError::InitialDatabaseSelectorRequired)
        );
        assert_eq!(bypass.state(), ConnectionState::Closing);
    }

    #[test]
    fn rejected_credentials_never_call_initial_database_selector() {
        let password = b"secret";
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(false, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);
        let mut connection = secure_handshake_connection_with_database("reports");
        connection
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        let full_response = CODEC.encode_client_auth_response(3, password).unwrap();
        let mut selector = TestDatabaseSelector::accepting();
        assert_eq!(
            connection.verify_and_apply_full_authentication_with_selector(
                &full_response,
                &verifier,
                &mut selector,
            ),
            Err(ConnectionStateError::AuthenticationRejected)
        );
        assert!(selector.calls.is_empty());
        assert_eq!(connection.state(), ConnectionState::Closing);
    }

    #[test]
    fn provider_failure_closes_without_emitting_a_protocol_error() {
        #[derive(Debug)]
        struct FailingProvider;
        impl CredentialProvider for FailingProvider {
            fn lookup(
                &self,
                _username: &str,
            ) -> Result<Option<crate::CredentialSnapshot>, CredentialProviderError> {
                Err(CredentialProviderError::BackendFailure)
            }
        }

        let mut connection = secure_handshake_connection();
        let verifier = CachingSha2Verifier::new(FailingProvider);
        assert_eq!(
            connection.verify_and_apply_initial_authentication(&verifier),
            Err(ConnectionStateError::CredentialVerification(
                CredentialVerificationError::Provider(CredentialProviderError::BackendFailure)
            ))
        );
        assert_eq!(connection.state(), ConnectionState::Closing);
    }

    #[test]
    fn empty_password_uses_full_auth_and_accepts_nul_only_packet() {
        let nonce = [0x13; AUTH_PLUGIN_DATA_LENGTH];
        let mut connection = secure_handshake_connection_with_nonce_and_auth(nonce, &[]);
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(&[])),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);

        let more_data = connection
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        assert_eq!(
            AuthMoreData::decode(CODEC, &more_data).unwrap().kind,
            AuthMoreDataKind::FullAuthenticationRequired
        );
        let nul_only = CODEC.encode_client_auth_response(3, &[]).unwrap();
        let ok = connection
            .verify_and_apply_full_authentication(&nul_only, &verifier)
            .unwrap();
        assert_eq!(AuthOkPacket::decode(CODEC, &ok).unwrap().sequence_id, 4);
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn cached_response_cannot_replay_against_a_different_server_nonce() {
        let password = b"secret";
        let original_nonce = [0x21; AUTH_PLUGIN_DATA_LENGTH];
        let different_nonce = [0x22; AUTH_PLUGIN_DATA_LENGTH];
        let auth_response = fast_auth_response(password, &original_nonce);
        let mut connection =
            secure_handshake_connection_with_nonce_and_auth(different_nonce, &auth_response);
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);

        let more_data = connection
            .verify_and_apply_initial_authentication(&verifier)
            .unwrap();
        assert_eq!(
            AuthMoreData::decode(CODEC, &more_data).unwrap().kind,
            AuthMoreDataKind::FullAuthenticationRequired
        );
        assert_eq!(connection.state(), ConnectionState::AuthenticateFull);
    }

    #[test]
    fn full_authentication_borrows_password_and_finishes_with_ok() {
        let mut connection = secure_handshake_connection();
        let more_data = connection
            .apply_initial_authentication_result(
                InitialAuthenticationResult::FullAuthenticationRequired,
            )
            .unwrap();
        assert_eq!(
            AuthMoreData::decode(CODEC, &more_data).unwrap().kind,
            AuthMoreDataKind::FullAuthenticationRequired
        );
        assert_eq!(connection.state(), ConnectionState::AuthenticateFull);

        let full_response = CODEC.encode_client_auth_response(3, b"secret").unwrap();
        let request = connection
            .receive_full_authentication_frame(&full_response)
            .unwrap();
        assert_eq!(request.username, "root");
        assert_eq!(request.plugin_name, CACHING_SHA2_PASSWORD_PLUGIN);
        assert_eq!(
            request.stage,
            AuthenticationVerificationStage::FullAuthenticationResponse
        );
        assert_eq!(request.auth_response, b"secret");
        assert_eq!(
            connection.state(),
            ConnectionState::AuthenticateFullVerification
        );

        let ok = connection
            .apply_full_authentication_result(FullAuthenticationResult::Authenticated)
            .unwrap();
        assert_eq!(AuthOkPacket::decode(CODEC, &ok).unwrap().sequence_id, 4);
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn authentication_rejection_closes_without_exposing_credentials() {
        let mut fast = secure_handshake_connection();
        assert_eq!(
            fast.apply_initial_authentication_result(InitialAuthenticationResult::Rejected),
            Err(ConnectionStateError::AuthenticationRejected)
        );
        assert_eq!(fast.state(), ConnectionState::Closing);

        let mut full = secure_handshake_connection();
        full.apply_initial_authentication_result(
            InitialAuthenticationResult::FullAuthenticationRequired,
        )
        .unwrap();
        let response = CODEC.encode_client_auth_response(3, b"secret").unwrap();
        full.receive_full_authentication_frame(&response).unwrap();
        assert_eq!(
            full.apply_full_authentication_result(FullAuthenticationResult::Rejected),
            Err(ConnectionStateError::AuthenticationRejected)
        );
        assert_eq!(full.state(), ConnectionState::Closing);
    }

    #[test]
    fn authentication_rejects_wrong_sequence_state_and_oversized_response() {
        let mut connection = secure_handshake_connection();
        assert!(matches!(
            connection.send_authentication_ok(),
            Err(ConnectionStateError::InvalidTransition {
                state: ConnectionState::AuthenticateCachingSha2Password,
                event: ConnectionEvent::SendAuthenticationOk
            })
        ));
        assert!(matches!(
            connection.receive_full_authentication_frame(&[0; 4]),
            Err(ConnectionStateError::InvalidTransition {
                state: ConnectionState::AuthenticateCachingSha2Password,
                event: ConnectionEvent::ReceiveClientAuthResponse
            })
        ));
        connection
            .apply_initial_authentication_result(
                InitialAuthenticationResult::FullAuthenticationRequired,
            )
            .unwrap();

        let wrong_sequence = CODEC.encode_client_auth_response(2, b"secret").unwrap();
        assert!(matches!(
            connection.receive_full_authentication_frame(&wrong_sequence),
            Err(ConnectionStateError::UnexpectedSequenceId {
                event: ConnectionEvent::ReceiveClientAuthResponse,
                expected: 3,
                actual: 2
            })
        ));
        assert_eq!(connection.state(), ConnectionState::AuthenticateFull);

        let oversized = vec![b'x'; MAX_FULL_AUTH_RESPONSE_LENGTH + 1];
        let oversized_frame = CODEC
            .encode(3, &oversized)
            .expect("test frame is below the packet codec limit");
        assert!(matches!(
            connection.receive_full_authentication_frame(&oversized_frame),
            Err(ConnectionStateError::AuthPacket(
                AuthPacketError::PayloadTooLarge { length, limit }
            )) if length == MAX_FULL_AUTH_RESPONSE_LENGTH + 1
                && limit == MAX_FULL_AUTH_RESPONSE_LENGTH
        ));
        assert_eq!(connection.state(), ConnectionState::AuthenticateFull);
    }

    #[test]
    fn requires_external_tls_event_before_authentication() {
        let mut config = server_config();
        config.capability_flags |= CLIENT_SSL;
        let mut connection =
            ClassicConnection::with_transport_security(config, TransportSecurity::Plaintext)
                .unwrap();
        connection.send_initial_handshake().unwrap();
        connection
            .receive_client_ssl_request(ssl_request(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            ))
            .unwrap();
        assert_eq!(connection.state(), ConnectionState::TlsUpgradeRequired);
        assert!(matches!(
            connection.send_authentication_ok(),
            Err(ConnectionStateError::InvalidTransition {
                state: ConnectionState::TlsUpgradeRequired,
                event: ConnectionEvent::SendAuthenticationOk
            })
        ));
        connection.tls_upgrade_complete().unwrap();
        assert_eq!(connection.state(), ConnectionState::TlsNegotiated);
        connection
            .receive_client_handshake_response(client_response_with_sequence(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
                TLS_CLIENT_HANDSHAKE_SEQUENCE_ID,
            ))
            .unwrap();
        connection.begin_authentication().unwrap();
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        connection.send_authentication_ok().unwrap();
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn follows_real_tls_packet_sequence_with_ssl_request_first() {
        let mut config = server_config();
        config.capability_flags |= CLIENT_SSL;
        let mut connection =
            ClassicConnection::with_transport_security(config, TransportSecurity::Plaintext)
                .unwrap();
        connection.send_initial_handshake().unwrap();
        connection
            .receive_client_handshake_frame(&ssl_request_frame(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
                CLIENT_HANDSHAKE_SEQUENCE_ID,
            ))
            .unwrap();
        assert_eq!(connection.state(), ConnectionState::TlsUpgradeRequired);
        connection.tls_upgrade_complete().unwrap();
        connection
            .receive_client_handshake_frame(&client_response_frame(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
                TLS_CLIENT_HANDSHAKE_SEQUENCE_ID,
            ))
            .unwrap();
        assert_eq!(connection.state(), ConnectionState::TlsNegotiated);
        connection.begin_authentication().unwrap();
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        connection.send_authentication_ok().unwrap();
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn default_constructor_does_not_allow_plaintext_authentication() {
        let mut connection = ClassicConnection::new(server_config()).unwrap();
        connection.send_initial_handshake().unwrap();
        assert!(matches!(
            connection.receive_client_handshake_response(client_response(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )),
            Err(ConnectionStateError::SecureTransportRequired)
        ));
        assert_eq!(connection.state(), ConnectionState::AwaitClientResponse);
    }

    #[test]
    fn rejects_client_limits_and_collations_before_authentication() {
        let mut connection =
            ClassicConnection::with_transport_security(server_config(), TransportSecurity::Secure)
                .unwrap();
        connection.send_initial_handshake().unwrap();

        let mut too_small = client_response(REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES);
        too_small.max_packet_size = MIN_SERVER_RESPONSE_PAYLOAD_LENGTH - 1;
        assert!(matches!(
            connection.receive_client_handshake_response(too_small),
            Err(ConnectionStateError::ClientHandshakeResponse(
                ClientHandshakeResponseError::MaxPacketSizeTooSmall {
                    max_packet_size,
                    ..
                }
            )) if max_packet_size == MIN_SERVER_RESPONSE_PAYLOAD_LENGTH - 1
        ));
        assert_eq!(connection.state(), ConnectionState::AwaitClientResponse);

        let mut unsupported_charset =
            client_response(REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES);
        unsupported_charset.character_set = 33;
        assert!(matches!(
            connection.receive_client_handshake_response(unsupported_charset),
            Err(ConnectionStateError::ClientHandshakeResponse(
                ClientHandshakeResponseError::UnsupportedCharacterSet { character_set: 33 }
            ))
        ));
        assert_eq!(connection.state(), ConnectionState::AwaitClientResponse);

        let mut ssl = ssl_request(REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL);
        ssl.max_packet_size = MIN_SERVER_RESPONSE_PAYLOAD_LENGTH - 1;
        assert!(matches!(
            connection.receive_client_ssl_request(ssl),
            Err(ConnectionStateError::SslRequest(
                ClientSslRequestError::MaxPacketSizeTooSmall {
                    max_packet_size,
                    ..
                }
            )) if max_packet_size == MIN_SERVER_RESPONSE_PAYLOAD_LENGTH - 1
        ));
        assert_eq!(connection.state(), ConnectionState::AwaitClientResponse);
    }

    #[test]
    fn rejects_wrong_handshake_sequences_and_malformed_ssl_requests() {
        let mut ordinary =
            ClassicConnection::with_transport_security(server_config(), TransportSecurity::Secure)
                .unwrap();
        ordinary.send_initial_handshake().unwrap();
        assert!(matches!(
            ordinary.receive_client_handshake_response(client_response_with_sequence(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
                0,
            )),
            Err(ConnectionStateError::UnexpectedSequenceId {
                event: ConnectionEvent::ReceiveClientResponse,
                expected: CLIENT_HANDSHAKE_SEQUENCE_ID,
                actual: 0,
            })
        ));

        let mut tls_config = server_config();
        tls_config.capability_flags |= CLIENT_SSL;
        let mut tls =
            ClassicConnection::with_transport_security(tls_config, TransportSecurity::Plaintext)
                .unwrap();
        tls.send_initial_handshake().unwrap();
        let wrong_sequence = ssl_request_frame(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
        );
        assert!(matches!(
            tls.receive_client_ssl_request_frame(&wrong_sequence),
            Err(ConnectionStateError::UnexpectedSequenceId {
                event: ConnectionEvent::ReceiveSslRequest,
                expected: CLIENT_HANDSHAKE_SEQUENCE_ID,
                actual: 0,
            })
        ));
        let mut malformed = ssl_request_frame(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            CLIENT_HANDSHAKE_SEQUENCE_ID,
        );
        malformed[crate::PACKET_HEADER_LEN + 9] = 1;
        assert!(matches!(
            tls.receive_client_ssl_request_frame(&malformed),
            Err(ConnectionStateError::SslRequest(
                ClientSslRequestError::NonZeroReservedBytes
            ))
        ));
        let truncated = CODEC
            .encode(CLIENT_HANDSHAKE_SEQUENCE_ID, &[0; 31])
            .unwrap();
        assert!(matches!(
            tls.receive_client_ssl_request_frame(&truncated),
            Err(ConnectionStateError::SslRequest(
                ClientSslRequestError::InvalidPayloadLength {
                    actual: 31,
                    expected: CLIENT_SSL_REQUEST_PAYLOAD_LENGTH,
                }
            ))
        ));
        assert_eq!(tls.state(), ConnectionState::AwaitClientResponse);
    }

    #[test]
    fn rejects_capability_mismatch_and_plaintext_authentication() {
        let mut config = server_config();
        config.capability_flags |= CLIENT_SSL;
        let mut connection =
            ClassicConnection::with_transport_security(config, TransportSecurity::Plaintext)
                .unwrap();
        connection.send_initial_handshake().unwrap();
        assert!(matches!(
            connection.receive_client_handshake_response(client_response(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )),
            Err(ConnectionStateError::SecureTransportRequired)
        ));
        assert_eq!(connection.state(), ConnectionState::AwaitClientResponse);

        let mut no_ssl = ClassicConnection::new(server_config()).unwrap();
        no_ssl.send_initial_handshake().unwrap();
        assert!(matches!(
            no_ssl.receive_client_handshake_response(client_response(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            )),
            Err(ConnectionStateError::CapabilityNotAdvertised { unsupported, .. })
                if unsupported == CLIENT_SSL
        ));
    }

    #[test]
    fn decodes_supported_commands_and_quit_starts_closing() {
        let mut connection = ready_connection();

        let query = CODEC.encode(0, b"\x03SELECT 1").unwrap();
        assert_eq!(
            connection.receive_command_frame(&query).unwrap(),
            ClassicCommandPacket {
                sequence_id: COMMAND_SEQUENCE_ID,
                command: ClassicCommand::Query { sql: "SELECT 1" },
            }
        );
        let init_db = CODEC.encode(0, b"\x02test_db").unwrap();
        assert_eq!(
            connection.receive_command_frame(&init_db).unwrap().command,
            ClassicCommand::InitDb {
                database: "test_db"
            }
        );
        let prepare = CODEC.encode(0, b"\x16SELECT ?").unwrap();
        assert_eq!(
            connection.receive_command_frame(&prepare).unwrap().command,
            ClassicCommand::StmtPrepare { sql: "SELECT ?" }
        );
        let ping = CODEC.encode(0, b"\x0e").unwrap();
        assert_eq!(
            connection.receive_command_frame(&ping).unwrap().command,
            ClassicCommand::Ping
        );
        assert_eq!(connection.state(), ConnectionState::Ready);

        let quit = CODEC.encode(0, b"\x01").unwrap();
        assert_eq!(
            connection.receive_command_frame(&quit).unwrap().command,
            ClassicCommand::Quit
        );
        assert_eq!(connection.state(), ConnectionState::Closing);
    }

    #[test]
    fn decodes_statement_close_and_reset_ids_as_little_endian_u32() {
        let mut connection = ready_connection();
        let close = CODEC
            .encode(
                COMMAND_SEQUENCE_ID,
                &[COM_STMT_CLOSE, 0x04, 0x03, 0x02, 0x01],
            )
            .unwrap();
        assert_eq!(
            connection.receive_command_frame(&close).unwrap().command,
            ClassicCommand::StmtClose {
                statement_id: 0x0102_0304,
            }
        );

        let reset = CODEC
            .encode(
                COMMAND_SEQUENCE_ID,
                &[COM_STMT_RESET, 0xd4, 0xc3, 0xb2, 0xa1],
            )
            .unwrap();
        assert_eq!(
            connection.receive_command_frame(&reset).unwrap().command,
            ClassicCommand::StmtReset {
                statement_id: 0xa1b2_c3d4,
            }
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn decodes_statement_execute_header_and_borrows_parameter_payload() {
        let mut connection = ready_connection();
        let mut payload = vec![COM_STMT_EXECUTE];
        payload.extend_from_slice(&0x0102_0304u32.to_le_bytes());
        payload.push(CURSOR_TYPE_NO_CURSOR);
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&[0x01, 0x02, 0x03]);
        let frame = CODEC.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();

        assert_eq!(
            connection.receive_command_frame(&frame).unwrap().command,
            ClassicCommand::StmtExecute {
                statement_id: 0x0102_0304,
                flags: CURSOR_TYPE_NO_CURSOR,
                iteration_count: 1,
                parameter_payload: &[0x01, 0x02, 0x03],
            }
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn decodes_statement_execute_without_parameter_payload() {
        let mut connection = ready_connection();
        let mut payload = vec![COM_STMT_EXECUTE];
        payload.extend_from_slice(&7u32.to_le_bytes());
        payload.push(CURSOR_TYPE_NO_CURSOR);
        payload.extend_from_slice(&1u32.to_le_bytes());
        let frame = CODEC.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();

        assert_eq!(
            connection.receive_command_frame(&frame).unwrap().command,
            ClassicCommand::StmtExecute {
                statement_id: 7,
                flags: CURSOR_TYPE_NO_CURSOR,
                iteration_count: 1,
                parameter_payload: &[],
            }
        );
    }

    #[test]
    fn rejects_statement_execute_with_malformed_fixed_body() {
        let mut connection = ready_connection();
        for body_length in 0..STMT_EXECUTE_FIXED_BODY_LENGTH {
            let mut payload = vec![COM_STMT_EXECUTE];
            payload.resize(payload.len() + body_length, 0);
            let frame = CODEC.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
            assert_eq!(
                connection.receive_command_frame(&frame),
                Err(ConnectionStateError::Command(
                    CommandPacketError::InvalidPayloadLength {
                        command: COM_STMT_EXECUTE,
                        expected: STMT_EXECUTE_FIXED_BODY_LENGTH + 1,
                        actual: body_length + 1,
                    }
                ))
            );
            assert_eq!(connection.state(), ConnectionState::Ready);
        }
    }

    #[test]
    fn rejects_statement_execute_with_unsupported_flags() {
        let mut connection = ready_connection();
        let mut payload = vec![COM_STMT_EXECUTE];
        payload.extend_from_slice(&7u32.to_le_bytes());
        payload.push(1);
        payload.extend_from_slice(&1u32.to_le_bytes());
        let frame = CODEC.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();

        assert_eq!(
            connection.receive_command_frame(&frame),
            Err(ConnectionStateError::Command(
                CommandPacketError::UnsupportedStmtExecuteFlags { flags: 1 }
            ))
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn rejects_statement_execute_with_invalid_iteration_count() {
        let mut connection = ready_connection();
        let mut payload = vec![COM_STMT_EXECUTE];
        payload.extend_from_slice(&7u32.to_le_bytes());
        payload.push(CURSOR_TYPE_NO_CURSOR);
        payload.extend_from_slice(&2u32.to_le_bytes());
        let frame = CODEC.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();

        assert_eq!(
            connection.receive_command_frame(&frame),
            Err(ConnectionStateError::Command(
                CommandPacketError::InvalidStmtExecuteIterationCount { iteration_count: 2 }
            ))
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn rejects_statement_close_and_reset_with_non_u32_bodies() {
        let mut connection = ready_connection();
        for (command, body) in [
            (COM_STMT_CLOSE, vec![0x01, 0x02, 0x03]),
            (COM_STMT_RESET, vec![0x01, 0x02, 0x03, 0x04, 0x05]),
        ] {
            let mut payload = vec![command];
            payload.extend_from_slice(&body);
            let frame = CODEC.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
            assert_eq!(
                connection.receive_command_frame(&frame),
                Err(ConnectionStateError::Command(
                    CommandPacketError::InvalidPayloadLength {
                        command,
                        expected: 5,
                        actual: body.len() + 1,
                    }
                ))
            );
            assert_eq!(connection.state(), ConnectionState::Ready);
        }
    }

    #[test]
    fn rejects_commands_before_ready_and_preserves_packet_sequence_rules() {
        let mut connection = ClassicConnection::new(server_config()).unwrap();
        let ping = CODEC.encode(COMMAND_SEQUENCE_ID, b"\x0e").unwrap();
        assert!(matches!(
            connection.receive_command_frame(&ping),
            Err(ConnectionStateError::CommandBeforeReady {
                state: ConnectionState::SendInitialHandshake
            })
        ));

        let mut connection = ready_connection();
        let wrong_sequence = CODEC.encode(1, b"\x0e").unwrap();
        assert!(matches!(
            connection.receive_command_frame(&wrong_sequence),
            Err(ConnectionStateError::Command(
                CommandPacketError::UnexpectedSequenceId {
                    expected: COMMAND_SEQUENCE_ID,
                    actual: 1
                }
            ))
        ));
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn rejects_malformed_command_payloads() {
        let mut connection = ready_connection();
        let cases = [
            (
                b"\x03".as_slice(),
                CommandPacketError::EmptyText {
                    command: COM_QUERY,
                    field: "query",
                },
            ),
            (
                b"\x02".as_slice(),
                CommandPacketError::EmptyText {
                    command: COM_INIT_DB,
                    field: "database",
                },
            ),
            (
                b"\x16".as_slice(),
                CommandPacketError::EmptyText {
                    command: COM_STMT_PREPARE,
                    field: "query",
                },
            ),
            (
                b"\x0eextra".as_slice(),
                CommandPacketError::InvalidPayloadLength {
                    command: COM_PING,
                    expected: 1,
                    actual: 6,
                },
            ),
            (
                b"\x01extra".as_slice(),
                CommandPacketError::InvalidPayloadLength {
                    command: COM_QUIT,
                    expected: 1,
                    actual: 6,
                },
            ),
            (
                b"\x03SELECT\0 1".as_slice(),
                CommandPacketError::EmbeddedNul {
                    command: COM_QUERY,
                    field: "query",
                    offset: 6,
                },
            ),
            (
                b"\x02db\0name".as_slice(),
                CommandPacketError::EmbeddedNul {
                    command: COM_INIT_DB,
                    field: "database",
                    offset: 2,
                },
            ),
            (
                b"\x16SELECT\0 ?".as_slice(),
                CommandPacketError::EmbeddedNul {
                    command: COM_STMT_PREPARE,
                    field: "query",
                    offset: 6,
                },
            ),
        ];
        for (payload, expected) in cases {
            let frame = CODEC.encode(COMMAND_SEQUENCE_ID, payload).unwrap();
            assert_eq!(
                connection.receive_command_frame(&frame),
                Err(ConnectionStateError::Command(expected))
            );
            assert_eq!(connection.state(), ConnectionState::Ready);
        }

        let invalid_utf8 = CODEC.encode(COMMAND_SEQUENCE_ID, b"\x03\xff").unwrap();
        assert_eq!(
            connection.receive_command_frame(&invalid_utf8),
            Err(ConnectionStateError::Command(
                CommandPacketError::InvalidUtf8 {
                    command: COM_QUERY,
                    field: "query"
                }
            ))
        );
        let invalid_prepare_utf8 = CODEC.encode(COMMAND_SEQUENCE_ID, b"\x16\xff").unwrap();
        assert_eq!(
            connection.receive_command_frame(&invalid_prepare_utf8),
            Err(ConnectionStateError::Command(
                CommandPacketError::InvalidUtf8 {
                    command: COM_STMT_PREPARE,
                    field: "query"
                }
            ))
        );
    }

    #[test]
    fn rejects_unsupported_prepared_statement_commands_explicitly() {
        let mut connection = ready_connection();
        let frame = CODEC
            .encode(COMMAND_SEQUENCE_ID, &[COM_STMT_SEND_LONG_DATA])
            .unwrap();
        assert_eq!(
            connection.receive_command_frame(&frame),
            Err(ConnectionStateError::Command(
                CommandPacketError::UnsupportedPreparedStatement {
                    command: COM_STMT_SEND_LONG_DATA
                }
            ))
        );
    }

    #[test]
    fn enforces_an_independent_command_payload_bound() {
        let command_codec = PacketCodec::new(MAX_COMMAND_PAYLOAD_LENGTH + 1).unwrap();
        let mut connection = ClassicConnection::with_codec(
            server_config(),
            command_codec,
            TransportSecurity::Secure,
        )
        .unwrap();
        connection.send_initial_handshake().unwrap();
        connection
            .receive_client_handshake_response(client_response(
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            ))
            .unwrap();
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        connection.send_authentication_ok().unwrap();
        let payload = vec![COM_QUERY; MAX_COMMAND_PAYLOAD_LENGTH + 1];
        let frame = command_codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
        assert_eq!(
            connection.receive_command_frame(&frame),
            Err(ConnectionStateError::Command(
                CommandPacketError::PayloadTooLarge {
                    length: MAX_COMMAND_PAYLOAD_LENGTH + 1,
                    limit: MAX_COMMAND_PAYLOAD_LENGTH,
                }
            ))
        );
        assert_eq!(connection.state(), ConnectionState::Ready);
    }

    #[test]
    fn closes_only_through_closing_state() {
        let mut connection = ClassicConnection::new(server_config()).unwrap();
        assert!(matches!(
            connection.finish_close(),
            Err(ConnectionStateError::InvalidTransition {
                state: ConnectionState::SendInitialHandshake,
                event: ConnectionEvent::Closed
            })
        ));
        connection.begin_close().unwrap();
        assert_eq!(connection.state(), ConnectionState::Closing);
        connection.finish_close().unwrap();
        assert_eq!(connection.state(), ConnectionState::Closed);
        assert!(matches!(
            connection.send_initial_handshake(),
            Err(ConnectionStateError::InvalidTransition {
                state: ConnectionState::Closed,
                event: ConnectionEvent::SendInitialHandshake
            })
        ));
    }
}
