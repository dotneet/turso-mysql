//! Offline account provisioning around the persistent account backend.
//!
//! This module has no command-line or protocol entry point. It is a narrow
//! library boundary for the future control plane: password bytes are borrowed
//! from caller-owned memory, the credential file is staged through a fresh
//! store handle, and the new handle is not installed until an external
//! checkpoint authority reports a durable checkpoint.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    validate_username, AccountDefinition, AccountGenerationBuilder, AccountId,
    AccountStoreCheckpoint, CredentialProviderConfigError, DatabaseGrant, DatabasePrivileges,
    GlobalPrivileges, PersistentAccountStore, PersistentAccountStoreError, SHA256_DIGEST_LENGTH,
};

/// Password bytes supplied by a caller-owned protected buffer.
///
/// The buffer is borrowed rather than copied and is cleared when this value is
/// dropped. Callers should construct it immediately before provisioning and
/// should not use the buffer again until this value has been dropped.
pub struct ProtectedPassword<'a> {
    bytes: &'a mut [u8],
}

impl<'a> ProtectedPassword<'a> {
    /// Borrows password bytes from caller-owned memory.
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes }
    }

    /// Returns the number of password bytes without exposing their contents.
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether no password bytes were supplied.
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for ProtectedPassword<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectedPassword")
            .field("bytes", &"<redacted>")
            .finish()
    }
}

impl Drop for ProtectedPassword<'_> {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// One account assembled from a caller-owned password without exposing its
/// derived verifier.
pub struct ProvisionedAccount {
    account_id: AccountId,
    definition: AccountDefinition,
}

impl ProvisionedAccount {
    /// Returns the opaque account identity needed to create database grants.
    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    /// Creates one database grant owned by this account.
    pub fn grant(
        &self,
        database: impl Into<String>,
        privileges: DatabasePrivileges,
    ) -> DatabaseGrant {
        DatabaseGrant::new(self.account_id.clone(), database, privileges)
    }

    /// Moves the account definition into a complete-generation builder.
    pub fn into_builder(self) -> AccountGenerationBuilder {
        AccountGenerationBuilder::new().with_account(self.definition)
    }

    /// Moves the account definition out for composition with other accounts.
    pub fn into_definition(self) -> AccountDefinition {
        self.definition
    }
}

impl fmt::Debug for ProvisionedAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvisionedAccount")
            .field("account_id", &self.account_id)
            .field("definition", &self.definition)
            .finish()
    }
}

/// Creates an account from caller-owned password bytes.
///
/// The only retained credential material is
/// `SHA256(SHA256(password))`, stored by [`AccountDefinition`] with its own
/// zeroizing drop implementation. Empty passwords are allowed by MySQL and
/// therefore are not rejected here.
pub fn provision_account(
    username: impl Into<String>,
    password: ProtectedPassword<'_>,
    enabled: bool,
    global_privileges: GlobalPrivileges,
) -> Result<ProvisionedAccount, OfflineProvisioningError> {
    let username = username.into();
    validate_username(&username).map_err(OfflineProvisioningError::InvalidUsername)?;
    let account_id =
        AccountId::generate().map_err(|_| OfflineProvisioningError::RandomUnavailable)?;
    let verifier = derive_verifier(&*password.bytes);
    let definition = AccountDefinition::new(username, account_id.clone(), enabled, verifier)
        .with_global_privileges(global_privileges);
    Ok(ProvisionedAccount {
        account_id,
        definition,
    })
}

fn derive_verifier(password: &[u8]) -> [u8; SHA256_DIGEST_LENGTH] {
    let mut first_digest = Sha256::digest(password);
    let mut first = Zeroizing::new([0; SHA256_DIGEST_LENGTH]);
    first.copy_from_slice(first_digest.as_slice());
    first_digest.as_mut_slice().zeroize();
    let mut second = Sha256::digest(first.as_ref());
    let mut verifier = [0; SHA256_DIGEST_LENGTH];
    verifier.copy_from_slice(second.as_slice());
    second.as_mut_slice().zeroize();
    verifier
}

/// Result of an external checkpoint compare-and-persist operation.
///
/// `Ambiguous` means the authority cannot tell whether the replacement was
/// durably stored. It must not be treated as success: the provisioning
/// boundary enters reconciliation-required state until the exact checkpoint
/// is read back from authoritative storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointPersistence {
    /// The replacement is durably stored and the expected checkpoint matched.
    Durable,
    /// The expected checkpoint did not match; no acknowledgement is allowed.
    Conflict,
    /// The authority knows the replacement was not durably stored.
    Failed,
    /// The authority cannot determine whether the replacement was stored.
    Ambiguous,
}

/// The external rollback-resistant checkpoint authority used by provisioning.
///
/// Implementations must compare `expected` and persist `replacement` as one
/// durable control-plane operation. Repeating an already durable replacement
/// must return [`CheckpointPersistence::Durable`], which makes reconciliation
/// after an ambiguous response safe. `expected == None` is used only for the
/// first store initialization. The trait intentionally has no error type:
/// backend errors often contain paths or implementation details, so callers
/// map them to the fixed statuses above before they reach this boundary.
pub trait AccountStoreCheckpointAuthority {
    /// Compare the old checkpoint and durably publish the replacement.
    fn compare_and_persist(
        &mut self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
    ) -> CheckpointPersistence;
}

/// The exact checkpoint transition needed to reconcile a published snapshot.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PendingAccountCheckpoint {
    expected: Option<AccountStoreCheckpoint>,
    replacement: AccountStoreCheckpoint,
    durable_revision: u64,
}

impl PendingAccountCheckpoint {
    /// Returns the checkpoint that the authority must compare, if initialized.
    pub const fn expected(&self) -> Option<&AccountStoreCheckpoint> {
        self.expected.as_ref()
    }

    /// Returns the checkpoint that must become durable before acknowledgement.
    pub const fn replacement(&self) -> &AccountStoreCheckpoint {
        &self.replacement
    }

    /// Returns the already-published account generation revision.
    pub const fn durable_revision(&self) -> u64 {
        self.durable_revision
    }
}

impl fmt::Debug for PendingAccountCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingAccountCheckpoint")
            .field("expected", &self.expected)
            .field("replacement", &self.replacement)
            .field("durable_revision", &self.durable_revision)
            .finish()
    }
}

/// A provisioning update was not acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineProvisioningError {
    /// The exact account name cannot be represented by the credential store.
    InvalidUsername(CredentialProviderConfigError),
    /// The account ID source was unavailable.
    RandomUnavailable,
    /// The persistent account store could not be opened or updated.
    Store(PersistentAccountStoreError),
    /// The durable snapshot did not exactly equal the supplied checkpoint.
    CheckpointMismatch,
    /// A prior update left durable state ahead of the checkpoint authority.
    ReconciliationRequired(Box<PendingAccountCheckpoint>),
    /// The store was published but the authority rejected the checkpoint.
    CheckpointConflict(Box<PendingAccountCheckpoint>),
    /// The store was published but the authority definitely did not persist.
    CheckpointFailed(Box<PendingAccountCheckpoint>),
    /// The store was published but checkpoint durability is unknown.
    CheckpointAmbiguous(Box<PendingAccountCheckpoint>),
}

impl fmt::Display for OfflineProvisioningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUsername(error) => error.fmt(f),
            Self::RandomUnavailable => {
                f.write_str("offline account provisioning random source unavailable")
            }
            Self::Store(error) => error.fmt(f),
            Self::CheckpointMismatch => f.write_str("account store checkpoint is not exact"),
            Self::ReconciliationRequired(pending) => write!(
                f,
                "account store requires checkpoint reconciliation at revision {}",
                pending.durable_revision
            ),
            Self::CheckpointConflict(pending) => write!(
                f,
                "account store revision {} was published but checkpoint CAS conflicted",
                pending.durable_revision
            ),
            Self::CheckpointFailed(pending) => write!(
                f,
                "account store revision {} was published but checkpoint persistence failed",
                pending.durable_revision
            ),
            Self::CheckpointAmbiguous(pending) => write!(
                f,
                "account store revision {} was published but checkpoint durability is ambiguous",
                pending.durable_revision
            ),
        }
    }
}

impl std::error::Error for OfflineProvisioningError {}

enum ProvisioningState {
    Active {
        store: PersistentAccountStore,
        checkpoint: AccountStoreCheckpoint,
    },
    ReconciliationRequired(PendingAccountCheckpoint),
}

/// Offline provisioning coordinator with exact-checkpoint staging.
///
/// Every replacement starts from a fresh store opened against the exact
/// active checkpoint. The staged store publishes its credential snapshot, but
/// remains inaccessible through this coordinator until the external authority
/// durably accepts the matching checkpoint. Failed or ambiguous checkpoint
/// writes permanently require reconciliation on this value.
pub struct OfflineAccountProvisioner {
    root: PathBuf,
    state: ProvisioningState,
}

impl OfflineAccountProvisioner {
    /// Opens a store only when its on-disk generation exactly matches the
    /// externally supplied checkpoint, including revision and digest.
    pub fn open(
        root: impl AsRef<Path>,
        checkpoint: AccountStoreCheckpoint,
    ) -> Result<Self, OfflineProvisioningError> {
        let root = root.as_ref().to_owned();
        let store = PersistentAccountStore::open(&root, &checkpoint)
            .map_err(OfflineProvisioningError::Store)?;
        let actual = store
            .checkpoint()
            .map_err(OfflineProvisioningError::Store)?;
        if actual.to_bytes() != checkpoint.to_bytes() {
            return Err(OfflineProvisioningError::CheckpointMismatch);
        }
        Ok(Self {
            root,
            state: ProvisioningState::Active { store, checkpoint },
        })
    }

    /// Initializes and publishes the first generation, then persists its
    /// external checkpoint before returning a usable coordinator.
    pub fn initialize<A: AccountStoreCheckpointAuthority>(
        root: impl AsRef<Path>,
        builder: AccountGenerationBuilder,
        authority: &mut A,
    ) -> Result<Self, OfflineProvisioningError> {
        let root = root.as_ref().to_owned();
        let store = PersistentAccountStore::initialize(&root, builder)
            .map_err(OfflineProvisioningError::Store)?;
        let checkpoint = store
            .checkpoint()
            .map_err(OfflineProvisioningError::Store)?;
        let pending = PendingAccountCheckpoint {
            expected: None,
            replacement: checkpoint,
            durable_revision: 0,
        };
        match authority.compare_and_persist(None, &checkpoint) {
            CheckpointPersistence::Durable => Ok(Self {
                root,
                state: ProvisioningState::Active { store, checkpoint },
            }),
            outcome => Err(checkpoint_failure(outcome, pending)),
        }
    }

    /// Returns the exact checkpoint currently acknowledged by this boundary.
    pub fn checkpoint(&self) -> Result<AccountStoreCheckpoint, OfflineProvisioningError> {
        match &self.state {
            ProvisioningState::Active { checkpoint, .. } => Ok(*checkpoint),
            ProvisioningState::ReconciliationRequired(pending) => Err(
                OfflineProvisioningError::ReconciliationRequired(Box::new(*pending)),
            ),
        }
    }

    /// Returns the acknowledged generation revision.
    pub fn revision(&self) -> Result<u64, OfflineProvisioningError> {
        match &self.state {
            ProvisioningState::Active { store, .. } => {
                store.revision().map_err(OfflineProvisioningError::Store)
            }
            ProvisioningState::ReconciliationRequired(pending) => Err(
                OfflineProvisioningError::ReconciliationRequired(Box::new(*pending)),
            ),
        }
    }

    /// Borrows the acknowledged store for authentication and authorization.
    ///
    /// A coordinator that needs checkpoint reconciliation never exposes its
    /// previous generation through this method.
    pub fn store(&self) -> Result<&PersistentAccountStore, OfflineProvisioningError> {
        match &self.state {
            ProvisioningState::Active { store, .. } => Ok(store),
            ProvisioningState::ReconciliationRequired(pending) => Err(
                OfflineProvisioningError::ReconciliationRequired(Box::new(*pending)),
            ),
        }
    }

    /// Returns the exact transition that must be reconciled, if any.
    pub const fn pending_checkpoint(&self) -> Option<PendingAccountCheckpoint> {
        match &self.state {
            ProvisioningState::Active { .. } => None,
            ProvisioningState::ReconciliationRequired(pending) => Some(*pending),
        }
    }

    /// Stages a complete generation with CAS and acknowledges it only after
    /// the exact replacement checkpoint is durably persisted externally.
    pub fn replace<A: AccountStoreCheckpointAuthority>(
        &mut self,
        builder: AccountGenerationBuilder,
        authority: &mut A,
    ) -> Result<u64, OfflineProvisioningError> {
        let (expected, expected_revision) = match &self.state {
            ProvisioningState::Active { store, checkpoint } => (
                *checkpoint,
                store.revision().map_err(OfflineProvisioningError::Store)?,
            ),
            ProvisioningState::ReconciliationRequired(pending) => {
                return Err(OfflineProvisioningError::ReconciliationRequired(Box::new(
                    *pending,
                )));
            }
        };

        let staged = match PersistentAccountStore::open(&self.root, &expected) {
            Ok(store) => store,
            Err(error) => return Err(OfflineProvisioningError::Store(error)),
        };
        let revision = match staged.replace(expected_revision, builder) {
            Ok(revision) => revision,
            Err(error) => return Err(OfflineProvisioningError::Store(error)),
        };
        let replacement = staged
            .checkpoint()
            .map_err(OfflineProvisioningError::Store)?;
        let pending = PendingAccountCheckpoint {
            expected: Some(expected),
            replacement,
            durable_revision: revision,
        };
        match authority.compare_and_persist(Some(&expected), &replacement) {
            CheckpointPersistence::Durable => {
                self.state = ProvisioningState::Active {
                    store: staged,
                    checkpoint: replacement,
                };
                Ok(revision)
            }
            outcome => {
                self.state = ProvisioningState::ReconciliationRequired(pending);
                Err(checkpoint_failure(outcome, pending))
            }
        }
    }

    /// Retries an unacknowledged checkpoint transition without republishing
    /// the account snapshot.
    pub fn reconcile<A: AccountStoreCheckpointAuthority>(
        &mut self,
        authority: &mut A,
    ) -> Result<(), OfflineProvisioningError> {
        let pending = match self.state {
            ProvisioningState::Active { .. } => return Ok(()),
            ProvisioningState::ReconciliationRequired(pending) => pending,
        };
        match authority.compare_and_persist(pending.expected(), pending.replacement()) {
            CheckpointPersistence::Durable => {
                let store = PersistentAccountStore::open(&self.root, pending.replacement())
                    .map_err(OfflineProvisioningError::Store)?;
                self.state = ProvisioningState::Active {
                    store,
                    checkpoint: *pending.replacement(),
                };
                Ok(())
            }
            outcome => Err(checkpoint_failure(outcome, pending)),
        }
    }

    /// Reopens this coordinator from a checkpoint already durably loaded by
    /// the caller. Reopening never installs an uncheckpointed snapshot through
    /// an in-place reload path.
    pub fn reopen(
        &mut self,
        checkpoint: AccountStoreCheckpoint,
    ) -> Result<(), OfflineProvisioningError> {
        let replacement = Self::open(&self.root, checkpoint)?;
        self.state = replacement.state;
        Ok(())
    }
}

impl fmt::Debug for OfflineAccountProvisioner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("OfflineAccountProvisioner");
        debug.field("root", &self.root);
        match &self.state {
            ProvisioningState::Active { checkpoint, .. } => debug
                .field("state", &"active")
                .field("checkpoint", checkpoint),
            ProvisioningState::ReconciliationRequired(pending) => debug
                .field("state", &"reconciliation-required")
                .field("pending", pending),
        }
        .finish()
    }
}

fn checkpoint_failure(
    outcome: CheckpointPersistence,
    pending: PendingAccountCheckpoint,
) -> OfflineProvisioningError {
    match outcome {
        CheckpointPersistence::Durable => unreachable!("durable checkpoint is not a failure"),
        CheckpointPersistence::Conflict => {
            OfflineProvisioningError::CheckpointConflict(Box::new(pending))
        }
        CheckpointPersistence::Failed => {
            OfflineProvisioningError::CheckpointFailed(Box::new(pending))
        }
        CheckpointPersistence::Ambiguous => {
            OfflineProvisioningError::CheckpointAmbiguous(Box::new(pending))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{AccountStoreCheckpointError, CredentialProvider};

    struct MemoryAuthority {
        checkpoint: Option<AccountStoreCheckpoint>,
        next: CheckpointPersistence,
    }

    impl MemoryAuthority {
        fn new(next: CheckpointPersistence) -> Self {
            Self {
                checkpoint: None,
                next,
            }
        }
    }

    impl AccountStoreCheckpointAuthority for MemoryAuthority {
        fn compare_and_persist(
            &mut self,
            expected: Option<&AccountStoreCheckpoint>,
            replacement: &AccountStoreCheckpoint,
        ) -> CheckpointPersistence {
            if self.checkpoint.as_ref() == Some(replacement) {
                return CheckpointPersistence::Durable;
            }
            if self.checkpoint.as_ref() != expected {
                return CheckpointPersistence::Conflict;
            }
            if self.next != CheckpointPersistence::Durable {
                return self.next;
            }
            self.checkpoint = Some(*replacement);
            CheckpointPersistence::Durable
        }
    }

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn builder(verifier: u8) -> AccountGenerationBuilder {
        let account_id = AccountId::from_bytes([verifier; SHA256_DIGEST_LENGTH]);
        AccountGenerationBuilder::new().with_account(
            AccountDefinition::new("alice", account_id, true, [verifier; 32])
                .with_global_privileges(GlobalPrivileges::new(true, false)),
        )
    }

    #[test]
    fn protected_password_hashes_twice_and_clears_caller_buffer() {
        let mut password = b"secret".to_vec();
        let expected = Sha256::digest(Sha256::digest(b"secret"));
        let account = provision_account(
            "alice",
            ProtectedPassword::new(&mut password),
            true,
            GlobalPrivileges::new(true, false),
        )
        .unwrap();
        let snapshot = account.into_builder();
        let store = crate::AccountStore::new(snapshot).unwrap();
        let credential = store.lookup("alice").unwrap().unwrap();
        assert_eq!(
            credential.credential().verifier_material(),
            expected.as_slice()
        );
        assert!(password.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn protected_inputs_and_provisioned_accounts_redact_debug() {
        let mut debug_password = b"secret".to_vec();
        {
            let protected = ProtectedPassword::new(&mut debug_password);
            assert!(!format!("{protected:?}").contains("secret"));
        }
        let mut password = b"secret".to_vec();
        let account = provision_account(
            "alice",
            ProtectedPassword::new(&mut password),
            true,
            GlobalPrivileges::new(true, false),
        )
        .unwrap();
        let debug = format!("{account:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn invalid_account_name_still_clears_the_password() {
        let mut password = b"secret".to_vec();
        assert!(matches!(
            provision_account(
                "",
                ProtectedPassword::new(&mut password),
                true,
                GlobalPrivileges::new(true, false),
            ),
            Err(OfflineProvisioningError::InvalidUsername(_))
        ));
        assert!(password.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn initialization_acknowledges_only_after_durable_checkpoint() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let provisioner =
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority)
                .unwrap();
        assert_eq!(provisioner.revision(), Ok(0));
        assert_eq!(authority.checkpoint, provisioner.checkpoint().ok());
    }

    #[test]
    fn failed_initial_checkpoint_returns_the_exact_recovery_transition() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Failed);
        let pending =
            match OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority)
            {
                Err(OfflineProvisioningError::CheckpointFailed(pending)) => pending,
                _ => panic!("initialization must return a recoverable checkpoint failure"),
            };
        assert_eq!(pending.expected(), None);
        assert_eq!(pending.durable_revision(), 0);

        authority.next = CheckpointPersistence::Durable;
        assert_eq!(
            authority.compare_and_persist(pending.expected(), pending.replacement()),
            CheckpointPersistence::Durable
        );
        let provisioner =
            OfflineAccountProvisioner::open(root.path(), *pending.replacement()).unwrap();
        assert_eq!(provisioner.revision(), Ok(0));
    }

    #[test]
    fn definite_checkpoint_failure_does_not_install_staged_generation() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let mut provisioner =
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority)
                .unwrap();
        authority.next = CheckpointPersistence::Failed;
        assert!(matches!(
            provisioner.replace(builder(0x22), &mut authority),
            Err(OfflineProvisioningError::CheckpointFailed(pending))
                if pending.durable_revision() == 1
        ));
        assert!(matches!(
            provisioner.revision(),
            Err(OfflineProvisioningError::ReconciliationRequired(pending))
                if pending.durable_revision() == 1
        ));
        assert!(matches!(
            provisioner.store(),
            Err(OfflineProvisioningError::ReconciliationRequired(pending))
                if pending.durable_revision() == 1
        ));
        assert!(matches!(
            provisioner.replace(builder(0x33), &mut authority),
            Err(OfflineProvisioningError::ReconciliationRequired(pending))
                if pending.durable_revision() == 1
        ));
        authority.next = CheckpointPersistence::Durable;
        provisioner.reconcile(&mut authority).unwrap();
        assert_eq!(provisioner.revision(), Ok(1));
    }

    #[test]
    fn ambiguous_checkpoint_failure_is_explicit_and_requires_reopen() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let mut provisioner =
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority)
                .unwrap();
        let old_checkpoint = authority.checkpoint.unwrap();
        authority.next = CheckpointPersistence::Ambiguous;
        assert!(matches!(
            provisioner.replace(builder(0x22), &mut authority),
            Err(OfflineProvisioningError::CheckpointAmbiguous(pending))
                if pending.durable_revision() == 1
        ));
        assert!(matches!(
            provisioner.reopen(old_checkpoint),
            Err(OfflineProvisioningError::CheckpointMismatch)
                | Err(OfflineProvisioningError::Store(
                    PersistentAccountStoreError::CheckpointMismatch
                ))
        ));
    }

    #[test]
    fn opening_requires_exact_revision_not_just_a_checkpoint_lower_bound() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let mut provisioner =
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority)
                .unwrap();
        let revision_zero = provisioner.checkpoint().unwrap();
        provisioner.replace(builder(0x22), &mut authority).unwrap();
        assert!(matches!(
            OfflineAccountProvisioner::open(root.path(), revision_zero),
            Err(OfflineProvisioningError::CheckpointMismatch)
                | Err(OfflineProvisioningError::Store(
                    PersistentAccountStoreError::CheckpointMismatch
                ))
        ));
    }

    #[test]
    fn checkpoint_encoding_remains_opaque_and_bounded() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let provisioner =
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority)
                .unwrap();
        let checkpoint = provisioner.checkpoint().unwrap();
        assert_eq!(
            AccountStoreCheckpoint::from_bytes(&checkpoint.to_bytes()),
            Ok(checkpoint)
        );
        assert_eq!(
            AccountStoreCheckpoint::from_bytes(&[0; SHA256_DIGEST_LENGTH * 2 + 7]),
            Err(AccountStoreCheckpointError::InvalidEncoding)
        );
    }
}
