//! Durable credential and authorization snapshots for classic MySQL sessions.
//!
//! The account generation in memory is changed only after the exact candidate
//! bytes have been durably published. A failed read or decode never replaces
//! the last generation that authenticated connections are using.

use std::{
    error::Error,
    fmt,
    path::Path,
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{
    account_store_format::StoredAuthSnapshot,
    account_store_fs::{
        AccountStoreFsError, AccountStoreRoot, AccountStoreSnapshotIdentity, ConditionalPublish,
        ConditionalRemove,
    },
    AccountGeneration,
};
use crate::{
    AccountGenerationBuilder, AccountId, AccountStore, AccountStoreReplaceError,
    AuthenticatedPrincipal, AuthorizationError, CredentialProvider, CredentialProviderError,
    CredentialSnapshot, DatabaseAction, DatabaseAuthorizer, SHA256_DIGEST_LENGTH,
};

/// A protected durable account backend.
///
/// Opening requires an explicitly configured directory that is owned by the
/// effective user and has exact `0700` permissions. The account file inside it
/// is not a general configuration file: missing, malformed, and unknown
/// snapshots all fail closed.
pub struct PersistentAccountStore {
    root: AccountStoreRoot,
    current: RwLock<Arc<CurrentGeneration>>,
    writer: Mutex<()>,
    initialization: Option<InitializationState>,
}

enum InitializationState {
    Prepared,
    Published(AccountStoreSnapshotIdentity),
}

impl PersistentAccountStore {
    /// Opens the exact durable account generation authorized by the checkpoint.
    pub fn open(
        root: impl AsRef<Path>,
        checkpoint: &AccountStoreCheckpoint,
    ) -> Result<Self, PersistentAccountStoreError> {
        Self::open_inner(root.as_ref(), checkpoint, None)
    }

    pub(crate) fn open_until(
        root: impl AsRef<Path>,
        checkpoint: &AccountStoreCheckpoint,
        deadline: Instant,
    ) -> Result<Self, PersistentAccountStoreError> {
        Self::open_inner(root.as_ref(), checkpoint, Some(deadline))
    }

    fn open_inner(
        root: &Path,
        checkpoint: &AccountStoreCheckpoint,
        deadline: Option<Instant>,
    ) -> Result<Self, PersistentAccountStoreError> {
        let root = AccountStoreRoot::open(root)
            .map_err(|error| map_fs_error(error, deadline.is_some()))?;
        cleanup_temporary_files(&root, deadline)?;
        let bytes = root
            .read_snapshot()
            .map_err(|error| map_fs_error(error, deadline.is_some()))?
            .ok_or(PersistentAccountStoreError::MissingSnapshot)?;
        let (generation, store_id) =
            decode_generation(&bytes).map_err(|_| PersistentAccountStoreError::InvalidSnapshot)?;
        let revision = generation.revision();
        if !checkpoint.matches(store_id, revision, &bytes) {
            return Err(PersistentAccountStoreError::CheckpointMismatch);
        }
        Ok(Self {
            root,
            current: RwLock::new(Arc::new(CurrentGeneration {
                revision,
                store_id,
                bytes,
                accounts: AccountStore::from_generation(generation),
            })),
            writer: Mutex::new(()),
            initialization: None,
        })
    }

    /// Creates the first durable generation without overwriting an existing file.
    pub(crate) fn initialize(
        root: impl AsRef<Path>,
        builder: AccountGenerationBuilder,
    ) -> Result<Self, PersistentAccountStoreError> {
        let mut store = Self::prepare_initialization(root, builder)?;
        store.publish_initialization()?;
        Ok(store)
    }

    pub(crate) fn prepare_initialization(
        root: impl AsRef<Path>,
        builder: AccountGenerationBuilder,
    ) -> Result<Self, PersistentAccountStoreError> {
        Self::prepare_initialization_inner(root.as_ref(), builder, None)
    }

    pub(crate) fn prepare_initialization_until(
        root: impl AsRef<Path>,
        builder: AccountGenerationBuilder,
        deadline: Instant,
    ) -> Result<Self, PersistentAccountStoreError> {
        Self::prepare_initialization_inner(root.as_ref(), builder, Some(deadline))
    }

    fn prepare_initialization_inner(
        root: &Path,
        builder: AccountGenerationBuilder,
        deadline: Option<Instant>,
    ) -> Result<Self, PersistentAccountStoreError> {
        let root = AccountStoreRoot::open(root)
            .map_err(|error| map_fs_error(error, deadline.is_some()))?;
        cleanup_temporary_files(&root, deadline)?;
        let store_id = *AccountId::generate()
            .map_err(|_| PersistentAccountStoreError::Unavailable)?
            .as_bytes();
        let generation = AccountGeneration::from_builder(builder, 0)
            .map_err(|_| PersistentAccountStoreError::InvalidGeneration)?;
        let bytes = generation
            .snapshot(store_id)
            .encode()
            .map_err(|_| PersistentAccountStoreError::Unavailable)?;
        Ok(Self {
            root,
            current: RwLock::new(Arc::new(CurrentGeneration {
                revision: 0,
                store_id,
                bytes,
                accounts: AccountStore::from_generation(generation),
            })),
            writer: Mutex::new(()),
            initialization: Some(InitializationState::Prepared),
        })
    }

    pub(crate) fn publish_initialization(&mut self) -> Result<(), PersistentAccountStoreError> {
        self.publish_initialization_inner(None)
    }

    pub(crate) fn publish_initialization_until(
        &mut self,
        deadline: Instant,
    ) -> Result<(), PersistentAccountStoreError> {
        self.publish_initialization_inner(Some(deadline))
    }

    fn publish_initialization_inner(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<(), PersistentAccountStoreError> {
        if !matches!(self.initialization, Some(InitializationState::Prepared)) {
            return Err(PersistentAccountStoreError::Conflict);
        }
        let current = self.current_generation()?;
        let published = match deadline {
            Some(deadline) => self.root.publish_if_absent_until(&current.bytes, deadline),
            None => self.root.publish_if_absent(&current.bytes),
        }
        .map_err(|error| map_fs_error(error, deadline.is_some()))?;
        match published {
            ConditionalPublish::Published { identity } => {
                self.initialization = Some(InitializationState::Published(identity));
                Ok(())
            }
            ConditionalPublish::Conflict => Err(PersistentAccountStoreError::AlreadyInitialized),
        }
    }

    /// Aborts an initialization snapshot after a definite external CAS
    /// conflict, but only if the retained inode and bytes are unchanged.
    pub(crate) fn abort_initialization(self) -> Result<(), PersistentAccountStoreError> {
        self.abort_initialization_inner(None)
    }

    pub(crate) fn abort_initialization_until(
        self,
        deadline: Instant,
    ) -> Result<(), PersistentAccountStoreError> {
        self.abort_initialization_inner(Some(deadline))
    }

    fn abort_initialization_inner(
        self,
        deadline: Option<Instant>,
    ) -> Result<(), PersistentAccountStoreError> {
        let Some(InitializationState::Published(identity)) = self.initialization else {
            return Err(PersistentAccountStoreError::Conflict);
        };
        let current = self.current_generation()?;
        let removed = match deadline {
            Some(deadline) => {
                self.root
                    .remove_snapshot_if_matches_until(identity, &current.bytes, deadline)
            }
            None => self
                .root
                .remove_snapshot_if_matches(identity, &current.bytes),
        }
        .map_err(|error| map_fs_error(error, deadline.is_some()))?;
        match removed {
            ConditionalRemove::Removed | ConditionalRemove::AlreadyAbsent => Ok(()),
            ConditionalRemove::Conflict => Err(PersistentAccountStoreError::Conflict),
        }
    }

    /// Returns the current durable generation revision.
    pub fn revision(&self) -> Result<u64, PersistentAccountStoreError> {
        Ok(self.current_generation()?.revision)
    }

    /// Returns the exact rollback-detection checkpoint for the current generation.
    ///
    /// The caller must durably replace its external checkpoint after every
    /// successful initialization, replacement, or reload before treating that
    /// generation as committed by the control plane.
    pub fn checkpoint(&self) -> Result<AccountStoreCheckpoint, PersistentAccountStoreError> {
        let current = self.current_generation()?;
        Ok(AccountStoreCheckpoint {
            store_id: current.store_id,
            expected_revision: current.revision,
            expected_digest: snapshot_digest(&current.bytes),
        })
    }

    /// Durably replaces the generation when both memory and disk still match.
    ///
    /// After success, persist [`Self::checkpoint`] outside this credential
    /// root before acknowledging the control-plane update.
    pub(crate) fn replace(
        &self,
        expected_revision: u64,
        builder: AccountGenerationBuilder,
    ) -> Result<u64, PersistentAccountStoreError> {
        let history = self.current_generation()?;
        if history.revision != expected_revision {
            return Err(PersistentAccountStoreError::Conflict);
        }
        let generation = history
            .accounts
            .build_replacement(expected_revision, builder)
            .map_err(map_replacement_error)?;
        let revision = generation.revision();
        let bytes = generation
            .snapshot(history.store_id)
            .encode()
            .map_err(|_| PersistentAccountStoreError::Unavailable)?;

        let _writer = self
            .writer
            .lock()
            .map_err(|_| PersistentAccountStoreError::Unavailable)?;
        let current = self.current_generation()?;
        if current.revision != expected_revision {
            return Err(PersistentAccountStoreError::Conflict);
        }
        match self
            .root
            .publish_if_unchanged(&current.bytes, &bytes)
            .map_err(map_fs_update_error)?
        {
            ConditionalPublish::Conflict => Err(PersistentAccountStoreError::Conflict),
            ConditionalPublish::Published { .. } => {
                let replacement = Arc::new(CurrentGeneration {
                    revision,
                    store_id: current.store_id,
                    bytes,
                    accounts: AccountStore::from_generation(generation),
                });
                let mut current = self
                    .current
                    .write()
                    .map_err(|_| PersistentAccountStoreError::Unavailable)?;
                *current = replacement;
                Ok(revision)
            }
        }
    }

    /// Installs an exact externally authorized on-disk generation.
    ///
    /// A mismatched checkpoint leaves the last valid in-memory generation in
    /// place. The caller must obtain the checkpoint from rollback-resistant
    /// control-plane storage; deriving it from the account root would turn an
    /// untrusted file replacement into an authorized generation.
    pub fn reload(
        &self,
        checkpoint: &AccountStoreCheckpoint,
    ) -> Result<ReloadOutcome, PersistentAccountStoreError> {
        let _writer = self
            .writer
            .lock()
            .map_err(|_| PersistentAccountStoreError::Unavailable)?;
        let bytes = self
            .root
            .read_snapshot()
            .map_err(map_fs_update_error)?
            .ok_or(PersistentAccountStoreError::MissingSnapshot)?;
        let (generation, store_id) =
            decode_generation(&bytes).map_err(|_| PersistentAccountStoreError::InvalidSnapshot)?;
        let candidate_revision = generation.revision();
        if !checkpoint.matches(store_id, candidate_revision, &bytes) {
            return Err(PersistentAccountStoreError::CheckpointMismatch);
        }
        let current = self.current_generation()?;
        if bytes.as_slice() == current.bytes.as_slice() {
            return Ok(ReloadOutcome::Unchanged);
        }
        if store_id != current.store_id
            || candidate_revision < current.revision
            || (candidate_revision == current.revision
                && bytes.as_slice() != current.bytes.as_slice())
        {
            return Err(PersistentAccountStoreError::Conflict);
        }

        let replacement = Arc::new(CurrentGeneration {
            revision: candidate_revision,
            store_id,
            bytes,
            accounts: AccountStore::from_generation(generation),
        });
        let mut current = self
            .current
            .write()
            .map_err(|_| PersistentAccountStoreError::Unavailable)?;
        *current = replacement;
        Ok(ReloadOutcome::Reloaded {
            revision: candidate_revision,
        })
    }

    fn current_generation(&self) -> Result<Arc<CurrentGeneration>, PersistentAccountStoreError> {
        self.current
            .read()
            .map(|current| Arc::clone(&current))
            .map_err(|_| PersistentAccountStoreError::Unavailable)
    }
}

impl fmt::Debug for PersistentAccountStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistentAccountStore")
            .field("revision", &self.revision().ok())
            .finish()
    }
}

impl CredentialProvider for PersistentAccountStore {
    fn lookup(
        &self,
        username: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
        self.current_generation()
            .map_err(|_| CredentialProviderError::BackendUnavailable)?
            .accounts
            .lookup(username)
    }
}

impl DatabaseAuthorizer for PersistentAccountStore {
    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        self.current_generation()
            .map_err(|_| AuthorizationError::Unavailable)?
            .accounts
            .authorize(principal, action)
    }
}

impl CredentialProvider for Arc<PersistentAccountStore> {
    fn lookup(
        &self,
        username: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
        self.as_ref().lookup(username)
    }
}

impl DatabaseAuthorizer for Arc<PersistentAccountStore> {
    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        self.as_ref().authorize(principal, action)
    }
}

/// The outcome of a reload attempt that accepted the on-disk snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The durable bytes already match the in-memory generation.
    Unchanged,
    /// A newer durable generation became current.
    Reloaded {
        /// The revision installed from disk.
        revision: u64,
    },
}

/// A public error that does not expose paths, usernames, or verifier bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistentAccountStoreError {
    /// The configured directory or backend could not be used safely.
    Unavailable,
    /// No durable account snapshot exists yet.
    MissingSnapshot,
    /// The durable bytes are malformed, corrupt, or use an unsupported format.
    InvalidSnapshot,
    /// Initialization found an already published final snapshot.
    AlreadyInitialized,
    /// The expected revision or expected durable bytes no longer match.
    Conflict,
    /// The durable snapshot does not exactly match the supplied checkpoint.
    CheckpointMismatch,
    /// The proposed account and privilege generation is invalid.
    InvalidGeneration,
    /// A bounded provisioning operation could not acquire the writer lock.
    ProvisioningBusy,
}

impl fmt::Display for PersistentAccountStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("persistent account store unavailable"),
            Self::MissingSnapshot => f.write_str("persistent account snapshot is missing"),
            Self::InvalidSnapshot => f.write_str("persistent account snapshot is invalid"),
            Self::AlreadyInitialized => f.write_str("persistent account snapshot already exists"),
            Self::Conflict => f.write_str("persistent account generation conflict"),
            Self::CheckpointMismatch => f.write_str("persistent account checkpoint mismatch"),
            Self::InvalidGeneration => f.write_str("persistent account generation is invalid"),
            Self::ProvisioningBusy => f.write_str("persistent account provisioning is busy"),
        }
    }
}

impl Error for PersistentAccountStoreError {}

/// An opaque exact expectation for opening one durable account generation.
///
/// Store these bytes outside the credential root in rollback-resistant control
/// plane storage. Keeping the checkpoint beside the snapshot cannot detect an
/// attacker who rolls both files back together.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AccountStoreCheckpoint {
    store_id: [u8; SHA256_DIGEST_LENGTH],
    expected_revision: u64,
    expected_digest: [u8; SHA256_DIGEST_LENGTH],
}

impl AccountStoreCheckpoint {
    /// Returns the durable account-generation revision represented here.
    pub const fn revision(self) -> u64 {
        self.expected_revision
    }

    /// Returns whether two checkpoints belong to one initialized account store.
    pub fn belongs_to_same_store(self, other: Self) -> bool {
        self.store_id == other.store_id
    }

    /// Returns a fixed-size representation for external durable storage.
    pub const fn to_bytes(self) -> [u8; SHA256_DIGEST_LENGTH * 2 + 8] {
        let mut bytes = [0; SHA256_DIGEST_LENGTH * 2 + 8];
        let mut index = 0;
        while index < SHA256_DIGEST_LENGTH {
            bytes[index] = self.store_id[index];
            index += 1;
        }
        let revision = self.expected_revision.to_be_bytes();
        let mut revision_index = 0;
        while revision_index < revision.len() {
            bytes[SHA256_DIGEST_LENGTH + revision_index] = revision[revision_index];
            revision_index += 1;
        }
        let mut digest_index = 0;
        while digest_index < SHA256_DIGEST_LENGTH {
            bytes[SHA256_DIGEST_LENGTH + 8 + digest_index] = self.expected_digest[digest_index];
            digest_index += 1;
        }
        bytes
    }

    /// Parses a checkpoint previously returned by [`Self::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AccountStoreCheckpointError> {
        if bytes.len() != SHA256_DIGEST_LENGTH * 2 + 8 {
            return Err(AccountStoreCheckpointError::InvalidEncoding);
        }
        let mut store_id = [0; SHA256_DIGEST_LENGTH];
        store_id.copy_from_slice(&bytes[..SHA256_DIGEST_LENGTH]);
        if store_id.iter().all(|byte| *byte == 0) {
            return Err(AccountStoreCheckpointError::InvalidEncoding);
        }
        let mut revision = [0; 8];
        revision.copy_from_slice(&bytes[SHA256_DIGEST_LENGTH..SHA256_DIGEST_LENGTH + 8]);
        let mut expected_digest = [0; SHA256_DIGEST_LENGTH];
        expected_digest.copy_from_slice(&bytes[SHA256_DIGEST_LENGTH + 8..]);
        Ok(Self {
            store_id,
            expected_revision: u64::from_be_bytes(revision),
            expected_digest,
        })
    }

    fn matches(&self, store_id: [u8; SHA256_DIGEST_LENGTH], revision: u64, bytes: &[u8]) -> bool {
        self.store_id == store_id
            && self.expected_revision == revision
            && self.expected_digest == snapshot_digest(bytes)
    }
}

impl fmt::Debug for AccountStoreCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountStoreCheckpoint")
            .field("store_id", &"<redacted>")
            .field("expected_revision", &self.expected_revision)
            .field("expected_digest", &"<redacted>")
            .finish()
    }
}

/// A checkpoint was malformed before it reached durable control-plane storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStoreCheckpointError {
    /// The encoded length or store ID was invalid.
    InvalidEncoding,
}

impl fmt::Display for AccountStoreCheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid account store checkpoint")
    }
}

impl Error for AccountStoreCheckpointError {}

struct CurrentGeneration {
    revision: u64,
    store_id: [u8; SHA256_DIGEST_LENGTH],
    bytes: Zeroizing<Vec<u8>>,
    accounts: AccountStore,
}

fn snapshot_digest(bytes: &[u8]) -> [u8; SHA256_DIGEST_LENGTH] {
    let digest = Sha256::digest(bytes);
    let mut output = [0; SHA256_DIGEST_LENGTH];
    output.copy_from_slice(&digest);
    output
}

fn decode_generation(bytes: &[u8]) -> Result<(AccountGeneration, [u8; SHA256_DIGEST_LENGTH]), ()> {
    let snapshot = StoredAuthSnapshot::decode(bytes).map_err(|_| ())?;
    let store_id = snapshot.store_id;
    let generation = AccountGeneration::from_snapshot(snapshot).map_err(|_| ())?;
    Ok((generation, store_id))
}

fn map_fs_update_error(_: AccountStoreFsError) -> PersistentAccountStoreError {
    PersistentAccountStoreError::Unavailable
}

fn cleanup_temporary_files(
    root: &AccountStoreRoot,
    deadline: Option<Instant>,
) -> Result<(), PersistentAccountStoreError> {
    let result = match deadline {
        Some(deadline) => root.cleanup_temporary_files_until(deadline),
        None => root.cleanup_temporary_files(),
    };
    result.map_err(|error| map_fs_error(error, deadline.is_some()))
}

fn map_fs_error(error: AccountStoreFsError, bounded: bool) -> PersistentAccountStoreError {
    if bounded && error == AccountStoreFsError::ProvisioningLockTimedOut {
        PersistentAccountStoreError::ProvisioningBusy
    } else {
        PersistentAccountStoreError::Unavailable
    }
}

fn map_replacement_error(error: AccountStoreReplaceError) -> PersistentAccountStoreError {
    match error {
        AccountStoreReplaceError::Conflict { .. } => PersistentAccountStoreError::Conflict,
        AccountStoreReplaceError::InvalidGeneration(_) => {
            PersistentAccountStoreError::InvalidGeneration
        }
        AccountStoreReplaceError::Unavailable => PersistentAccountStoreError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;
    use crate::{
        AccountDefinition, AccountId, DatabaseGrant, DatabasePrivileges, GlobalPrivileges,
    };

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn builder(verifier: u8, query: bool) -> AccountGenerationBuilder {
        let account_id = AccountId::from_bytes([7; 32]);
        let account = AccountDefinition::new("alice", account_id.clone(), true, [verifier; 32])
            .with_global_privileges(GlobalPrivileges::new(true, false));
        let builder = AccountGenerationBuilder::new().with_account(account);
        if query {
            builder.with_grant(DatabaseGrant::new(
                account_id,
                "reports",
                DatabasePrivileges::new(false, true, false, false),
            ))
        } else {
            builder
        }
    }

    fn principal() -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([7; 32]))
    }

    #[test]
    fn initialize_open_and_restart_keep_one_full_verifier_without_fast_cache() {
        let root = root();
        let store = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        let checkpoint = store.checkpoint().unwrap();
        assert_eq!(store.revision(), Ok(0));
        let snapshot = store.lookup("alice").unwrap().unwrap();
        assert_eq!(snapshot.credential().verifier_material(), &[0x11; 32]);
        assert_eq!(snapshot.credential().fast_cache_verifier(), None);
        drop(store);

        let restarted = PersistentAccountStore::open(root.path(), &checkpoint).unwrap();
        assert_eq!(restarted.revision(), Ok(0));
        assert_eq!(
            restarted.authorize(
                &principal(),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Ok(())
        );
    }

    #[test]
    fn initialize_never_overwrites_an_existing_snapshot() {
        let root = root();
        let store = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        assert!(matches!(
            PersistentAccountStore::initialize(root.path(), builder(0x22, false)),
            Err(PersistentAccountStoreError::AlreadyInitialized)
        ));
        assert_eq!(
            store
                .lookup("alice")
                .unwrap()
                .unwrap()
                .credential()
                .verifier_material(),
            &[0x11; 32]
        );
    }

    #[test]
    fn replacement_is_durable_and_other_instances_cannot_overwrite_it() {
        let root = root();
        let first = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        let checkpoint = first.checkpoint().unwrap();
        let second = PersistentAccountStore::open(root.path(), &checkpoint).unwrap();
        assert_eq!(first.replace(0, builder(0x22, false)), Ok(1));
        assert_eq!(
            second.replace(0, builder(0x33, true)),
            Err(PersistentAccountStoreError::Conflict)
        );
        let checkpoint = first.checkpoint().unwrap();
        drop(first);
        let restarted = PersistentAccountStore::open(root.path(), &checkpoint).unwrap();
        assert_eq!(restarted.revision(), Ok(1));
        assert_eq!(
            restarted.authorize(
                &principal(),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Err(AuthorizationError::Denied)
        );
    }

    #[test]
    fn durable_retirement_rejects_reusing_a_deleted_account_id_after_restart() {
        let root = root();
        let store = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        assert_eq!(store.replace(0, AccountGenerationBuilder::new()), Ok(1));
        let checkpoint = store.checkpoint().unwrap();
        assert_eq!(
            store.replace(1, builder(0x22, true)),
            Err(PersistentAccountStoreError::InvalidGeneration)
        );
        drop(store);
        let restarted = PersistentAccountStore::open(root.path(), &checkpoint).unwrap();
        assert_eq!(
            restarted.replace(1, builder(0x22, true)),
            Err(PersistentAccountStoreError::InvalidGeneration)
        );
    }

    #[test]
    fn durable_round_trip_preserves_every_database_permission_bit() {
        let root = root();
        let account_id = AccountId::from_bytes([7; 32]);
        let builder = AccountGenerationBuilder::new()
            .with_account(
                AccountDefinition::new("alice", account_id.clone(), true, [0x11; 32])
                    .with_global_privileges(GlobalPrivileges::new(true, true)),
            )
            .with_grant(DatabaseGrant::new(
                account_id,
                "reports",
                DatabasePrivileges::new(true, true, true, true),
            ));
        let store = PersistentAccountStore::initialize(root.path(), builder).unwrap();
        let checkpoint = store.checkpoint().unwrap();
        drop(store);
        let restarted = PersistentAccountStore::open(root.path(), &checkpoint).unwrap();
        for action in [
            DatabaseAction::Connect {
                database: Some("reports"),
            },
            DatabaseAction::Query {
                database: "reports",
            },
            DatabaseAction::Create {
                database: "reports",
            },
            DatabaseAction::Drop {
                database: "reports",
            },
            DatabaseAction::List,
        ] {
            assert_eq!(restarted.authorize(&principal(), action), Ok(()));
        }
    }

    #[test]
    fn reload_applies_revocations_but_keeps_the_last_good_generation_on_failures() {
        let root = root();
        let store = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        let checkpoint = store.checkpoint().unwrap();
        let writer = PersistentAccountStore::open(root.path(), &checkpoint).unwrap();
        writer.replace(0, builder(0x22, false)).unwrap();
        let replacement_checkpoint = writer.checkpoint().unwrap();
        assert_eq!(
            store.reload(&checkpoint),
            Err(PersistentAccountStoreError::CheckpointMismatch)
        );
        assert_eq!(
            store.authorize(
                &principal(),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Ok(())
        );
        assert_eq!(
            store.reload(&replacement_checkpoint),
            Ok(ReloadOutcome::Reloaded { revision: 1 })
        );
        assert_eq!(
            store.authorize(
                &principal(),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Err(AuthorizationError::Denied)
        );

        store.root.publish_snapshot(b"corrupt").unwrap();
        assert_eq!(
            store.reload(&replacement_checkpoint),
            Err(PersistentAccountStoreError::InvalidSnapshot)
        );
        assert_eq!(
            store.authorize(
                &principal(),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Err(AuthorizationError::Denied)
        );
    }

    #[test]
    fn reload_rejects_older_or_same_revision_bytes_without_replacing_memory() {
        let root = root();
        let store = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        store.replace(0, builder(0x22, false)).unwrap();
        let checkpoint = store.checkpoint().unwrap();

        let older = AccountGeneration::from_builder(builder(0x33, true), 0)
            .unwrap()
            .snapshot(checkpoint.store_id)
            .encode()
            .unwrap();
        store.root.publish_snapshot(&older).unwrap();
        assert_eq!(
            store.reload(&checkpoint),
            Err(PersistentAccountStoreError::CheckpointMismatch)
        );

        let same_revision = AccountGeneration::from_builder(builder(0x44, true), 1)
            .unwrap()
            .snapshot(checkpoint.store_id)
            .encode()
            .unwrap();
        store.root.publish_snapshot(&same_revision).unwrap();
        assert_eq!(
            store.reload(&checkpoint),
            Err(PersistentAccountStoreError::CheckpointMismatch)
        );
        assert_eq!(
            store.authorize(
                &principal(),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Err(AuthorizationError::Denied)
        );
    }

    #[test]
    fn opening_a_missing_or_corrupt_snapshot_never_creates_a_default_allow_policy() {
        let root = root();
        assert!(matches!(
            PersistentAccountStore::open(root.path(), &test_checkpoint()),
            Err(PersistentAccountStoreError::MissingSnapshot)
        ));
        let account_root = AccountStoreRoot::open(root.path()).unwrap();
        account_root.publish_snapshot(b"corrupt").unwrap();
        assert!(matches!(
            PersistentAccountStore::open(root.path(), &test_checkpoint()),
            Err(PersistentAccountStoreError::InvalidSnapshot)
        ));
    }

    #[test]
    fn checkpoint_round_trips_without_exposing_its_store_id() {
        let root = root();
        let store = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        let checkpoint = store.checkpoint().unwrap();
        assert_eq!(
            AccountStoreCheckpoint::from_bytes(&checkpoint.to_bytes()),
            Ok(checkpoint)
        );
        assert!(format!("{checkpoint:?}").contains("<redacted>"));
        assert_eq!(
            AccountStoreCheckpoint::from_bytes(&[0; SHA256_DIGEST_LENGTH * 2 + 8]),
            Err(AccountStoreCheckpointError::InvalidEncoding)
        );
        let different_same_revision = AccountGeneration::from_builder(builder(0x22, false), 0)
            .unwrap()
            .snapshot(checkpoint.store_id)
            .encode()
            .unwrap();
        store
            .root
            .publish_snapshot(&different_same_revision)
            .unwrap();
        assert!(matches!(
            PersistentAccountStore::open(root.path(), &checkpoint),
            Err(PersistentAccountStoreError::CheckpointMismatch)
        ));
    }

    #[test]
    fn a_checkpoint_rejects_rolled_back_or_other_store_snapshots() {
        let root = root();
        let store = PersistentAccountStore::initialize(root.path(), builder(0x11, true)).unwrap();
        let revision_zero_checkpoint = store.checkpoint().unwrap();
        let revision_zero = store.root.read_snapshot().unwrap().unwrap();
        store.replace(0, builder(0x22, false)).unwrap();
        let checkpoint = store.checkpoint().unwrap();
        assert!(matches!(
            PersistentAccountStore::open(root.path(), &revision_zero_checkpoint),
            Err(PersistentAccountStoreError::CheckpointMismatch)
        ));
        store.root.publish_snapshot(&revision_zero).unwrap();
        assert!(matches!(
            PersistentAccountStore::open(root.path(), &checkpoint),
            Err(PersistentAccountStoreError::CheckpointMismatch)
        ));

        let other_root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(other_root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let other =
            PersistentAccountStore::initialize(other_root.path(), builder(0x33, true)).unwrap();
        let other_bytes = other.root.read_snapshot().unwrap().unwrap();
        store.root.publish_snapshot(&other_bytes).unwrap();
        assert!(matches!(
            PersistentAccountStore::open(root.path(), &checkpoint),
            Err(PersistentAccountStoreError::CheckpointMismatch)
        ));
    }

    fn test_checkpoint() -> AccountStoreCheckpoint {
        AccountStoreCheckpoint {
            store_id: [1; SHA256_DIGEST_LENGTH],
            expected_revision: 0,
            expected_digest: [0; SHA256_DIGEST_LENGTH],
        }
    }
}
