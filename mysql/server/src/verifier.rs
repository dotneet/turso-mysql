//! External credential-provider boundary for `caching_sha2_password`.
//!
//! This module only accepts precomputed `SHA256(SHA256(password))` material.
//! It never accepts a plaintext password as a provider value. Production
//! storage belongs in an implementation of [`CredentialProvider`]; the
//! in-memory provider below is intended for tests and development only.

use std::{collections::BTreeMap, error::Error, fmt};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    AuthenticationVerificationStage, CredentialVerificationRequest, FullAuthenticationResult,
    InitialAuthenticationResult, TransportSecurity, CACHING_SHA2_PASSWORD_PLUGIN,
    MAX_CLIENT_USERNAME_LENGTH, MAX_FULL_AUTH_RESPONSE_LENGTH,
};

/// SHA-256 digest size in bytes.
pub const SHA256_DIGEST_LENGTH: usize = 32;
/// Fast authentication responses contain one SHA-256 digest.
pub const FAST_AUTH_RESPONSE_LENGTH: usize = SHA256_DIGEST_LENGTH;

/// Precomputed `SHA256(SHA256(password))` material for one account.
///
/// The material is cleared when the value is dropped and is intentionally
/// omitted from [`Debug`] output. Use [`Self::from_sha256_sha256`] with
/// material obtained from a credential store; no plaintext constructor is
/// provided.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct StoredCredential {
    enabled: bool,
    full_verifier_material: [u8; SHA256_DIGEST_LENGTH],
    fast_cache_verifier: Option<[u8; SHA256_DIGEST_LENGTH]>,
}

impl StoredCredential {
    /// Creates a record from precomputed SHA-256 verifier material.
    pub const fn from_sha256_sha256(
        enabled: bool,
        verifier_material: [u8; SHA256_DIGEST_LENGTH],
    ) -> Self {
        Self {
            enabled,
            full_verifier_material: verifier_material,
            fast_cache_verifier: Some(verifier_material),
        }
    }

    /// Creates a record with a persistent full verifier and no fast cache.
    pub const fn from_full_verifier(
        enabled: bool,
        verifier_material: [u8; SHA256_DIGEST_LENGTH],
    ) -> Self {
        Self {
            enabled,
            full_verifier_material: verifier_material,
            fast_cache_verifier: None,
        }
    }

    /// Returns whether this account is enabled for authentication.
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the precomputed verifier material for a verifier operation.
    pub fn verifier_material(&self) -> &[u8; SHA256_DIGEST_LENGTH] {
        &self.full_verifier_material
    }

    /// Returns the optional in-memory fast-auth cache material.
    pub fn fast_cache_verifier(&self) -> Option<&[u8; SHA256_DIGEST_LENGTH]> {
        self.fast_cache_verifier.as_ref()
    }

    fn duplicate(&self) -> Self {
        Self {
            enabled: self.enabled,
            full_verifier_material: self.full_verifier_material,
            fast_cache_verifier: self.fast_cache_verifier,
        }
    }
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredential")
            .field("enabled", &self.enabled)
            .field("verifier_material", &"<redacted>")
            .finish()
    }
}

/// Errors raised by a credential backend.
///
/// These errors stay outside protocol responses. They intentionally carry no
/// username, password, verifier bytes, or backend message that could contain
/// credential material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProviderError {
    /// The backend could not be reached or is temporarily unavailable.
    BackendUnavailable,
    /// The backend failed without exposing a backend-specific message.
    BackendFailure,
}

impl fmt::Display for CredentialProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable => f.write_str("credential backend unavailable"),
            Self::BackendFailure => f.write_str("credential backend failure"),
        }
    }
}

impl Error for CredentialProviderError {}

/// Looks up enabled state and precomputed verifier material for a username.
pub trait CredentialProvider {
    /// Returns a record, or `None` for an unknown account.
    fn lookup(&self, username: &str) -> Result<Option<StoredCredential>, CredentialProviderError>;
}

/// The default provider denies every account.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DefaultCredentialProvider;

impl CredentialProvider for DefaultCredentialProvider {
    fn lookup(&self, _username: &str) -> Result<Option<StoredCredential>, CredentialProviderError> {
        Ok(None)
    }
}

/// Alias for callers that want to state the deny-all behavior explicitly.
pub type DenyAllCredentialProvider = DefaultCredentialProvider;

/// Configuration errors for the test/development in-memory provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProviderConfigError {
    /// The account name must not be empty.
    EmptyUsername,
    /// The account name exceeds the classic handshake bound.
    UsernameTooLong { length: usize, limit: usize },
    /// The account name contains a NUL byte.
    EmbeddedNul { offset: usize },
}

impl fmt::Display for CredentialProviderConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUsername => f.write_str("credential username must not be empty"),
            Self::UsernameTooLong { length, limit } => {
                write!(
                    f,
                    "credential username length {length} exceeds limit {limit}"
                )
            }
            Self::EmbeddedNul { offset } => {
                write!(
                    f,
                    "credential username contains an embedded NUL at byte {offset}"
                )
            }
        }
    }
}

impl Error for CredentialProviderConfigError {}

/// An in-memory provider intended only for tests and development.
///
/// Production deployments must implement [`CredentialProvider`] over their
/// own protected storage instead of retaining credentials in this map.
#[derive(Default)]
pub struct InMemoryCredentialProvider {
    entries: BTreeMap<String, StoredCredential>,
}

impl fmt::Debug for InMemoryCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryCredentialProvider")
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

impl InMemoryCredentialProvider {
    /// Creates an empty in-memory provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts precomputed verifier material for one account.
    pub fn insert(
        &mut self,
        username: impl Into<String>,
        credential: StoredCredential,
    ) -> Result<(), CredentialProviderConfigError> {
        let username = username.into();
        validate_username(&username)?;
        self.entries.insert(username, credential);
        Ok(())
    }

    /// Removes one account without returning its credential material.
    pub fn remove(&mut self, username: &str) -> bool {
        self.entries.remove(username).is_some()
    }

    /// Returns the number of configured accounts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether no accounts are configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl CredentialProvider for InMemoryCredentialProvider {
    fn lookup(&self, username: &str) -> Result<Option<StoredCredential>, CredentialProviderError> {
        Ok(self.entries.get(username).map(StoredCredential::duplicate))
    }
}

/// A result tagged with the state-machine result type for the request stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationVerificationResult {
    /// Result for the initial handshake response.
    Initial(InitialAuthenticationResult),
    /// Result for the secure full-authentication response.
    Full(FullAuthenticationResult),
}

/// Errors from credential lookup or an incorrectly staged verification call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialVerificationError {
    /// The provider failed; this is not a protocol error payload.
    Provider(CredentialProviderError),
    /// The stage-specific verifier method received the other request type.
    UnexpectedStage {
        expected: AuthenticationVerificationStage,
        actual: AuthenticationVerificationStage,
    },
    /// A request selected an authentication plugin outside this verifier.
    UnsupportedPlugin,
    /// The request was not created by a state-machine packet decoder.
    UnvalidatedRequest,
}

impl fmt::Display for CredentialVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(error) => write!(f, "credential provider error: {error}"),
            Self::UnexpectedStage { expected, actual } => {
                write!(f, "verification stage {actual:?} is not {expected:?}")
            }
            Self::UnsupportedPlugin => f.write_str("unsupported authentication plugin"),
            Self::UnvalidatedRequest => f.write_str("authentication request was not validated"),
        }
    }
}

impl Error for CredentialVerificationError {}

impl From<CredentialProviderError> for CredentialVerificationError {
    fn from(error: CredentialProviderError) -> Self {
        Self::Provider(error)
    }
}

/// Verifies `caching_sha2_password` requests with an external credential provider.
pub struct CachingSha2Verifier<P = DefaultCredentialProvider> {
    provider: P,
}

impl<P> CachingSha2Verifier<P> {
    /// Creates a verifier over an external provider.
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Returns the configured provider.
    pub const fn provider(&self) -> &P {
        &self.provider
    }
}

impl<P: Default> Default for CachingSha2Verifier<P> {
    fn default() -> Self {
        Self::new(P::default())
    }
}

impl<P> fmt::Debug for CachingSha2Verifier<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachingSha2Verifier")
            .field("provider", &"<redacted>")
            .finish()
    }
}

impl<P: CredentialProvider> CachingSha2Verifier<P> {
    /// Verifies an initial handshake response for the existing state machine.
    pub fn verify_initial(
        &self,
        request: &CredentialVerificationRequest<'_>,
    ) -> Result<InitialAuthenticationResult, CredentialVerificationError> {
        self.require_plugin(request)?;
        if request.stage != AuthenticationVerificationStage::InitialHandshakeResponse {
            return Err(CredentialVerificationError::UnexpectedStage {
                expected: AuthenticationVerificationStage::InitialHandshakeResponse,
                actual: request.stage,
            });
        }
        let credential = self.provider.lookup(&request.username)?;
        let enabled = credential.as_ref().is_some_and(StoredCredential::enabled);
        let has_fast_cache = credential
            .as_ref()
            .is_some_and(|credential| credential.fast_cache_verifier().is_some());
        let material = credential
            .as_ref()
            .and_then(StoredCredential::fast_cache_verifier)
            .map_or(&[0; SHA256_DIGEST_LENGTH], |material| material);
        let matches = request.auth_response.len() == FAST_AUTH_RESPONSE_LENGTH
            && fast_response_matches(
                request.auth_response,
                &request.server_auth_plugin_data,
                material,
            );
        if enabled && has_fast_cache && matches {
            Ok(InitialAuthenticationResult::FastAuthSuccess)
        } else if request.transport_security == TransportSecurity::Secure {
            // Cache misses, disabled accounts, and failed fast responses all
            // use the same next protocol step. The full verifier turns the
            // latter two into a rejection without exposing account state.
            Ok(InitialAuthenticationResult::FullAuthenticationRequired)
        } else {
            Ok(InitialAuthenticationResult::Rejected)
        }
    }

    /// Verifies a full response that was decoded by the secure connection path.
    pub fn verify_full(
        &self,
        request: &CredentialVerificationRequest<'_>,
    ) -> Result<FullAuthenticationResult, CredentialVerificationError> {
        self.require_plugin(request)?;
        if request.stage != AuthenticationVerificationStage::FullAuthenticationResponse {
            return Err(CredentialVerificationError::UnexpectedStage {
                expected: AuthenticationVerificationStage::FullAuthenticationResponse,
                actual: request.stage,
            });
        }
        if request.transport_security != TransportSecurity::Secure {
            return Ok(FullAuthenticationResult::Rejected);
        }
        // The packet decoder has already consumed exactly one trailing NUL;
        // retain the same bound here for callers that pass a request directly.
        if request.auth_response.len() >= MAX_FULL_AUTH_RESPONSE_LENGTH {
            return Ok(FullAuthenticationResult::Rejected);
        }

        let credential = self.provider.lookup(&request.username)?;
        let enabled = credential.as_ref().is_some_and(StoredCredential::enabled);
        let material = credential.as_ref().map_or(
            &[0; SHA256_DIGEST_LENGTH],
            StoredCredential::verifier_material,
        );
        let matches = full_response_matches(request.auth_response, material);
        if enabled && matches {
            Ok(FullAuthenticationResult::Authenticated)
        } else {
            Ok(FullAuthenticationResult::Rejected)
        }
    }

    /// Verifies either request stage while preserving the existing apply path.
    pub fn verify(
        &self,
        request: &CredentialVerificationRequest<'_>,
    ) -> Result<AuthenticationVerificationResult, CredentialVerificationError> {
        match request.stage {
            AuthenticationVerificationStage::InitialHandshakeResponse => self
                .verify_initial(request)
                .map(AuthenticationVerificationResult::Initial),
            AuthenticationVerificationStage::FullAuthenticationResponse => self
                .verify_full(request)
                .map(AuthenticationVerificationResult::Full),
        }
    }

    fn require_plugin(
        &self,
        request: &CredentialVerificationRequest<'_>,
    ) -> Result<(), CredentialVerificationError> {
        if request.plugin_name != CACHING_SHA2_PASSWORD_PLUGIN {
            return Err(CredentialVerificationError::UnsupportedPlugin);
        }
        if !request.frame_validated {
            return Err(CredentialVerificationError::UnvalidatedRequest);
        }
        Ok(())
    }
}

fn validate_username(username: &str) -> Result<(), CredentialProviderConfigError> {
    if username.is_empty() {
        return Err(CredentialProviderConfigError::EmptyUsername);
    }
    if username.len() > MAX_CLIENT_USERNAME_LENGTH {
        return Err(CredentialProviderConfigError::UsernameTooLong {
            length: username.len(),
            limit: MAX_CLIENT_USERNAME_LENGTH,
        });
    }
    if let Some(offset) = username.as_bytes().iter().position(|byte| *byte == 0) {
        return Err(CredentialProviderConfigError::EmbeddedNul { offset });
    }
    Ok(())
}

fn sha256_digest(input: &[u8]) -> [u8; SHA256_DIGEST_LENGTH] {
    let digest = Sha256::digest(input);
    let mut output = [0; SHA256_DIGEST_LENGTH];
    output.copy_from_slice(&digest);
    output
}

fn fast_response_matches(
    auth_response: &[u8],
    scramble: &[u8; crate::AUTH_PLUGIN_DATA_LENGTH],
    verifier_material: &[u8; SHA256_DIGEST_LENGTH],
) -> bool {
    let mut stage_three = sha256_digest(verifier_material);
    let mut challenge = [0; SHA256_DIGEST_LENGTH + crate::AUTH_PLUGIN_DATA_LENGTH];
    challenge[..SHA256_DIGEST_LENGTH].copy_from_slice(&stage_three);
    challenge[SHA256_DIGEST_LENGTH..].copy_from_slice(scramble);
    let mut mask = sha256_digest(&challenge);
    let mut candidate_stage_one = [0; SHA256_DIGEST_LENGTH];
    for (candidate, (&response, &mask_byte)) in candidate_stage_one
        .iter_mut()
        .zip(auth_response.iter().zip(mask.iter()))
    {
        *candidate = response ^ mask_byte;
    }
    let mut candidate_stage_two = sha256_digest(&candidate_stage_one);
    let matches = candidate_stage_two
        .as_slice()
        .ct_eq(verifier_material.as_slice())
        .unwrap_u8()
        == 1;
    stage_three.zeroize();
    challenge.zeroize();
    mask.zeroize();
    candidate_stage_one.zeroize();
    candidate_stage_two.zeroize();
    matches
}

fn full_response_matches(
    auth_response: &[u8],
    verifier_material: &[u8; SHA256_DIGEST_LENGTH],
) -> bool {
    let mut stage_one = sha256_digest(auth_response);
    let mut stage_two = sha256_digest(&stage_one);
    let matches = stage_two
        .as_slice()
        .ct_eq(verifier_material.as_slice())
        .unwrap_u8()
        == 1;
    stage_one.zeroize();
    stage_two.zeroize();
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AuthPacketError, ClientAuthResponse, PacketCodec, AUTH_PLUGIN_DATA_LENGTH,
        MAX_FULL_AUTH_RESPONSE_LENGTH,
    };

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: 4096,
    };

    fn verifier_material(password: &[u8]) -> [u8; SHA256_DIGEST_LENGTH] {
        let stage_one = sha256_digest(password);
        sha256_digest(&stage_one)
    }

    fn fast_response(
        password: &[u8],
        scramble: &[u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> [u8; FAST_AUTH_RESPONSE_LENGTH] {
        let stage_one = sha256_digest(password);
        let stage_two = sha256_digest(&stage_one);
        let stage_three = sha256_digest(&stage_two);
        let mut challenge = [0; SHA256_DIGEST_LENGTH + AUTH_PLUGIN_DATA_LENGTH];
        challenge[..SHA256_DIGEST_LENGTH].copy_from_slice(&stage_three);
        challenge[SHA256_DIGEST_LENGTH..].copy_from_slice(scramble);
        let mask = sha256_digest(&challenge);
        let mut response = [0; FAST_AUTH_RESPONSE_LENGTH];
        for (output, (&password_hash, &mask_byte)) in
            response.iter_mut().zip(stage_one.iter().zip(mask.iter()))
        {
            *output = password_hash ^ mask_byte;
        }
        response
    }

    fn initial_request<'a>(
        username: &str,
        response: &'a [u8],
        scramble: [u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> CredentialVerificationRequest<'a> {
        CredentialVerificationRequest {
            username: username.to_owned(),
            plugin_name: CACHING_SHA2_PASSWORD_PLUGIN,
            server_auth_plugin_data: scramble,
            auth_response: response,
            stage: AuthenticationVerificationStage::InitialHandshakeResponse,
            transport_security: TransportSecurity::Secure,
            frame_validated: true,
        }
    }

    fn full_request<'a>(username: &str, response: &'a [u8]) -> CredentialVerificationRequest<'a> {
        CredentialVerificationRequest {
            username: username.to_owned(),
            plugin_name: CACHING_SHA2_PASSWORD_PLUGIN,
            server_auth_plugin_data: [0; AUTH_PLUGIN_DATA_LENGTH],
            auth_response: response,
            stage: AuthenticationVerificationStage::FullAuthenticationResponse,
            transport_security: TransportSecurity::Secure,
            frame_validated: true,
        }
    }

    #[test]
    fn fast_vector_matches_mysql_formula_and_wrong_response_requires_full_auth() {
        let password = b"correct horse battery staple";
        let scramble = [0x42; AUTH_PLUGIN_DATA_LENGTH];
        let response = fast_response(password, &scramble);
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);
        let request = initial_request("root", &response, scramble);
        assert_eq!(
            verifier.verify_initial(&request).unwrap(),
            InitialAuthenticationResult::FastAuthSuccess
        );

        let wrong = [0; FAST_AUTH_RESPONSE_LENGTH];
        let wrong_request = initial_request("root", &wrong, scramble);
        assert_eq!(
            verifier.verify_initial(&wrong_request).unwrap(),
            InitialAuthenticationResult::FullAuthenticationRequired
        );
    }

    #[test]
    fn full_auth_uses_the_same_stored_verifier_material() {
        let password = b"secret";
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);
        assert_eq!(
            verifier
                .verify_full(&full_request("root", password))
                .unwrap(),
            FullAuthenticationResult::Authenticated
        );
        assert_eq!(
            verifier
                .verify_full(&full_request("root", b"wrong"))
                .unwrap(),
            FullAuthenticationResult::Rejected
        );
    }

    #[test]
    fn unknown_disabled_and_wrong_credentials_share_full_auth_boundary() {
        let password = b"secret";
        let scramble = [7; AUTH_PLUGIN_DATA_LENGTH];
        let response = fast_response(password, &scramble);
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "disabled",
                StoredCredential::from_sha256_sha256(false, verifier_material(password)),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);
        for request in [
            initial_request("unknown", &response, scramble),
            initial_request("disabled", &response, scramble),
            initial_request("disabled", &[0; FAST_AUTH_RESPONSE_LENGTH], scramble),
        ] {
            assert_eq!(
                verifier.verify_initial(&request).unwrap(),
                InitialAuthenticationResult::FullAuthenticationRequired
            );
        }
        for request in [
            full_request("unknown", password),
            full_request("disabled", password),
            full_request("disabled", b"wrong"),
        ] {
            assert_eq!(
                verifier.verify_full(&request).unwrap(),
                FullAuthenticationResult::Rejected
            );
        }
    }

    #[test]
    fn default_provider_denies_and_backend_errors_remain_typed() {
        let verifier = CachingSha2Verifier::new(DefaultCredentialProvider);
        let request = full_request("root", b"secret");
        assert_eq!(
            verifier.verify_full(&request).unwrap(),
            FullAuthenticationResult::Rejected
        );

        #[derive(Debug)]
        struct FailingProvider;
        impl CredentialProvider for FailingProvider {
            fn lookup(
                &self,
                _username: &str,
            ) -> Result<Option<StoredCredential>, CredentialProviderError> {
                Err(CredentialProviderError::BackendUnavailable)
            }
        }
        let verifier = CachingSha2Verifier::new(FailingProvider);
        assert_eq!(
            verifier.verify_full(&request),
            Err(CredentialVerificationError::Provider(
                CredentialProviderError::BackendUnavailable
            ))
        );
    }

    #[test]
    fn verification_rejects_oversized_auth_and_wrong_stage() {
        let verifier = CachingSha2Verifier::new(DefaultCredentialProvider);
        let oversized = vec![b'x'; MAX_FULL_AUTH_RESPONSE_LENGTH];
        assert_eq!(
            verifier
                .verify_full(&full_request("root", &oversized))
                .unwrap(),
            FullAuthenticationResult::Rejected
        );
        let mut insecure = full_request("root", b"secret");
        insecure.transport_security = TransportSecurity::Plaintext;
        assert_eq!(
            verifier.verify_full(&insecure).unwrap(),
            FullAuthenticationResult::Rejected
        );
        let initial = initial_request("root", &[0; FAST_AUTH_RESPONSE_LENGTH], [0; 20]);
        assert_eq!(
            verifier.verify_full(&initial),
            Err(CredentialVerificationError::UnexpectedStage {
                expected: AuthenticationVerificationStage::FullAuthenticationResponse,
                actual: AuthenticationVerificationStage::InitialHandshakeResponse,
            })
        );
    }

    #[test]
    fn credential_material_and_requests_are_redacted() {
        let material = verifier_material(b"distinctive-auth-secret");
        let credential = StoredCredential::from_sha256_sha256(true, material);
        let debug = format!("{credential:?}");
        assert!(!debug.contains("distinctive"));
        assert!(!debug.contains(&format!("{material:?}")));

        let request = full_request("root", b"distinctive-auth-secret");
        let debug = format!("{request:?}");
        assert!(!debug.contains("distinctive-auth-secret"));
    }

    #[test]
    fn full_auth_frame_requires_exact_trailing_nul_before_verification() {
        let missing = CODEC.encode(2, b"secret").unwrap();
        assert_eq!(
            ClientAuthResponse::decode(CODEC, &missing),
            Err(AuthPacketError::MissingTerminator {
                field: "full authentication response"
            })
        );
        let embedded = CODEC.encode(2, b"sec\0ret\0").unwrap();
        assert_eq!(
            ClientAuthResponse::decode(CODEC, &embedded),
            Err(AuthPacketError::EmbeddedNul {
                field: "full authentication response",
                offset: 3,
            })
        );
    }

    #[test]
    fn verifier_rejects_a_request_without_state_machine_validation() {
        let mut request = full_request("root", b"secret");
        request.frame_validated = false;
        let verifier = CachingSha2Verifier::new(DefaultCredentialProvider);
        assert_eq!(
            verifier.verify_full(&request),
            Err(CredentialVerificationError::UnvalidatedRequest)
        );
    }

    #[test]
    fn provider_rejects_unbounded_or_malformed_account_names() {
        let mut provider = InMemoryCredentialProvider::new();
        let credential = StoredCredential::from_sha256_sha256(true, [0; SHA256_DIGEST_LENGTH]);
        assert_eq!(
            provider.insert("", credential),
            Err(CredentialProviderConfigError::EmptyUsername)
        );
        let credential = StoredCredential::from_sha256_sha256(true, [0; SHA256_DIGEST_LENGTH]);
        assert_eq!(
            provider.insert("a\0b", credential),
            Err(CredentialProviderConfigError::EmbeddedNul { offset: 1 })
        );
    }
}
