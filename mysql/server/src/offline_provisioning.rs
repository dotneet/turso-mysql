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
    time::Instant,
};

use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    account_store_fs::{AccountStoreFsError, AccountStoreRoot, MAX_PENDING_BYTES},
    validate_username, AccountDefinition, AccountGenerationBuilder, AccountId,
    AccountStoreCheckpoint, AccountStoreCheckpointReader, CheckpointAuthorityId,
    CheckpointReadError, CredentialProviderConfigError, DatabaseGrant, DatabasePrivileges,
    GlobalPrivileges, PersistentAccountStore, PersistentAccountStoreError, SHA256_DIGEST_LENGTH,
};

const PENDING_MAGIC: &[u8; 4] = b"TMCP";
const PENDING_VERSION: u8 = 1;
const PENDING_OPERATION_INITIALIZE: u8 = 0;
const PENDING_PREFIX_BYTES: usize = 8;
const PENDING_CHECKSUM_BYTES: usize = SHA256_DIGEST_LENGTH;
const PENDING_CHECKPOINT_BYTES: usize = SHA256_DIGEST_LENGTH * 2 + 8;

/// One exact initialization transition retained until the authority acknowledges it.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingAccountStoreUpdate {
    authority: CheckpointAuthorityId,
    expected: Option<AccountStoreCheckpoint>,
    replacement: AccountStoreCheckpoint,
}

impl PendingAccountStoreUpdate {
    /// Returns the authority name that owns this pending update.
    pub fn authority(&self) -> &CheckpointAuthorityId {
        &self.authority
    }

    /// Returns the checkpoint that must match before publication.
    pub const fn expected(&self) -> Option<&AccountStoreCheckpoint> {
        self.expected.as_ref()
    }

    /// Returns the checkpoint that must become durable.
    pub const fn replacement(&self) -> &AccountStoreCheckpoint {
        &self.replacement
    }

    fn new_initialization(
        authority: CheckpointAuthorityId,
        replacement: AccountStoreCheckpoint,
    ) -> Self {
        Self {
            authority,
            expected: None,
            replacement,
        }
    }

    fn encode(&self) -> Result<Vec<u8>, OfflineProvisioningError> {
        let authority = self.authority.as_str().as_bytes();
        let authority_len: u16 = authority
            .len()
            .try_into()
            .map_err(|_| OfflineProvisioningError::PendingJournalInvalid)?;
        let mut bytes = Vec::with_capacity(
            PENDING_PREFIX_BYTES
                + authority.len()
                + 1
                + PENDING_CHECKPOINT_BYTES
                + PENDING_CHECKSUM_BYTES,
        );
        bytes.extend_from_slice(PENDING_MAGIC);
        bytes.push(PENDING_VERSION);
        bytes.push(PENDING_OPERATION_INITIALIZE);
        bytes.extend_from_slice(&authority_len.to_be_bytes());
        bytes.extend_from_slice(authority);
        bytes.push(0);
        bytes.extend_from_slice(&self.replacement.to_bytes());
        bytes.extend_from_slice(&Sha256::digest(&bytes));
        if bytes.len() > MAX_PENDING_BYTES {
            return Err(OfflineProvisioningError::PendingJournalInvalid);
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, OfflineProvisioningError> {
        if bytes.len()
            < PENDING_PREFIX_BYTES + 1 + PENDING_CHECKPOINT_BYTES + PENDING_CHECKSUM_BYTES
            || bytes.len() > MAX_PENDING_BYTES
            || bytes[..4] != *PENDING_MAGIC
            || bytes[4] != PENDING_VERSION
            || bytes[5] != PENDING_OPERATION_INITIALIZE
        {
            return Err(OfflineProvisioningError::PendingJournalInvalid);
        }
        let authority_len = usize::from(u16::from_be_bytes([bytes[6], bytes[7]]));
        let authority_end = PENDING_PREFIX_BYTES
            .checked_add(authority_len)
            .ok_or(OfflineProvisioningError::PendingJournalInvalid)?;
        let expected_tag = *bytes
            .get(authority_end)
            .ok_or(OfflineProvisioningError::PendingJournalInvalid)?;
        if expected_tag != 0 {
            return Err(OfflineProvisioningError::PendingJournalInvalid);
        }
        let replacement_start = authority_end
            .checked_add(1)
            .ok_or(OfflineProvisioningError::PendingJournalInvalid)?;
        let replacement_end = replacement_start
            .checked_add(PENDING_CHECKPOINT_BYTES)
            .ok_or(OfflineProvisioningError::PendingJournalInvalid)?;
        let checksum_end = replacement_end
            .checked_add(PENDING_CHECKSUM_BYTES)
            .ok_or(OfflineProvisioningError::PendingJournalInvalid)?;
        if checksum_end != bytes.len()
            || Sha256::digest(&bytes[..replacement_end]).as_slice() != &bytes[replacement_end..]
        {
            return Err(OfflineProvisioningError::PendingJournalInvalid);
        }
        let authority = std::str::from_utf8(&bytes[PENDING_PREFIX_BYTES..authority_end])
            .map_err(|_| OfflineProvisioningError::PendingJournalInvalid)
            .and_then(|value| {
                CheckpointAuthorityId::new(value.to_owned())
                    .map_err(|_| OfflineProvisioningError::PendingJournalInvalid)
            })?;
        let replacement =
            AccountStoreCheckpoint::from_bytes(&bytes[replacement_start..replacement_end])
                .map_err(|_| OfflineProvisioningError::PendingJournalInvalid)?;
        Ok(Self {
            authority,
            expected: None,
            replacement,
        })
    }
}

impl fmt::Debug for PendingAccountStoreUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingAccountStoreUpdate")
            .field("authority", &self.authority)
            .field("expected", &self.expected)
            .field("replacement", &self.replacement)
            .finish()
    }
}

/// The result of recovering an initialization journal after a process crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitializationReconcileOutcome {
    /// No initialization journal was present.
    NoPendingUpdate,
    /// The journal was durable but the first snapshot was never published.
    AbortedBeforeSnapshot,
    /// The exact journal transition is now durable at the authority.
    Reconciled { revision: u64 },
}

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
            .field("definition", &"<redacted>")
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
    /// Reading the authority checkpoint during crash recovery did not complete safely.
    CheckpointRead(CheckpointReadError),
    /// A prior update left durable state ahead of the checkpoint authority.
    ReconciliationRequired(Box<PendingAccountCheckpoint>),
    /// The store was published but the authority rejected the checkpoint.
    CheckpointConflict(Box<PendingAccountCheckpoint>),
    /// The store was published but the authority definitely did not persist.
    CheckpointFailed(Box<PendingAccountCheckpoint>),
    /// The store was published but checkpoint durability is unknown.
    CheckpointAmbiguous(Box<PendingAccountCheckpoint>),
    /// The retained initialization journal was malformed or did not match its snapshot.
    PendingJournalInvalid,
    /// The retained initialization journal belongs to another authority.
    PendingAuthorityMismatch,
    /// Another provisioning transaction retained the bounded provisioning lock.
    ProvisioningBusy,
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
            Self::CheckpointRead(error) => error.fmt(f),
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
            Self::PendingJournalInvalid => f.write_str("offline provisioning journal is invalid"),
            Self::PendingAuthorityMismatch => {
                f.write_str("offline provisioning journal belongs to another authority")
            }
            Self::ProvisioningBusy => f.write_str("offline provisioning is busy"),
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
            CheckpointPersistence::Conflict => {
                store
                    .abort_initialization()
                    .map_err(OfflineProvisioningError::Store)?;
                Err(OfflineProvisioningError::CheckpointConflict(Box::new(
                    pending,
                )))
            }
            outcome => Err(checkpoint_failure(outcome, pending)),
        }
    }

    /// Initializes through a durable pending journal so a later process can
    /// replay the exact initial CAS after a crash or ambiguous response.
    pub fn initialize_crash_safe<A: AccountStoreCheckpointAuthority>(
        root: impl AsRef<Path>,
        authority_id: CheckpointAuthorityId,
        builder: AccountGenerationBuilder,
        authority: &mut A,
        deadline: Instant,
    ) -> Result<Self, OfflineProvisioningError> {
        let root = root.as_ref().to_owned();
        let journal_root = AccountStoreRoot::open(&root).map_err(map_journal_error)?;
        let _lock = journal_root
            .acquire_provisioning_lock_until(deadline)
            .map_err(map_journal_error)?;
        if journal_root
            .read_provisioning_journal()
            .map_err(map_journal_error)?
            .is_some()
        {
            return Err(OfflineProvisioningError::PendingJournalInvalid);
        }

        let mut staged = PersistentAccountStore::prepare_initialization(&root, builder)
            .map_err(OfflineProvisioningError::Store)?;
        let replacement = staged
            .checkpoint()
            .map_err(OfflineProvisioningError::Store)?;
        let pending = PendingAccountStoreUpdate::new_initialization(authority_id, replacement);
        let journal = pending.encode()?;
        journal_root
            .publish_provisioning_journal(&journal)
            .map_err(map_journal_error)?;

        if let Err(error) = staged.publish_initialization() {
            return Err(OfflineProvisioningError::Store(error));
        }

        match authority.compare_and_persist(None, &replacement) {
            CheckpointPersistence::Durable => {
                let store = PersistentAccountStore::open(&root, &replacement)
                    .map_err(OfflineProvisioningError::Store)?;
                journal_root
                    .clear_provisioning_journal_if_matches(&journal)
                    .map_err(map_journal_error)?;
                Ok(Self {
                    root,
                    state: ProvisioningState::Active {
                        store,
                        checkpoint: replacement,
                    },
                })
            }
            CheckpointPersistence::Conflict => {
                staged
                    .abort_initialization()
                    .map_err(OfflineProvisioningError::Store)?;
                journal_root
                    .clear_provisioning_journal_if_matches(&journal)
                    .map_err(map_journal_error)?;
                Err(OfflineProvisioningError::CheckpointConflict(Box::new(
                    pending_checkpoint(None, replacement),
                )))
            }
            outcome => Err(checkpoint_failure(
                outcome,
                pending_checkpoint(None, replacement),
            )),
        }
    }

    /// Reconciles one durable initialization journal without deriving a
    /// checkpoint from an untrusted account snapshot.
    pub fn reconcile_crash_safe_initialization<
        A: AccountStoreCheckpointAuthority + AccountStoreCheckpointReader,
    >(
        root: impl AsRef<Path>,
        authority_id: &CheckpointAuthorityId,
        authority: &mut A,
        deadline: Instant,
    ) -> Result<InitializationReconcileOutcome, OfflineProvisioningError> {
        let root = root.as_ref();
        let journal_root = AccountStoreRoot::open(root).map_err(map_journal_error)?;
        let _lock = journal_root
            .acquire_provisioning_lock_until(deadline)
            .map_err(map_journal_error)?;
        let Some(journal) = journal_root
            .read_provisioning_journal()
            .map_err(map_journal_error)?
        else {
            return Ok(InitializationReconcileOutcome::NoPendingUpdate);
        };
        let pending = PendingAccountStoreUpdate::decode(&journal)?;
        if pending.authority() != authority_id {
            return Err(OfflineProvisioningError::PendingAuthorityMismatch);
        }

        match PersistentAccountStore::open(root, pending.replacement()) {
            Ok(_) => {}
            Err(PersistentAccountStoreError::MissingSnapshot) => {
                let remaining = deadline.checked_duration_since(Instant::now()).ok_or(
                    OfflineProvisioningError::CheckpointRead(CheckpointReadError::TimedOut),
                )?;
                let request = authority
                    .request_checkpoint(pending.authority())
                    .map_err(OfflineProvisioningError::CheckpointRead)?;
                match request.wait(remaining) {
                    crate::runtime_config::AccountStoreCheckpointWait::Completed(Err(
                        CheckpointReadError::Missing,
                    )) => {
                        journal_root
                            .clear_provisioning_journal_if_matches(&journal)
                            .map_err(map_journal_error)?;
                        return Ok(InitializationReconcileOutcome::AbortedBeforeSnapshot);
                    }
                    crate::runtime_config::AccountStoreCheckpointWait::Completed(Err(error)) => {
                        return Err(OfflineProvisioningError::CheckpointRead(error));
                    }
                    crate::runtime_config::AccountStoreCheckpointWait::Completed(Ok(_)) => {
                        return Err(OfflineProvisioningError::CheckpointMismatch);
                    }
                    crate::runtime_config::AccountStoreCheckpointWait::TimedOut(_) => {
                        return Err(OfflineProvisioningError::CheckpointRead(
                            CheckpointReadError::TimedOut,
                        ));
                    }
                    crate::runtime_config::AccountStoreCheckpointWait::Stopped(_) => {
                        return Err(OfflineProvisioningError::CheckpointRead(
                            CheckpointReadError::Unavailable,
                        ));
                    }
                }
            }
            Err(_) => return Err(OfflineProvisioningError::PendingJournalInvalid),
        }
        match authority.compare_and_persist(pending.expected(), pending.replacement()) {
            CheckpointPersistence::Durable => {
                let store = PersistentAccountStore::open(root, pending.replacement())
                    .map_err(OfflineProvisioningError::Store)?;
                let revision = store.revision().map_err(OfflineProvisioningError::Store)?;
                journal_root
                    .clear_provisioning_journal_if_matches(&journal)
                    .map_err(map_journal_error)?;
                Ok(InitializationReconcileOutcome::Reconciled { revision })
            }
            outcome => Err(checkpoint_failure(
                outcome,
                pending_checkpoint(None, *pending.replacement()),
            )),
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
        debug.field("root", &"<redacted>");
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

fn pending_checkpoint(
    expected: Option<AccountStoreCheckpoint>,
    replacement: AccountStoreCheckpoint,
) -> PendingAccountCheckpoint {
    PendingAccountCheckpoint {
        expected,
        replacement,
        durable_revision: replacement.revision(),
    }
}

fn map_journal_error(error: AccountStoreFsError) -> OfflineProvisioningError {
    match error {
        AccountStoreFsError::ProvisioningLockTimedOut => OfflineProvisioningError::ProvisioningBusy,
        AccountStoreFsError::InvalidEntry | AccountStoreFsError::SnapshotTooLarge => {
            OfflineProvisioningError::PendingJournalInvalid
        }
        AccountStoreFsError::InvalidRoot | AccountStoreFsError::Backend => {
            OfflineProvisioningError::Store(PersistentAccountStoreError::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
        thread,
        time::{Duration, Instant},
    };

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{AccountStoreCheckpointError, AccountStoreCheckpointRequest, CredentialProvider};

    struct MemoryAuthority {
        checkpoint: Option<AccountStoreCheckpoint>,
        next: CheckpointPersistence,
        read: Option<Result<AccountStoreCheckpoint, CheckpointReadError>>,
    }

    impl MemoryAuthority {
        fn new(next: CheckpointPersistence) -> Self {
            Self {
                checkpoint: None,
                next,
                read: None,
            }
        }

        fn set_read(&mut self, read: Result<AccountStoreCheckpoint, CheckpointReadError>) {
            self.read = Some(read);
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

    impl AccountStoreCheckpointReader for MemoryAuthority {
        fn request_checkpoint(
            &self,
            _authority: &CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
            Ok(AccountStoreCheckpointRequest::completed(
                self.read
                    .unwrap_or_else(|| self.checkpoint.ok_or(CheckpointReadError::Missing)),
            ))
        }
    }

    struct ConflictAuthority;

    impl AccountStoreCheckpointAuthority for ConflictAuthority {
        fn compare_and_persist(
            &mut self,
            _expected: Option<&AccountStoreCheckpoint>,
            _replacement: &AccountStoreCheckpoint,
        ) -> CheckpointPersistence {
            CheckpointPersistence::Conflict
        }
    }

    struct ReplacingConflictAuthority {
        root: std::path::PathBuf,
        replacement_checkpoint: Option<AccountStoreCheckpoint>,
    }

    impl AccountStoreCheckpointAuthority for ReplacingConflictAuthority {
        fn compare_and_persist(
            &mut self,
            _expected: Option<&AccountStoreCheckpoint>,
            _replacement: &AccountStoreCheckpoint,
        ) -> CheckpointPersistence {
            let snapshot = self.root.join(".turso-mysql-authz-v1");
            fs::remove_file(snapshot).unwrap();
            let replacement =
                PersistentAccountStore::initialize(&self.root, builder(0x22)).unwrap();
            self.replacement_checkpoint = Some(replacement.checkpoint().unwrap());
            CheckpointPersistence::Conflict
        }
    }

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn authority_id() -> CheckpointAuthorityId {
        CheckpointAuthorityId::new("accounts").unwrap()
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
        assert!(!debug.contains("alice"));
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
        assert!(!format!("{provisioner:?}").contains(&root.path().display().to_string()));
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
    fn initial_checkpoint_conflict_aborts_its_snapshot_for_retry() {
        let root = root();
        let mut authority = ConflictAuthority;
        let snapshot = root.path().join(".turso-mysql-authz-v1");
        assert!(matches!(
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority),
            Err(OfflineProvisioningError::CheckpointConflict(pending))
                if pending.durable_revision() == 0
        ));
        assert!(!snapshot.exists());

        assert!(matches!(
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority),
            Err(OfflineProvisioningError::CheckpointConflict(_))
        ));
        assert!(!snapshot.exists());
    }

    #[test]
    fn initial_conflict_does_not_remove_a_replacement_snapshot() {
        let root = root();
        let mut authority = ReplacingConflictAuthority {
            root: root.path().to_owned(),
            replacement_checkpoint: None,
        };
        assert!(matches!(
            OfflineAccountProvisioner::initialize(root.path(), builder(0x11), &mut authority),
            Err(OfflineProvisioningError::Store(
                PersistentAccountStoreError::Conflict
            ))
        ));
        let replacement_checkpoint = authority.replacement_checkpoint.unwrap();
        assert_eq!(
            OfflineAccountProvisioner::open(root.path(), replacement_checkpoint)
                .unwrap()
                .revision(),
            Ok(0)
        );
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
        let replacement_checkpoint = match provisioner.replace(builder(0x22), &mut authority) {
            Err(OfflineProvisioningError::CheckpointAmbiguous(pending)) => {
                assert_eq!(pending.durable_revision(), 1);
                *pending.replacement()
            }
            other => panic!("expected an ambiguous checkpoint result, got {other:?}"),
        };
        assert_eq!(
            OfflineAccountProvisioner::open(root.path(), replacement_checkpoint)
                .unwrap()
                .revision(),
            Ok(1)
        );
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

    #[test]
    fn crash_safe_initialization_writes_then_removes_its_pending_journal() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let provisioner = OfflineAccountProvisioner::initialize_crash_safe(
            root.path(),
            authority_id(),
            builder(0x11),
            &mut authority,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(provisioner.revision(), Ok(0));
        assert!(AccountStoreRoot::open(root.path())
            .unwrap()
            .read_provisioning_journal()
            .unwrap()
            .is_none());
    }

    #[test]
    fn crash_safe_initialization_reconciles_an_ambiguous_exact_transition() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Ambiguous);
        assert!(matches!(
            OfflineAccountProvisioner::initialize_crash_safe(
                root.path(),
                authority_id(),
                builder(0x11),
                &mut authority,
                Instant::now() + Duration::from_secs(1),
            ),
            Err(OfflineProvisioningError::CheckpointAmbiguous(_))
        ));
        authority.next = CheckpointPersistence::Durable;
        assert_eq!(
            OfflineAccountProvisioner::reconcile_crash_safe_initialization(
                root.path(),
                &authority_id(),
                &mut authority,
                Instant::now() + Duration::from_secs(1),
            ),
            Ok(InitializationReconcileOutcome::Reconciled { revision: 0 })
        );
    }

    #[test]
    fn reconciliation_clears_a_journal_without_its_initial_snapshot() {
        let source = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let checkpoint =
            OfflineAccountProvisioner::initialize(source.path(), builder(0x11), &mut authority)
                .unwrap()
                .checkpoint()
                .unwrap();
        let target = root();
        let journal = PendingAccountStoreUpdate::new_initialization(authority_id(), checkpoint)
            .encode()
            .unwrap();
        AccountStoreRoot::open(target.path())
            .unwrap()
            .publish_provisioning_journal(&journal)
            .unwrap();
        authority.set_read(Err(CheckpointReadError::Missing));
        assert_eq!(
            OfflineAccountProvisioner::reconcile_crash_safe_initialization(
                target.path(),
                &authority_id(),
                &mut authority,
                Instant::now() + Duration::from_secs(1),
            ),
            Ok(InitializationReconcileOutcome::AbortedBeforeSnapshot)
        );
    }

    #[test]
    fn reconciliation_keeps_a_snapshotless_journal_until_authority_is_missing() {
        let source = root();
        let mut source_authority = MemoryAuthority::new(CheckpointPersistence::Durable);
        let replacement = OfflineAccountProvisioner::initialize(
            source.path(),
            builder(0x11),
            &mut source_authority,
        )
        .unwrap()
        .checkpoint()
        .unwrap();
        let other_source = root();
        let other = OfflineAccountProvisioner::initialize(
            other_source.path(),
            builder(0x22),
            &mut MemoryAuthority::new(CheckpointPersistence::Durable),
        )
        .unwrap()
        .checkpoint()
        .unwrap();

        for read in [
            Ok(replacement),
            Ok(other),
            Err(CheckpointReadError::Unavailable),
            Err(CheckpointReadError::TimedOut),
        ] {
            let target = root();
            let journal =
                PendingAccountStoreUpdate::new_initialization(authority_id(), replacement)
                    .encode()
                    .unwrap();
            let journal_root = AccountStoreRoot::open(target.path()).unwrap();
            journal_root.publish_provisioning_journal(&journal).unwrap();
            let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
            authority.set_read(read);
            let result = OfflineAccountProvisioner::reconcile_crash_safe_initialization(
                target.path(),
                &authority_id(),
                &mut authority,
                Instant::now() + Duration::from_secs(1),
            );
            match read {
                Ok(_) => assert_eq!(result, Err(OfflineProvisioningError::CheckpointMismatch)),
                Err(error) => {
                    assert_eq!(result, Err(OfflineProvisioningError::CheckpointRead(error)))
                }
            }
            assert!(journal_root.read_provisioning_journal().unwrap().is_some());
        }
    }

    #[test]
    fn reconciliation_rejects_a_corrupt_or_replaced_journal() {
        let root = root();
        let mut authority = MemoryAuthority::new(CheckpointPersistence::Ambiguous);
        let _ = OfflineAccountProvisioner::initialize_crash_safe(
            root.path(),
            authority_id(),
            builder(0x11),
            &mut authority,
            Instant::now() + Duration::from_secs(1),
        );
        let journal_path = root.path().join(".turso-mysql-provision-pending-v1");
        let valid_bytes = fs::read(&journal_path).unwrap();
        let mut bytes = valid_bytes.clone();
        bytes[0] ^= 1;
        fs::write(&journal_path, bytes).unwrap();
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            OfflineAccountProvisioner::reconcile_crash_safe_initialization(
                root.path(),
                &authority_id(),
                &mut authority,
                Instant::now() + Duration::from_secs(1),
            ),
            Err(OfflineProvisioningError::PendingJournalInvalid)
        );
        fs::remove_file(&journal_path).unwrap();
        fs::write(&journal_path, valid_bytes).unwrap();
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            OfflineAccountProvisioner::reconcile_crash_safe_initialization(
                root.path(),
                &authority_id(),
                &mut authority,
                Instant::now() + Duration::from_secs(1),
            ),
            Err(OfflineProvisioningError::PendingJournalInvalid)
        );
        fs::remove_file(&journal_path).unwrap();
        symlink("outside", &journal_path).unwrap();
        assert_eq!(
            OfflineAccountProvisioner::reconcile_crash_safe_initialization(
                root.path(),
                &authority_id(),
                &mut authority,
                Instant::now() + Duration::from_secs(1),
            ),
            Err(OfflineProvisioningError::PendingJournalInvalid)
        );
    }

    #[test]
    fn crash_safe_initialization_times_out_behind_another_provisioner() {
        let root = root();
        let lock_root = AccountStoreRoot::open(root.path()).unwrap();
        let lock = lock_root
            .acquire_provisioning_lock_until(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let path = root.path().to_owned();
        let result = thread::spawn(move || {
            let mut authority = MemoryAuthority::new(CheckpointPersistence::Durable);
            OfflineAccountProvisioner::initialize_crash_safe(
                path,
                authority_id(),
                builder(0x11),
                &mut authority,
                Instant::now() + Duration::from_millis(30),
            )
        })
        .join()
        .unwrap();
        assert!(matches!(
            result,
            Err(OfflineProvisioningError::ProvisioningBusy)
        ));
        drop(lock);
    }
}
