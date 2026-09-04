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

/// An opaque, provider-assigned account identity.
///
/// The handshake username is only a lookup key.  Providers must return the
/// canonical identity that authorization will use after authentication.  The
/// bytes are intentionally not exposed or formatted.
#[derive(Clone, PartialEq, Eq, Hash, Zeroize, ZeroizeOnDrop)]
pub struct AccountId([u8; SHA256_DIGEST_LENGTH]);

impl AccountId {
    /// Creates an account identity from provider-owned canonical bytes.
    pub const fn from_bytes(bytes: [u8; SHA256_DIGEST_LENGTH]) -> Self {
        Self(bytes)
    }

    /// Generates a non-zero persistent account identity.
    pub fn generate() -> Result<Self, AccountIdGenerationError> {
        for _ in 0..16 {
            let mut bytes = [0; SHA256_DIGEST_LENGTH];
            getrandom::fill(&mut bytes).map_err(|_| AccountIdGenerationError::Unavailable)?;
            let account_id = Self(bytes);
            if !account_id.is_zero() {
                return Ok(account_id);
            }
        }
        Err(AccountIdGenerationError::Unavailable)
    }

    /// Returns the provider-owned bytes for protected in-crate storage.
    pub(crate) const fn as_bytes(&self) -> &[u8; SHA256_DIGEST_LENGTH] {
        &self.0
    }

    /// Returns whether this value cannot identify a persisted account.
    pub(crate) fn is_zero(&self) -> bool {
        self.as_bytes().iter().all(|byte| *byte == 0)
    }
}

/// A secure account ID could not be generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountIdGenerationError {
    /// The operating system did not provide usable random bytes.
    Unavailable,
}

impl fmt::Display for AccountIdGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("account ID generation unavailable")
    }
}

impl Error for AccountIdGenerationError {}

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountId(<redacted>)")
    }
}

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

    pub(crate) fn duplicate(&self) -> Self {
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

/// The credential and canonical identity returned by one provider lookup.
///
/// This value is owned by the authentication flow and is cleared when it is
/// dropped.  It is deliberately not `Clone`: a connection must retain one
/// snapshot from its initial response through full authentication.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct CredentialSnapshot {
    account_id: AccountId,
    credential: StoredCredential,
}

impl CredentialSnapshot {
    /// Creates one provider result from a canonical account identity.
    pub const fn new(account_id: AccountId, credential: StoredCredential) -> Self {
        Self {
            account_id,
            credential,
        }
    }

    pub(crate) fn credential(&self) -> &StoredCredential {
        &self.credential
    }
}

impl fmt::Debug for CredentialSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialSnapshot")
            .field("account_id", &"<redacted>")
            .field("credential", &"<redacted>")
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
    /// Returns one owned snapshot, or `None` for an unknown account.
    fn lookup(&self, username: &str)
        -> Result<Option<CredentialSnapshot>, CredentialProviderError>;
}

/// The default provider denies every account.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DefaultCredentialProvider;

impl CredentialProvider for DefaultCredentialProvider {
    fn lookup(
        &self,
        _username: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
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
    entries: BTreeMap<String, (AccountId, StoredCredential)>,
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
        let account_id = AccountId::from_bytes(sha256_digest(username.as_bytes()));
        self.entries.insert(username, (account_id, credential));
        Ok(())
    }

    /// Inserts precomputed verifier material with an explicit canonical ID.
    pub fn insert_with_account_id(
        &mut self,
        username: impl Into<String>,
        account_id: AccountId,
        credential: StoredCredential,
    ) -> Result<(), CredentialProviderConfigError> {
        let username = username.into();
        validate_username(&username)?;
        self.entries.insert(username, (account_id, credential));
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
    fn lookup(
        &self,
        username: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
        Ok(self.entries.get(username).map(|(account_id, credential)| {
            CredentialSnapshot::new(account_id.clone(), credential.duplicate())
        }))
    }
}

/// A result tagged with the state-machine result type for the request stage.
#[cfg(test)]
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
    /// A full-authentication request does not belong to the pending handshake.
    PendingRequestMismatch,
    /// The connection did not retain an initial authentication snapshot.
    PendingAuthenticationMissing,
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
            Self::PendingRequestMismatch => {
                f.write_str("full authentication request does not match the pending handshake")
            }
            Self::PendingAuthenticationMissing => {
                f.write_str("pending authentication snapshot is missing")
            }
        }
    }
}

impl Error for CredentialVerificationError {}

impl From<CredentialProviderError> for CredentialVerificationError {
    fn from(error: CredentialProviderError) -> Self {
        Self::Provider(error)
    }
}

/// The identity minted after a verifier has accepted authentication.
///
/// No constructor is public.  Callers can compare the provider's canonical
/// account identity, but cannot recover a username or turn it into a string.
#[derive(PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct AuthenticatedPrincipal {
    account_id: AccountId,
}

impl AuthenticatedPrincipal {
    /// Returns the opaque canonical account identity for authorization.
    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    #[cfg(test)]
    pub(crate) fn from_account_id_for_testing(account_id: AccountId) -> Self {
        Self { account_id }
    }

    fn from_snapshot(snapshot: &CredentialSnapshot) -> Self {
        Self {
            account_id: snapshot.account_id.clone(),
        }
    }
}

impl fmt::Debug for AuthenticatedPrincipal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthenticatedPrincipal(<redacted>)")
    }
}

/// The connection identity that ties the initial and full requests together.
///
/// A full-auth response is accepted only for the same handshake username and
/// server challenge that produced the pending snapshot.
#[derive(Debug, PartialEq, Eq)]
struct AuthenticationRequestBinding {
    username: String,
    server_auth_plugin_data: [u8; crate::AUTH_PLUGIN_DATA_LENGTH],
    transport_security: TransportSecurity,
}

impl AuthenticationRequestBinding {
    fn from_request(request: &CredentialVerificationRequest<'_>) -> Self {
        Self {
            username: request.username.clone(),
            server_auth_plugin_data: request.server_auth_plugin_data,
            transport_security: request.transport_security,
        }
    }

    fn matches(&self, request: &CredentialVerificationRequest<'_>) -> bool {
        self.username == request.username
            && self.server_auth_plugin_data == request.server_auth_plugin_data
            && self.transport_security == request.transport_security
    }
}

/// The one-use credential snapshot retained between fast and full auth.
///
/// This type is crate-private so callers cannot bypass the verifier by
/// manufacturing a pending authentication value.
pub(crate) struct PendingAuthentication {
    binding: AuthenticationRequestBinding,
    snapshot: Option<CredentialSnapshot>,
}

impl PendingAuthentication {
    fn new(
        request: &CredentialVerificationRequest<'_>,
        snapshot: Option<CredentialSnapshot>,
    ) -> Self {
        Self {
            binding: AuthenticationRequestBinding::from_request(request),
            snapshot,
        }
    }

    fn matches(&self, request: &CredentialVerificationRequest<'_>) -> bool {
        self.binding.matches(request)
    }
}

impl fmt::Debug for PendingAuthentication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingAuthentication")
            .field("binding", &"<redacted>")
            .field("snapshot", &"<redacted>")
            .finish()
    }
}

/// Initial verification result used by the complete-frame owner.
pub(crate) struct InitialAuthenticationVerification {
    pub(crate) result: InitialAuthenticationResult,
    pub(crate) pending: Option<PendingAuthentication>,
    pub(crate) principal: Option<AuthenticatedPrincipal>,
}

/// Full verification result used by the complete-frame owner.
pub(crate) struct FullAuthenticationVerification {
    pub(crate) result: FullAuthenticationResult,
    pub(crate) principal: Option<AuthenticatedPrincipal>,
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
    /// Begins one connection authentication attempt and takes one credential
    /// snapshot for both possible verification stages.
    pub(crate) fn verify_initial_for_connection(
        &self,
        request: &CredentialVerificationRequest<'_>,
    ) -> Result<InitialAuthenticationVerification, CredentialVerificationError> {
        self.require_plugin(request)?;
        if request.stage != AuthenticationVerificationStage::InitialHandshakeResponse {
            return Err(CredentialVerificationError::UnexpectedStage {
                expected: AuthenticationVerificationStage::InitialHandshakeResponse,
                actual: request.stage,
            });
        }

        let snapshot = self.provider.lookup(&request.username)?;
        let enabled = snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.credential().enabled());
        let has_fast_cache = snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.credential().fast_cache_verifier().is_some());
        let material = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.credential().fast_cache_verifier())
            .map_or(&[0; SHA256_DIGEST_LENGTH], |material| material);
        let matches = request.auth_response.len() == FAST_AUTH_RESPONSE_LENGTH
            && fast_response_matches(
                request.auth_response,
                &request.server_auth_plugin_data,
                material,
            );

        if enabled && has_fast_cache && matches {
            let principal = AuthenticatedPrincipal::from_snapshot(
                snapshot
                    .as_ref()
                    .expect("enabled cached credentials require a snapshot"),
            );
            Ok(InitialAuthenticationVerification {
                result: InitialAuthenticationResult::FastAuthSuccess,
                pending: None,
                principal: Some(principal),
            })
        } else if request.transport_security == TransportSecurity::Secure {
            // Unknown, disabled, cache-miss, and wrong credentials all take
            // the same full-authentication branch.  The snapshot is retained
            // so this branch never asks the provider a second time.
            Ok(InitialAuthenticationVerification {
                result: InitialAuthenticationResult::FullAuthenticationRequired,
                pending: Some(PendingAuthentication::new(request, snapshot)),
                principal: None,
            })
        } else {
            Ok(InitialAuthenticationVerification {
                result: InitialAuthenticationResult::Rejected,
                pending: None,
                principal: None,
            })
        }
    }

    /// Completes one connection authentication attempt from its owned
    /// pending snapshot.  The pending value is consumed even on mismatch.
    pub(crate) fn verify_full_for_connection(
        &self,
        pending: PendingAuthentication,
        request: &CredentialVerificationRequest<'_>,
    ) -> Result<FullAuthenticationVerification, CredentialVerificationError> {
        self.require_plugin(request)?;
        if request.stage != AuthenticationVerificationStage::FullAuthenticationResponse {
            return Err(CredentialVerificationError::UnexpectedStage {
                expected: AuthenticationVerificationStage::FullAuthenticationResponse,
                actual: request.stage,
            });
        }
        if !pending.matches(request) {
            return Err(CredentialVerificationError::PendingRequestMismatch);
        }
        if request.transport_security != TransportSecurity::Secure {
            return Ok(FullAuthenticationVerification {
                result: FullAuthenticationResult::Rejected,
                principal: None,
            });
        }
        // The packet decoder has already consumed exactly one trailing NUL;
        // retain the same bound here for callers that pass a request directly.
        if request.auth_response.len() >= MAX_FULL_AUTH_RESPONSE_LENGTH {
            return Ok(FullAuthenticationVerification {
                result: FullAuthenticationResult::Rejected,
                principal: None,
            });
        }

        let snapshot = pending.snapshot;
        let enabled = snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.credential().enabled());
        let material = snapshot
            .as_ref()
            .map_or(&[0; SHA256_DIGEST_LENGTH], |snapshot| {
                snapshot.credential().verifier_material()
            });
        let matches = full_response_matches(request.auth_response, material);
        if enabled && matches {
            let principal = AuthenticatedPrincipal::from_snapshot(
                snapshot
                    .as_ref()
                    .expect("enabled credentials require a snapshot"),
            );
            Ok(FullAuthenticationVerification {
                result: FullAuthenticationResult::Authenticated,
                principal: Some(principal),
            })
        } else {
            Ok(FullAuthenticationVerification {
                result: FullAuthenticationResult::Rejected,
                principal: None,
            })
        }
    }

    /// Verifies an initial handshake response for the existing state machine.
    #[cfg(test)]
    pub(crate) fn verify_initial(
        &self,
        request: &CredentialVerificationRequest<'_>,
    ) -> Result<InitialAuthenticationResult, CredentialVerificationError> {
        Ok(self.verify_initial_for_connection(request)?.result)
    }

    /// Verifies a full response that was decoded by the secure connection path.
    #[cfg(test)]
    pub(crate) fn verify_full(
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
        if request.auth_response.len() >= MAX_FULL_AUTH_RESPONSE_LENGTH {
            return Ok(FullAuthenticationResult::Rejected);
        }
        let snapshot = self.provider.lookup(&request.username)?;
        let enabled = snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.credential().enabled());
        let material = snapshot
            .as_ref()
            .map_or(&[0; SHA256_DIGEST_LENGTH], |snapshot| {
                snapshot.credential().verifier_material()
            });
        let matches = full_response_matches(request.auth_response, material);
        if enabled && matches {
            Ok(FullAuthenticationResult::Authenticated)
        } else {
            Ok(FullAuthenticationResult::Rejected)
        }
    }

    /// Verifies either request stage while preserving the existing apply path.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn verify(
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

/// Validates an account username before collecting or deriving credentials.
pub fn validate_username(username: &str) -> Result<(), CredentialProviderConfigError> {
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
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: 4096,
    };

    #[test]
    fn generated_account_ids_are_never_zero() {
        assert!(!AccountId::generate().unwrap().is_zero());
    }

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
        full_request_with_scramble(username, response, [0; AUTH_PLUGIN_DATA_LENGTH])
    }

    fn full_request_with_scramble<'a>(
        username: &str,
        response: &'a [u8],
        scramble: [u8; AUTH_PLUGIN_DATA_LENGTH],
    ) -> CredentialVerificationRequest<'a> {
        CredentialVerificationRequest {
            username: username.to_owned(),
            plugin_name: CACHING_SHA2_PASSWORD_PLUGIN,
            server_auth_plugin_data: scramble,
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
    fn one_provider_snapshot_is_used_for_full_auth_after_the_provider_changes() {
        struct ChangingProvider {
            lookups: Arc<AtomicUsize>,
            changed: Arc<AtomicBool>,
        }

        impl CredentialProvider for ChangingProvider {
            fn lookup(
                &self,
                _username: &str,
            ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                if self.changed.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                Ok(Some(CredentialSnapshot::new(
                    AccountId::from_bytes([0x71; SHA256_DIGEST_LENGTH]),
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
        let verifier = CachingSha2Verifier::new(provider);
        let scramble = [0x34; AUTH_PLUGIN_DATA_LENGTH];
        let initial = initial_request("handshake-name", &[0; FAST_AUTH_RESPONSE_LENGTH], scramble);
        let start = verifier.verify_initial_for_connection(&initial).unwrap();
        assert_eq!(
            start.result,
            InitialAuthenticationResult::FullAuthenticationRequired
        );
        let pending = start.pending.unwrap();
        changed.store(true, Ordering::SeqCst);

        let full = full_request_with_scramble("handshake-name", b"secret", scramble);
        let finish = verifier.verify_full_for_connection(pending, &full).unwrap();
        assert_eq!(finish.result, FullAuthenticationResult::Authenticated);
        assert!(finish.principal.is_some());
        assert_eq!(lookups.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_canonical_account_id_survives_fast_and_full_auth_paths() {
        let expected = AccountId::from_bytes([0xa5; SHA256_DIGEST_LENGTH]);
        let password = b"secret";
        let scramble = [0x25; AUTH_PLUGIN_DATA_LENGTH];
        let response = fast_response(password, &scramble);

        let mut fast_provider = InMemoryCredentialProvider::new();
        fast_provider
            .insert_with_account_id(
                "login-alias",
                expected.clone(),
                StoredCredential::from_sha256_sha256(true, verifier_material(password)),
            )
            .unwrap();
        let fast_verifier = CachingSha2Verifier::new(fast_provider);
        let fast = fast_verifier
            .verify_initial_for_connection(&initial_request("login-alias", &response, scramble))
            .unwrap();
        assert_eq!(fast.result, InitialAuthenticationResult::FastAuthSuccess);
        assert_eq!(fast.principal.unwrap().account_id(), &expected);

        let mut full_provider = InMemoryCredentialProvider::new();
        full_provider
            .insert_with_account_id(
                "login-alias",
                expected.clone(),
                StoredCredential::from_full_verifier(true, verifier_material(password)),
            )
            .unwrap();
        let full_verifier = CachingSha2Verifier::new(full_provider);
        let full_start = full_verifier
            .verify_initial_for_connection(&initial_request(
                "login-alias",
                &[0; FAST_AUTH_RESPONSE_LENGTH],
                scramble,
            ))
            .unwrap();
        let full = full_verifier
            .verify_full_for_connection(
                full_start.pending.unwrap(),
                &full_request_with_scramble("login-alias", password, scramble),
            )
            .unwrap();
        assert_eq!(full.result, FullAuthenticationResult::Authenticated);
        assert_eq!(full.principal.unwrap().account_id(), &expected);
    }

    #[test]
    fn pending_authentication_rejects_a_different_user_or_server_nonce() {
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert(
                "root",
                StoredCredential::from_full_verifier(true, verifier_material(b"secret")),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);
        let first_scramble = [0x11; AUTH_PLUGIN_DATA_LENGTH];
        let first = verifier
            .verify_initial_for_connection(&initial_request(
                "root",
                &[0; FAST_AUTH_RESPONSE_LENGTH],
                first_scramble,
            ))
            .unwrap();
        assert!(matches!(
            verifier.verify_full_for_connection(
                first.pending.unwrap(),
                &full_request_with_scramble("other", b"secret", first_scramble),
            ),
            Err(CredentialVerificationError::PendingRequestMismatch)
        ));

        let second = verifier
            .verify_initial_for_connection(&initial_request(
                "root",
                &[0; FAST_AUTH_RESPONSE_LENGTH],
                first_scramble,
            ))
            .unwrap();
        assert!(matches!(
            verifier.verify_full_for_connection(
                second.pending.unwrap(),
                &full_request_with_scramble("root", b"secret", [0x22; AUTH_PLUGIN_DATA_LENGTH]),
            ),
            Err(CredentialVerificationError::PendingRequestMismatch)
        ));
    }

    #[test]
    fn authenticated_principal_debug_is_redacted() {
        let mut provider = InMemoryCredentialProvider::new();
        provider
            .insert_with_account_id(
                "root",
                AccountId::from_bytes([0xd3; SHA256_DIGEST_LENGTH]),
                StoredCredential::from_sha256_sha256(true, verifier_material(b"secret")),
            )
            .unwrap();
        let verifier = CachingSha2Verifier::new(provider);
        let principal = verifier
            .verify_initial_for_connection(&initial_request(
                "root",
                &fast_response(b"secret", &[0x2a; AUTH_PLUGIN_DATA_LENGTH]),
                [0x2a; AUTH_PLUGIN_DATA_LENGTH],
            ))
            .unwrap()
            .principal
            .expect("successful authentication must mint a principal");
        let debug = format!("{principal:?}");
        assert_eq!(debug, "AuthenticatedPrincipal(<redacted>)");
        assert!(!debug.contains("d3"));
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
            ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
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
