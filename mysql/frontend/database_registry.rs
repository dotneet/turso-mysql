//! Trusted logical-database registry state.
//!
//! Filesystem access is capability based: the Unix backend holds an already-open
//! data-root directory and performs all child operations relative to it. The
//! backend's durable identity metadata is intentionally separate from the
//! format-v2 page marker and core attachment work.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
#[path = "filesystem_backend.rs"]
mod filesystem_backend;

const ROOT_MANIFEST_VERSION: u32 = 1;
const MYSQL_OWNER_MARKER_VERSION: u8 = 2;
const MAX_DATABASE_NAME_BYTES: usize = 64;
static NEXT_REGISTRY_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// The only root-wide table-name policy accepted by this registry slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NamePolicy {
    LowerCaseTableNames1,
}

impl NamePolicy {
    fn lower_case_table_names(self) -> u8 {
        match self {
            Self::LowerCaseTableNames1 => 1,
        }
    }
}

/// Durable root policy, written before a logical database can be created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootManifest {
    version: u32,
    name_policy: NamePolicy,
}

impl RootManifest {
    pub(crate) const fn lower_case_table_names_1() -> Self {
        Self {
            version: ROOT_MANIFEST_VERSION,
            name_policy: NamePolicy::LowerCaseTableNames1,
        }
    }

    fn validate(&self) -> Result<(), RegistryError> {
        if self.version != ROOT_MANIFEST_VERSION {
            return Err(RegistryError::UnsupportedManifestVersion(self.version));
        }
        if self.name_policy != NamePolicy::LowerCaseTableNames1 {
            return Err(RegistryError::UnsupportedNamePolicy);
        }
        Ok(())
    }
}

/// A canonical MySQL logical-database name, never a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct DatabaseName(String);

impl DatabaseName {
    pub(crate) fn parse(name: &str) -> Result<Self, RegistryError> {
        if name.is_empty() {
            return Err(RegistryError::EmptyDatabaseName);
        }
        if name.len() > MAX_DATABASE_NAME_BYTES {
            return Err(RegistryError::DatabaseNameTooLong);
        }
        if matches!(name, "." | "..") {
            return Err(RegistryError::ReservedDatabaseName);
        }

        let mut canonical = String::with_capacity(name.len());
        for byte in name.bytes() {
            let byte = match byte {
                b'A'..=b'Z' => byte.to_ascii_lowercase(),
                b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$' => byte,
                0 => return Err(RegistryError::NulInDatabaseName),
                b'/' | b'\\' => return Err(RegistryError::SeparatorInDatabaseName),
                0x80..=u8::MAX => return Err(RegistryError::NonAsciiDatabaseName),
                _ => return Err(RegistryError::InvalidDatabaseNameCharacter),
            };
            canonical.push(char::from(byte));
        }
        if reserved_database_name(&canonical) {
            return Err(RegistryError::ReservedDatabaseName);
        }
        Ok(Self(canonical))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn reserved_database_name(name: &str) -> bool {
    matches!(
        name,
        "information_schema"
            | "mysql"
            | "performance_schema"
            | "sys"
            | "main"
            | "temp"
            | "sqlite_master"
            | "sqlite_schema"
    )
}

/// An opaque file identity allocated by the trusted root backend.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct OpaqueFileKey(String);

impl OpaqueFileKey {
    pub(crate) fn new(key: String) -> Result<Self, RegistryError> {
        let payload = key.as_bytes().get(3..).unwrap_or_default();
        if !key.starts_with("db_")
            || key.len() != 35
            || !payload
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            || payload.iter().all(|byte| *byte == b'0')
        {
            return Err(RegistryError::InvalidOpaqueFileKey);
        }
        Ok(Self(key))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Decodes the durable key payload into the database identity used by
    /// schema metadata. This remains fallible because deserialization bypasses
    /// [`OpaqueFileKey::new`]; callers must not derive identity from a path,
    /// inode, or logical database name instead.
    pub(crate) fn to_database_identity(&self) -> Result<[u8; 16], RegistryError> {
        OpaqueFileKey::new(self.0.clone())?;
        let mut identity = [0; 16];
        for (index, byte) in identity.iter_mut().enumerate() {
            let offset = 3 + index * 2;
            let high = decode_lower_hex(self.0.as_bytes()[offset])?;
            let low = decode_lower_hex(self.0.as_bytes()[offset + 1])?;
            *byte = (high << 4) | low;
        }
        Ok(identity)
    }
}

fn decode_lower_hex(byte: u8) -> Result<u8, RegistryError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(RegistryError::InvalidOpaqueFileKey),
    }
}

/// The policy proof a new database file must receive before user SQL exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MySqlOwnerMarkerV2 {
    pub(crate) version: u8,
    pub(crate) owner: FrontendOwner,
    pub(crate) lower_case_table_names: u8,
    pub(crate) reserved_bits: u8,
}

/// The v2 marker owner field; accepting another owner would mix frontend files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrontendOwner {
    MySql,
}

impl MySqlOwnerMarkerV2 {
    pub(crate) fn for_policy(policy: NamePolicy) -> Self {
        Self {
            version: MYSQL_OWNER_MARKER_VERSION,
            owner: FrontendOwner::MySql,
            lower_case_table_names: policy.lower_case_table_names(),
            reserved_bits: 0,
        }
    }

    pub(crate) fn validate_for_policy(self, policy: NamePolicy) -> bool {
        self.version == MYSQL_OWNER_MARKER_VERSION
            && self.owner == FrontendOwner::MySql
            && self.lower_case_table_names == policy.lower_case_table_names()
            && self.reserved_bits == 0
    }
}

/// The exact durable proof a registry entry expects from its database file.
///
/// `file_key` is stored inside the database file as its logical-database
/// identity. A valid MySQL file copied over another key is therefore a
/// mismatch, even when its owner marker and name policy are otherwise valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseFileExpectation {
    file_key: OpaqueFileKey,
    marker: MySqlOwnerMarkerV2,
}

impl DatabaseFileExpectation {
    fn new(file_key: OpaqueFileKey, marker: MySqlOwnerMarkerV2) -> Self {
        Self { file_key, marker }
    }

    pub(crate) fn file_key(&self) -> &OpaqueFileKey {
        &self.file_key
    }

    pub(crate) fn marker(&self) -> MySqlOwnerMarkerV2 {
        self.marker
    }
}

/// A database is visible to callers only in the ready state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DatabaseState {
    Creating,
    Ready,
    Dropping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryEntry {
    pub(crate) file_key: OpaqueFileKey,
    pub(crate) state: DatabaseState,
}

/// Durable registry state written atomically by the root backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistrySnapshot {
    pub(crate) entries: BTreeMap<DatabaseName, RegistryEntry>,
}

/// Errors are deliberately path-free because logical database callers must not
/// learn the configured root or host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryError {
    EmptyDatabaseName,
    DatabaseNameTooLong,
    NulInDatabaseName,
    SeparatorInDatabaseName,
    NonAsciiDatabaseName,
    InvalidDatabaseNameCharacter,
    ReservedDatabaseName,
    InvalidOpaqueFileKey,
    DuplicateOpaqueFileKey,
    NonCanonicalDatabaseName,
    UnsupportedManifestVersion(u32),
    UnsupportedNamePolicy,
    Backend,
    RegistryAlreadyOpen,
    RegistryPoisoned,
    DatabaseAlreadyExists(DatabaseName),
    DatabaseNotFound(DatabaseName),
    DatabaseBusy(DatabaseName),
    DatabaseNotReady(DatabaseName),
    DatabaseMarkerMismatch(DatabaseName),
    InvalidRegistryState,
}

/// The only filesystem observations the registry may use for destructive work.
///
/// Matching means both the MySQL v2 owner/policy marker and the persisted
/// logical-database identity match the requested opaque key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatabaseFileInspection {
    /// Neither the main artifact nor its registry-private companion exists.
    Missing,
    /// Exactly one valid artifact exists. Only interrupted lifecycle recovery
    /// may remove this state.
    Partial,
    Matching,
    Mismatch,
}

/// The result of opening and checking a database file through a root capability.
pub(crate) enum OpenDatabaseInspection<H> {
    Missing,
    Matching(H),
    Mismatch,
}

/// Capability-only persistent storage for the registry root.
///
/// Implementations must not interpret a logical database name as a path.
/// `stage_database_new`, `publish_database_stage_new`, `inspect_database`, `unlink_database`, and registry
/// replacement must operate relative to a retained root directory handle, with
/// no-follow and beneath-root guarantees. `replace_registry` is not successful
/// until its create-new temporary file is synced, renamed in that directory,
/// and the directory itself is synced.
pub(crate) trait RegistryRoot {
    /// Every clone must keep the same exclusive root lock held. The lock may
    /// be released only after the registry and every database lease are gone.
    type RegistryLock: Clone;
    /// An already-open main/companion descriptor bundle that passed the owner's
    /// identity checks.
    type DatabaseHandle;
    /// Private main/companion descriptors prepared before they become visible.
    type DatabaseStage;

    /// Acquires the root-wide exclusive registry lock for this registry's lifetime.
    fn acquire_exclusive_registry_lock(&mut self) -> Result<Self::RegistryLock, RegistryError>;
    fn read_manifest(&mut self) -> Result<Option<RootManifest>, RegistryError>;
    fn create_manifest_new(&mut self, manifest: &RootManifest) -> Result<(), RegistryError>;
    /// `None` means the durable registry file is absent. The caller may use
    /// that only while creating a brand-new manifest; an existing manifest
    /// with no registry is an incomplete root and must fail closed.
    fn read_registry(&mut self) -> Result<Option<RegistrySnapshot>, RegistryError>;
    /// Atomically replaces the registry and durably syncs the containing root.
    fn replace_registry(&mut self, registry: &RegistrySnapshot) -> Result<(), RegistryError>;
    fn allocate_file_key(&mut self) -> Result<OpaqueFileKey, RegistryError>;
    /// Returns `Missing` only when all final and lifecycle-private names for
    /// this key are available for a fresh create.
    fn inspect_database_creation(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<DatabaseFileInspection, RegistryError>;
    /// Creates private main and companion artifacts and writes their durable
    /// owner and identity envelopes.
    fn stage_database_new(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<Self::DatabaseStage, RegistryError>;
    /// Syncs all bytes in a private database stage before publication.
    fn sync_database_stage(&mut self, stage: &Self::DatabaseStage) -> Result<(), RegistryError>;
    /// Publishes a fully synced stage without replacing an existing name.
    ///
    /// Once this method starts, an error may leave an ambiguous published
    /// bundle. Callers must preserve durable `Creating` state in that case.
    fn publish_database_stage_new(
        &mut self,
        expected: &DatabaseFileExpectation,
        stage: Self::DatabaseStage,
    ) -> Result<(), RegistryError>;
    /// Removes only private stage names. This is valid before publication has
    /// started; final names are never removed by this operation.
    fn abort_database_stage(
        &mut self,
        expected: &DatabaseFileExpectation,
        stage: Self::DatabaseStage,
    ) -> Result<(), RegistryError>;
    /// Checks the owner marker, name policy, and persisted opaque identity.
    ///
    /// A file with the right marker but an identity for another key is
    /// `Mismatch`, not `Matching`.
    fn inspect_database(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<DatabaseFileInspection, RegistryError>;
    /// Opens and checks the database artifacts once. A matching handle must
    /// retain the exact inspected main and registry-private companion files,
    /// not reopen either one by a logical database name.
    fn open_database(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<OpenDatabaseInspection<Self::DatabaseHandle>, RegistryError>;
    /// Re-checks and removes `expected`, returning success when it is absent.
    ///
    /// This makes recovery of interrupted creating and dropping records
    /// idempotent while requiring the file identity to be revalidated.
    fn unlink_database(&mut self, expected: &DatabaseFileExpectation) -> Result<(), RegistryError>;
    fn fsync_dir(&mut self) -> Result<(), RegistryError>;
}

struct LeaseTable {
    counts: Mutex<BTreeMap<DatabaseName, usize>>,
}

struct LeasePermit {
    table: Arc<LeaseTable>,
    name: DatabaseName,
}

impl LeaseTable {
    fn acquire(self: &Arc<Self>, name: DatabaseName) -> LeasePermit {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = counts.entry(name.clone()).or_default();
        *count = count
            .checked_add(1)
            .expect("database lease count must not overflow");
        LeasePermit {
            table: Arc::clone(self),
            name,
        }
    }

    fn contains(&self, name: &DatabaseName) -> bool {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(name)
    }

    fn release(&self, name: &DatabaseName) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = {
            let count = counts
                .get_mut(name)
                .expect("database lease permit must have a matching count");
            assert!(*count > 0, "database lease count must not underflow");
            *count -= 1;
            *count == 0
        };
        if remove {
            counts.remove(name);
        }
    }
}

impl Drop for LeasePermit {
    fn drop(&mut self) {
        self.table.release(&self.name);
    }
}

/// Trusted registry operations with no SQL, raw ATTACH, or raw path entrypoint.
pub(crate) struct DatabaseRegistry<R: RegistryRoot> {
    root: R,
    _lock: R::RegistryLock,
    manifest: RootManifest,
    snapshot: RegistrySnapshot,
    leases: Arc<LeaseTable>,
    instance_id: u64,
    poisoned: bool,
}

impl<R: RegistryRoot> DatabaseRegistry<R> {
    pub(crate) fn open_or_create(mut root: R) -> Result<Self, RegistryError> {
        let lock = root.acquire_exclusive_registry_lock()?;
        // Read both durable markers before writing either one. A one-sided
        // root is an interrupted bootstrap and must remain untouched.
        let on_disk_manifest = root.read_manifest()?;
        let on_disk_registry = root.read_registry()?;
        let (manifest, snapshot) = match (on_disk_manifest, on_disk_registry) {
            (None, None) => {
                let manifest = RootManifest::lower_case_table_names_1();
                root.create_manifest_new(&manifest)?;
                root.fsync_dir()?;
                let snapshot = RegistrySnapshot::default();
                root.replace_registry(&snapshot)?;
                (manifest, snapshot)
            }
            (Some(manifest), Some(snapshot)) => {
                manifest.validate()?;
                (manifest, snapshot)
            }
            (Some(_), None) | (None, Some(_)) => return Err(RegistryError::Backend),
        };
        let mut registry = Self {
            root,
            _lock: lock,
            manifest,
            snapshot,
            leases: Arc::new(LeaseTable {
                counts: Mutex::new(BTreeMap::new()),
            }),
            instance_id: next_registry_instance_id(),
            poisoned: false,
        };
        registry.validate_snapshot()?;
        registry.recover_incomplete_operations()?;
        Ok(registry)
    }

    pub(crate) fn create(&mut self, requested_name: &str) -> Result<DatabaseName, RegistryError> {
        self.create_with_initializer(requested_name, |_, _, lifetime| {
            drop(lifetime);
            Ok(())
        })
        .map(|(name, ())| name)
    }

    /// Creates a database and lets the initializer build a value that owns its
    /// lifetime lease. The initializer must move that lease into `T` whenever
    /// `T` can outlive this call; dropping the lease releases the database's
    /// busy state and its retained root-lock clone.
    pub(crate) fn create_with_initializer<F, T>(
        &mut self,
        requested_name: &str,
        initializer: F,
    ) -> Result<(DatabaseName, T), RegistryError>
    where
        F: FnOnce(
            &mut R::DatabaseStage,
            &DatabaseFileExpectation,
            DatabaseLifetimeLease<R::RegistryLock>,
        ) -> Result<T, RegistryError>,
    {
        self.ensure_active()?;
        let name = DatabaseName::parse(requested_name)?;
        if self.snapshot.entries.contains_key(&name) {
            return Err(RegistryError::DatabaseAlreadyExists(name));
        }
        let allocation = self.root.allocate_file_key();
        let file_key = self.poison_on_backend_error(allocation)?;
        OpaqueFileKey::new(file_key.0.clone())?;
        if self
            .snapshot
            .entries
            .values()
            .any(|entry| entry.file_key == file_key)
        {
            return Err(RegistryError::DuplicateOpaqueFileKey);
        }
        let expected = DatabaseFileExpectation::new(file_key.clone(), self.owner_marker());
        let inspection = self.root.inspect_database_creation(&expected);
        if self.poison_on_backend_error(inspection)? != DatabaseFileInspection::Missing {
            return Err(RegistryError::DuplicateOpaqueFileKey);
        }
        let entry = RegistryEntry {
            file_key,
            state: DatabaseState::Creating,
        };
        self.snapshot.entries.insert(name.clone(), entry);
        self.persist_snapshot()?;

        let stage_result = self.root.stage_database_new(&expected);
        let mut stage = self.poison_on_backend_error(stage_result)?;
        let lifetime = DatabaseLifetimeLease {
            _permit: self.leases.acquire(name.clone()),
            _lock: self._lock.clone(),
        };
        let initialized = match initializer(&mut stage, &expected, lifetime) {
            Ok(initialized) => initialized,
            Err(error) => return self.abort_stage_after_failure(&expected, stage, error),
        };
        let sync = self.root.sync_database_stage(&stage);
        if let Err(error) = sync {
            drop(initialized);
            self.poisoned = true;
            return match self.root.abort_database_stage(&expected, stage) {
                Ok(()) => Err(error),
                Err(abort_error) => Err(abort_error),
            };
        }
        // The stage is consumed here. An error can mean that one or both final
        // names already point at the stage, so no cleanup may unlink by name.
        let publish = self.root.publish_database_stage_new(&expected, stage);
        self.poison_on_backend_error(publish)?;
        let directory_sync = self.root.fsync_dir();
        self.poison_on_backend_error(directory_sync)?;

        let entry = self
            .snapshot
            .entries
            .get_mut(&name)
            .ok_or(RegistryError::InvalidRegistryState)?;
        entry.state = DatabaseState::Ready;
        self.persist_snapshot()?;
        Ok((name, initialized))
    }

    fn abort_stage_after_failure<T>(
        &mut self,
        expected: &DatabaseFileExpectation,
        stage: R::DatabaseStage,
        error: RegistryError,
    ) -> Result<(DatabaseName, T), RegistryError> {
        self.poisoned = true;
        match self.root.abort_database_stage(expected, stage) {
            Ok(()) => Err(error),
            Err(abort_error) => Err(abort_error),
        }
    }

    pub(crate) fn acquire(
        &mut self,
        requested_name: &str,
    ) -> Result<DatabaseLease<R::RegistryLock, R::DatabaseHandle>, RegistryError> {
        self.ensure_active()?;
        let name = DatabaseName::parse(requested_name)?;
        let entry = self
            .snapshot
            .entries
            .get(&name)
            .ok_or_else(|| RegistryError::DatabaseNotFound(name.clone()))?;
        if entry.state != DatabaseState::Ready {
            return Err(RegistryError::DatabaseNotReady(name));
        }
        let file_key = entry.file_key.clone();
        let expected = DatabaseFileExpectation::new(file_key.clone(), self.owner_marker());
        let opened = self.root.open_database(&expected);
        let handle = match self.poison_on_backend_error(opened)? {
            OpenDatabaseInspection::Matching(handle) => handle,
            OpenDatabaseInspection::Missing | OpenDatabaseInspection::Mismatch => {
                return Err(RegistryError::DatabaseMarkerMismatch(name));
            }
        };
        let permit = self.leases.acquire(name.clone());
        Ok(DatabaseLease {
            permit,
            database_handle: handle,
            _lock: self._lock.clone(),
            registry_instance_id: self.instance_id,
            name,
            file_key,
        })
    }

    pub(crate) fn release<H>(
        &mut self,
        lease: DatabaseLease<R::RegistryLock, H>,
    ) -> Result<(), RegistryError> {
        self.ensure_active()?;
        if lease.registry_instance_id != self.instance_id
            || self
                .snapshot
                .entries
                .get(&lease.name)
                .map(|entry| &entry.file_key)
                != Some(&lease.file_key)
        {
            return Err(RegistryError::InvalidRegistryState);
        }
        if !self.leases.contains(&lease.name) {
            return Err(RegistryError::InvalidRegistryState);
        }
        Ok(())
    }

    pub(crate) fn drop_database(&mut self, requested_name: &str) -> Result<(), RegistryError> {
        self.ensure_active()?;
        let name = DatabaseName::parse(requested_name)?;
        if self.leases.contains(&name) {
            return Err(RegistryError::DatabaseBusy(name));
        }
        let entry = self
            .snapshot
            .entries
            .get(&name)
            .ok_or_else(|| RegistryError::DatabaseNotFound(name.clone()))?;
        if entry.state != DatabaseState::Ready {
            return Err(RegistryError::DatabaseNotReady(name));
        }
        let file_key = entry.file_key.clone();
        let expected = DatabaseFileExpectation::new(file_key, self.owner_marker());
        let inspection = self.root.inspect_database(&expected);
        match self.poison_on_backend_error(inspection)? {
            DatabaseFileInspection::Missing => {}
            DatabaseFileInspection::Matching => {}
            DatabaseFileInspection::Partial | DatabaseFileInspection::Mismatch => {
                return Err(RegistryError::DatabaseMarkerMismatch(name));
            }
        }
        let entry = self
            .snapshot
            .entries
            .get_mut(&name)
            .ok_or(RegistryError::InvalidRegistryState)?;
        entry.state = DatabaseState::Dropping;
        self.persist_snapshot()?;
        let unlink = self.root.unlink_database(&expected);
        self.poison_on_backend_error(unlink)?;
        self.snapshot.entries.remove(&name);
        self.persist_snapshot()
    }

    pub(crate) fn contains(&self, requested_name: &str) -> Result<bool, RegistryError> {
        self.ensure_active()?;
        let name = DatabaseName::parse(requested_name)?;
        Ok(matches!(
            self.snapshot.entries.get(&name),
            Some(RegistryEntry {
                state: DatabaseState::Ready,
                ..
            })
        ))
    }

    pub(crate) fn ready_databases(&self) -> Result<Vec<DatabaseName>, RegistryError> {
        self.ensure_active()?;
        Ok(self
            .snapshot
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == DatabaseState::Ready)
            .map(|(name, _)| name.clone())
            .collect())
    }

    fn owner_marker(&self) -> MySqlOwnerMarkerV2 {
        MySqlOwnerMarkerV2::for_policy(self.manifest.name_policy)
    }

    fn persist_snapshot(&mut self) -> Result<(), RegistryError> {
        let replace = self.root.replace_registry(&self.snapshot);
        self.poison_on_backend_error(replace)
    }

    fn ensure_active(&self) -> Result<(), RegistryError> {
        if self.poisoned {
            return Err(RegistryError::RegistryPoisoned);
        }
        Ok(())
    }

    fn poison_on_backend_error<T>(
        &mut self,
        result: Result<T, RegistryError>,
    ) -> Result<T, RegistryError> {
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn validate_snapshot(&self) -> Result<(), RegistryError> {
        let mut file_keys = BTreeSet::new();
        for (name, entry) in &self.snapshot.entries {
            if DatabaseName::parse(name.as_str())? != *name {
                return Err(RegistryError::NonCanonicalDatabaseName);
            }
            OpaqueFileKey::new(entry.file_key.0.clone())?;
            if !file_keys.insert(&entry.file_key) {
                return Err(RegistryError::DuplicateOpaqueFileKey);
            }
        }
        Ok(())
    }

    fn recover_incomplete_operations(&mut self) -> Result<(), RegistryError> {
        let incomplete = self
            .snapshot
            .entries
            .iter()
            .filter(|(_, entry)| entry.state != DatabaseState::Ready)
            .map(|(name, entry)| (name.clone(), entry.file_key.clone()))
            .collect::<Vec<_>>();
        for (name, file_key) in incomplete {
            let expected = DatabaseFileExpectation::new(file_key, self.owner_marker());
            match self.root.inspect_database(&expected)? {
                DatabaseFileInspection::Missing
                | DatabaseFileInspection::Partial
                | DatabaseFileInspection::Matching => {
                    self.root.unlink_database(&expected)?;
                }
                DatabaseFileInspection::Mismatch => {
                    return Err(RegistryError::DatabaseMarkerMismatch(name));
                }
            }
            self.snapshot.entries.remove(&name);
            self.persist_snapshot()?;
        }
        Ok(())
    }
}

/// A live logical-database selection. Dropping it releases the logical lease.
pub(crate) struct DatabaseLease<L, H> {
    database_handle: H,
    permit: LeasePermit,
    _lock: L,
    registry_instance_id: u64,
    name: DatabaseName,
    file_key: OpaqueFileKey,
}

/// Retains the registry state Core needs while it owns a pre-opened database.
pub(crate) struct DatabaseLifetimeLease<L> {
    // The logical permit must drop before the root lock so another registry
    // cannot observe the lease as released before this guard gives up the lock.
    _permit: LeasePermit,
    _lock: L,
}

impl<L, H> DatabaseLease<L, H> {
    pub(crate) fn name(&self) -> &DatabaseName {
        &self.name
    }

    pub(crate) fn database_handle(&self) -> &H {
        &self.database_handle
    }

    /// Returns the exact durable database identity retained by this lease.
    /// Frontend/Core attachment must use this value rather than deriving an
    /// identity from the logical name or an opened file's host metadata.
    pub(crate) fn database_identity(&self) -> Result<[u8; 16], RegistryError> {
        self.file_key.to_database_identity()
    }

    pub(crate) fn into_core_parts(self) -> (H, DatabaseLifetimeLease<L>) {
        let Self {
            permit,
            database_handle,
            _lock,
            ..
        } = self;
        (
            database_handle,
            DatabaseLifetimeLease {
                _permit: permit,
                _lock,
            },
        )
    }
}

fn next_registry_instance_id() -> u64 {
    let id = NEXT_REGISTRY_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
    assert_ne!(id, 0, "database registry instance IDs must not wrap");
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakeRoot {
        manifest: Option<RootManifest>,
        registry: RegistrySnapshot,
        registry_initialized: bool,
        files: BTreeMap<OpaqueFileKey, FakeFile>,
        next_key: u128,
        mutations: Rc<Cell<usize>>,
        registry_lock: Rc<FakeLockState>,
        events: Rc<RefCell<Vec<&'static str>>>,
        fail_replace: bool,
        fail_create: bool,
        fail_sync: bool,
        fail_publish: bool,
        fail_publish_after_main: bool,
        fail_fsync: bool,
        fail_replace_call: Option<usize>,
        replace_calls: usize,
    }

    impl Default for FakeRoot {
        fn default() -> Self {
            Self {
                manifest: None,
                registry: RegistrySnapshot::default(),
                registry_initialized: false,
                files: BTreeMap::new(),
                next_key: 1,
                mutations: Rc::new(Cell::new(0)),
                registry_lock: Rc::new(FakeLockState::default()),
                events: Rc::new(RefCell::new(Vec::new())),
                fail_replace: false,
                fail_create: false,
                fail_sync: false,
                fail_publish: false,
                fail_publish_after_main: false,
                fail_fsync: false,
                fail_replace_call: None,
                replace_calls: 0,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeFile {
        marker: MySqlOwnerMarkerV2,
        identity: OpaqueFileKey,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeDatabaseHandle(FakeFile);

    struct FakeDatabaseStage {
        file: FakeFile,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    #[derive(Default)]
    struct FakeLockState {
        locked: Cell<bool>,
        holders: Cell<usize>,
    }

    struct FakeRegistryLock(Rc<FakeLockState>);

    impl Clone for FakeRegistryLock {
        fn clone(&self) -> Self {
            self.0.holders.set(self.0.holders.get() + 1);
            Self(Rc::clone(&self.0))
        }
    }

    impl Drop for FakeRegistryLock {
        fn drop(&mut self) {
            let holders = self.0.holders.get().checked_sub(1).unwrap();
            self.0.holders.set(holders);
            if holders == 0 {
                self.0.locked.set(false);
            }
        }
    }

    impl FakeRoot {
        fn event(&self, name: &'static str) {
            self.events.borrow_mut().push(name);
        }
    }

    impl RegistryRoot for FakeRoot {
        type RegistryLock = FakeRegistryLock;
        type DatabaseHandle = FakeDatabaseHandle;
        type DatabaseStage = FakeDatabaseStage;

        fn acquire_exclusive_registry_lock(&mut self) -> Result<Self::RegistryLock, RegistryError> {
            if self.registry_lock.locked.replace(true) {
                return Err(RegistryError::RegistryAlreadyOpen);
            }
            self.registry_lock.holders.set(1);
            Ok(FakeRegistryLock(Rc::clone(&self.registry_lock)))
        }

        fn read_manifest(&mut self) -> Result<Option<RootManifest>, RegistryError> {
            Ok(self.manifest.clone())
        }

        fn create_manifest_new(&mut self, manifest: &RootManifest) -> Result<(), RegistryError> {
            if self.manifest.replace(manifest.clone()).is_some() {
                return Err(RegistryError::Backend);
            }
            self.mutations.set(self.mutations.get() + 1);
            Ok(())
        }

        fn read_registry(&mut self) -> Result<Option<RegistrySnapshot>, RegistryError> {
            Ok(
                (self.registry_initialized || !self.registry.entries.is_empty())
                    .then(|| self.registry.clone()),
            )
        }

        fn replace_registry(&mut self, registry: &RegistrySnapshot) -> Result<(), RegistryError> {
            self.replace_calls += 1;
            self.event("registry");
            if self.fail_replace || self.fail_replace_call == Some(self.replace_calls) {
                return Err(RegistryError::Backend);
            }
            self.registry = registry.clone();
            self.registry_initialized = true;
            self.mutations.set(self.mutations.get() + 1);
            Ok(())
        }

        fn allocate_file_key(&mut self) -> Result<OpaqueFileKey, RegistryError> {
            let key = OpaqueFileKey::new(format!("db_{:032x}", self.next_key))?;
            self.next_key += 1;
            Ok(key)
        }

        fn stage_database_new(
            &mut self,
            expected: &DatabaseFileExpectation,
        ) -> Result<Self::DatabaseStage, RegistryError> {
            self.event("stage");
            if self.fail_create {
                return Err(RegistryError::Backend);
            }
            if self.files.contains_key(expected.file_key()) {
                return Err(RegistryError::Backend);
            }
            Ok(FakeDatabaseStage {
                file: FakeFile {
                    marker: expected.marker(),
                    identity: expected.file_key().clone(),
                },
                events: Rc::clone(&self.events),
            })
        }

        fn sync_database_stage(
            &mut self,
            _stage: &Self::DatabaseStage,
        ) -> Result<(), RegistryError> {
            self.event("sync");
            if self.fail_sync {
                Err(RegistryError::Backend)
            } else {
                Ok(())
            }
        }

        fn publish_database_stage_new(
            &mut self,
            expected: &DatabaseFileExpectation,
            stage: Self::DatabaseStage,
        ) -> Result<(), RegistryError> {
            self.event("publish");
            if self.fail_publish {
                return Err(RegistryError::Backend);
            }
            self.files.insert(expected.file_key().clone(), stage.file);
            self.mutations.set(self.mutations.get() + 1);
            if self.fail_publish_after_main {
                return Err(RegistryError::Backend);
            }
            Ok(())
        }

        fn abort_database_stage(
            &mut self,
            _expected: &DatabaseFileExpectation,
            _stage: Self::DatabaseStage,
        ) -> Result<(), RegistryError> {
            self.event("abort");
            Ok(())
        }

        fn inspect_database_creation(
            &mut self,
            expected: &DatabaseFileExpectation,
        ) -> Result<DatabaseFileInspection, RegistryError> {
            self.inspect_database(expected)
        }

        fn inspect_database(
            &mut self,
            expected: &DatabaseFileExpectation,
        ) -> Result<DatabaseFileInspection, RegistryError> {
            Ok(match self.files.get(expected.file_key()) {
                None => DatabaseFileInspection::Missing,
                Some(found)
                    if expected
                        .marker()
                        .validate_for_policy(NamePolicy::LowerCaseTableNames1)
                        && found.marker == expected.marker()
                        && found.identity == *expected.file_key() =>
                {
                    DatabaseFileInspection::Matching
                }
                Some(_) => DatabaseFileInspection::Mismatch,
            })
        }

        fn open_database(
            &mut self,
            expected: &DatabaseFileExpectation,
        ) -> Result<OpenDatabaseInspection<Self::DatabaseHandle>, RegistryError> {
            Ok(match self.files.get(expected.file_key()) {
                None => OpenDatabaseInspection::Missing,
                Some(found)
                    if expected
                        .marker()
                        .validate_for_policy(NamePolicy::LowerCaseTableNames1)
                        && found.marker == expected.marker()
                        && found.identity == *expected.file_key() =>
                {
                    OpenDatabaseInspection::Matching(FakeDatabaseHandle(found.clone()))
                }
                Some(_) => OpenDatabaseInspection::Mismatch,
            })
        }

        fn unlink_database(
            &mut self,
            expected: &DatabaseFileExpectation,
        ) -> Result<(), RegistryError> {
            if self.inspect_database(expected)? == DatabaseFileInspection::Mismatch {
                return Err(RegistryError::Backend);
            }
            self.files.remove(expected.file_key());
            self.mutations.set(self.mutations.get() + 1);
            Ok(())
        }

        fn fsync_dir(&mut self) -> Result<(), RegistryError> {
            self.event("dir_fsync");
            if self.fail_fsync {
                Err(RegistryError::Backend)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn canonicalizes_names_and_lists_only_ready_databases() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        let created = registry.create("App_DB").unwrap();
        assert_eq!(created.as_str(), "app_db");
        assert_eq!(registry.ready_databases().unwrap(), vec![created.clone()]);
        assert_eq!(
            registry.create("app_db"),
            Err(RegistryError::DatabaseAlreadyExists(created))
        );
        for invalid in ["", ".", "..", "a/b", "a\\b", "a\0b", "café", "main", "a-b"] {
            assert!(DatabaseName::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn staged_create_keeps_initializer_and_durability_order() {
        let root = FakeRoot::default();
        let events = Rc::clone(&root.events);
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        events.borrow_mut().clear();

        let (name, value) = registry
            .create_with_initializer("ordered", |stage, expected, lifetime| {
                assert_eq!(stage.file.identity, *expected.file_key());
                stage.events.borrow_mut().push("initializer");
                drop(lifetime);
                Ok(7u8)
            })
            .unwrap();

        assert_eq!(name.as_str(), "ordered");
        assert_eq!(value, 7);
        assert_eq!(
            events.borrow().as_slice(),
            [
                "registry",
                "stage",
                "initializer",
                "sync",
                "publish",
                "dir_fsync",
                "registry"
            ]
        );
        assert_eq!(registry.snapshot.entries[&name].state, DatabaseState::Ready);
    }

    #[test]
    fn initializer_result_can_retain_lifetime_lease_until_database_drop() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        let (name, lifetime) = registry
            .create_with_initializer("guarded", |_, _, lifetime| Ok(Some(lifetime)))
            .unwrap();

        assert_eq!(
            registry.drop_database(name.as_str()),
            Err(RegistryError::DatabaseBusy(name.clone()))
        );
        drop(lifetime);
        registry.drop_database(name.as_str()).unwrap();
    }

    #[test]
    fn initializer_error_releases_the_minted_lifetime_lease() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();

        let result = registry.create_with_initializer(
            "initializer_guard_error",
            |_,
             _,
             lifetime|
             -> Result<Option<DatabaseLifetimeLease<FakeRegistryLock>>, RegistryError> {
                drop(lifetime);
                Err(RegistryError::Backend)
            },
        );
        assert!(matches!(result, Err(RegistryError::Backend)));
        let name = DatabaseName::parse("initializer_guard_error").unwrap();
        assert!(!registry.leases.contains(&name));
    }

    #[test]
    fn partial_publish_releases_lifetime_lease_when_initializer_result_is_dropped() {
        let root = FakeRoot {
            fail_publish_after_main: true,
            ..FakeRoot::default()
        };
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();

        let result = registry.create_with_initializer(
            "partial_publish_guard",
            |_,
             _,
             lifetime|
             -> Result<Option<DatabaseLifetimeLease<FakeRegistryLock>>, RegistryError> {
                Ok(Some(lifetime))
            },
        );
        assert!(matches!(result, Err(RegistryError::Backend)));
        let name = DatabaseName::parse("partial_publish_guard").unwrap();
        assert!(!registry.leases.contains(&name));
    }

    #[test]
    fn ready_persist_failure_releases_lifetime_lease_when_initializer_result_is_dropped() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        registry.root.fail_replace_call = Some(3);

        let result = registry.create_with_initializer(
            "ready_persist_guard",
            |_,
             _,
             lifetime|
             -> Result<Option<DatabaseLifetimeLease<FakeRegistryLock>>, RegistryError> {
                Ok(Some(lifetime))
            },
        );
        assert!(matches!(result, Err(RegistryError::Backend)));
        let name = DatabaseName::parse("ready_persist_guard").unwrap();
        assert!(!registry.leases.contains(&name));
    }

    #[test]
    fn initializer_failure_aborts_private_stage_without_promoting_ready() {
        let root = FakeRoot::default();
        let events = Rc::clone(&root.events);
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        events.borrow_mut().clear();

        assert_eq!(
            registry.create_with_initializer(
                "initializer_failure",
                |stage, _, lifetime| -> Result<(), RegistryError> {
                    stage.events.borrow_mut().push("initializer");
                    drop(lifetime);
                    Err(RegistryError::Backend)
                }
            ),
            Err(RegistryError::Backend)
        );
        let name = DatabaseName::parse("initializer_failure").unwrap();
        assert_eq!(
            registry.snapshot.entries[&name].state,
            DatabaseState::Creating
        );
        assert!(!registry
            .root
            .files
            .contains_key(&registry.snapshot.entries[&name].file_key));
        assert_eq!(
            events.borrow().as_slice(),
            ["registry", "stage", "initializer", "abort"]
        );
        assert!(matches!(
            registry.acquire(name.as_str()),
            Err(RegistryError::RegistryPoisoned)
        ));
    }

    #[test]
    fn stage_sync_failure_aborts_before_publication() {
        let root = FakeRoot {
            fail_sync: true,
            ..FakeRoot::default()
        };
        let events = Rc::clone(&root.events);
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        events.borrow_mut().clear();

        assert_eq!(registry.create("sync_failure"), Err(RegistryError::Backend));
        let name = DatabaseName::parse("sync_failure").unwrap();
        assert_eq!(
            registry.snapshot.entries[&name].state,
            DatabaseState::Creating
        );
        assert!(registry.root.files.is_empty());
        assert_eq!(
            events.borrow().as_slice(),
            ["registry", "stage", "sync", "abort"]
        );
    }

    #[test]
    fn publish_failure_preserves_creating_state_and_does_not_abort_stage() {
        let root = FakeRoot {
            fail_publish: true,
            ..FakeRoot::default()
        };
        let events = Rc::clone(&root.events);
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        events.borrow_mut().clear();

        assert_eq!(
            registry.create("publish_failure"),
            Err(RegistryError::Backend)
        );
        let name = DatabaseName::parse("publish_failure").unwrap();
        assert_eq!(
            registry.snapshot.entries[&name].state,
            DatabaseState::Creating
        );
        assert!(registry.root.files.is_empty());
        assert_eq!(
            events.borrow().as_slice(),
            ["registry", "stage", "sync", "publish"]
        );
    }

    #[test]
    fn ambiguous_publish_failure_keeps_partial_artifact_for_recovery() {
        let root = FakeRoot {
            fail_publish_after_main: true,
            ..FakeRoot::default()
        };
        let events = Rc::clone(&root.events);
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        events.borrow_mut().clear();

        assert_eq!(
            registry.create("partial_publish"),
            Err(RegistryError::Backend)
        );
        let name = DatabaseName::parse("partial_publish").unwrap();
        let file_key = registry.snapshot.entries[&name].file_key.clone();
        assert_eq!(
            registry.snapshot.entries[&name].state,
            DatabaseState::Creating
        );
        assert!(registry.root.files.contains_key(&file_key));
        assert_eq!(
            events.borrow().as_slice(),
            ["registry", "stage", "sync", "publish"]
        );
    }

    #[test]
    fn directory_sync_failure_does_not_promote_ready_or_unlink_published_artifact() {
        let root = FakeRoot::default();
        let events = Rc::clone(&root.events);
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        registry.root.fail_fsync = true;
        events.borrow_mut().clear();

        assert_eq!(
            registry.create("dir_sync_failure"),
            Err(RegistryError::Backend)
        );
        let name = DatabaseName::parse("dir_sync_failure").unwrap();
        let file_key = registry.snapshot.entries[&name].file_key.clone();
        assert_eq!(
            registry.snapshot.entries[&name].state,
            DatabaseState::Creating
        );
        assert!(registry.root.files.contains_key(&file_key));
        assert_eq!(
            events.borrow().as_slice(),
            ["registry", "stage", "sync", "publish", "dir_fsync"]
        );
    }

    #[test]
    fn ready_persist_failure_keeps_durable_creating_and_published_artifact() {
        let root = FakeRoot::default();
        let events = Rc::clone(&root.events);
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        registry.root.fail_replace_call = Some(3);
        events.borrow_mut().clear();

        assert_eq!(
            registry.create("ready_persist_failure"),
            Err(RegistryError::Backend)
        );
        let name = DatabaseName::parse("ready_persist_failure").unwrap();
        let file_key = registry.snapshot.entries[&name].file_key.clone();
        assert_eq!(registry.snapshot.entries[&name].state, DatabaseState::Ready);
        assert_eq!(
            registry.root.registry.entries[&name].state,
            DatabaseState::Creating
        );
        assert!(registry.root.files.contains_key(&file_key));
        assert_eq!(
            events.borrow().as_slice(),
            [
                "registry",
                "stage",
                "sync",
                "publish",
                "dir_fsync",
                "registry"
            ]
        );
        assert_eq!(
            registry.contains(name.as_str()),
            Err(RegistryError::RegistryPoisoned)
        );
    }

    #[test]
    fn opaque_file_key_decodes_to_stable_database_identity_bytes() {
        let key = OpaqueFileKey::new("db_00112233445566778899aabbccddeeff".to_owned()).unwrap();
        assert_eq!(
            key.to_database_identity().unwrap(),
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );

        let deserialized: OpaqueFileKey =
            serde_json::from_str("\"db_00112233445566778899AABBCCDDEEFF\"").unwrap();
        assert_eq!(
            deserialized.to_database_identity(),
            Err(RegistryError::InvalidOpaqueFileKey)
        );
    }

    #[test]
    fn opaque_file_key_rejects_the_reserved_zero_database_identity() {
        assert_eq!(
            OpaqueFileKey::new("db_00000000000000000000000000000000".to_string()),
            Err(RegistryError::InvalidOpaqueFileKey)
        );
    }

    #[test]
    fn lease_exposes_the_registry_database_identity() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        registry.create("users").unwrap();
        let lease = registry.acquire("users").unwrap();
        assert_eq!(
            lease.database_identity().unwrap(),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        registry.release(lease).unwrap();
        registry.drop_database("users").unwrap();
    }

    #[test]
    fn root_allows_only_one_live_registry() {
        let root = FakeRoot::default();
        let same_root = root.clone();
        let registry = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(matches!(
            DatabaseRegistry::open_or_create(same_root),
            Err(RegistryError::RegistryAlreadyOpen)
        ));
        drop(registry);
    }

    #[test]
    fn one_sided_root_state_is_rejected_without_mutation() {
        let mutations = Rc::new(Cell::new(0));
        let root = FakeRoot {
            manifest: Some(RootManifest::lower_case_table_names_1()),
            mutations: Rc::clone(&mutations),
            ..FakeRoot::default()
        };
        assert!(matches!(
            DatabaseRegistry::open_or_create(root),
            Err(RegistryError::Backend)
        ));
        assert_eq!(mutations.get(), 0);

        let mutations = Rc::new(Cell::new(0));
        let root = FakeRoot {
            registry_initialized: true,
            mutations: Rc::clone(&mutations),
            ..FakeRoot::default()
        };
        assert!(matches!(
            DatabaseRegistry::open_or_create(root),
            Err(RegistryError::Backend)
        ));
        assert_eq!(mutations.get(), 0);
    }

    #[test]
    fn database_lease_keeps_the_root_lock_after_registry_drop() {
        let root = FakeRoot::default();
        let same_root = root.clone();
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        registry.create("live").unwrap();
        let lease = registry.acquire("live").unwrap();

        drop(registry);
        assert!(matches!(
            DatabaseRegistry::open_or_create(same_root.clone()),
            Err(RegistryError::RegistryAlreadyOpen)
        ));

        drop(lease);
        assert!(DatabaseRegistry::open_or_create(same_root).is_ok());
    }

    #[test]
    fn dropping_a_database_lease_releases_the_busy_state() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        let name = registry.create("live").unwrap();
        let lease = registry.acquire("live").unwrap();

        assert_eq!(
            registry.drop_database("live"),
            Err(RegistryError::DatabaseBusy(name))
        );
        drop(lease);
        registry.drop_database("live").unwrap();
    }

    #[test]
    fn releasing_a_lease_consumes_its_only_permit() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        registry.create("live").unwrap();
        let lease = registry.acquire("live").unwrap();

        registry.release(lease).unwrap();
        // `release` takes ownership, so a second release of this lease cannot compile.
        registry.drop_database("live").unwrap();
    }

    #[test]
    fn core_parts_keep_the_root_lock_and_busy_state_after_registry_drop() {
        let root = FakeRoot::default();
        let same_root = root.clone();
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        let name = registry.create("live").unwrap();
        let lease = registry.acquire("live").unwrap();
        let (_handle, lifetime) = lease.into_core_parts();

        assert_eq!(
            registry.drop_database("live"),
            Err(RegistryError::DatabaseBusy(name))
        );
        drop(registry);
        assert!(matches!(
            DatabaseRegistry::open_or_create(same_root.clone()),
            Err(RegistryError::RegistryAlreadyOpen)
        ));

        drop(lifetime);
        assert!(DatabaseRegistry::open_or_create(same_root).is_ok());
    }

    #[test]
    fn lease_is_bound_to_its_registry_instance_and_blocks_drop() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        let created = registry.create("orders").unwrap();
        let lease = registry.acquire("ORDERS").unwrap();
        assert_eq!(lease.name(), &created);
        assert_eq!(
            registry.drop_database("orders"),
            Err(RegistryError::DatabaseBusy(created))
        );

        let key = OpaqueFileKey::new("db_0000000000000000000000000000000a".to_owned()).unwrap();
        let marker = MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1);
        let name = DatabaseName::parse("orders").unwrap();
        let mut other_root = FakeRoot {
            manifest: Some(RootManifest::lower_case_table_names_1()),
            ..FakeRoot::default()
        };
        other_root.registry.entries.insert(
            name,
            RegistryEntry {
                file_key: key.clone(),
                state: DatabaseState::Ready,
            },
        );
        other_root.files.insert(
            key.clone(),
            FakeFile {
                marker,
                identity: key,
            },
        );
        let mut other_registry = DatabaseRegistry::open_or_create(other_root).unwrap();
        assert_eq!(
            other_registry.release(lease),
            Err(RegistryError::InvalidRegistryState)
        );
        registry.drop_database("orders").unwrap();
    }

    #[test]
    fn snapshot_rejects_noncanonical_names_and_file_keys() {
        let key = OpaqueFileKey::new("db_0000000000000000000000000000000b".to_owned()).unwrap();
        assert_eq!(key.as_str(), "db_0000000000000000000000000000000b");
        let mut root = FakeRoot {
            manifest: Some(RootManifest::lower_case_table_names_1()),
            ..FakeRoot::default()
        };
        root.registry.entries.insert(
            DatabaseName("App".to_owned()),
            RegistryEntry {
                file_key: key,
                state: DatabaseState::Ready,
            },
        );
        assert!(matches!(
            DatabaseRegistry::open_or_create(root),
            Err(RegistryError::NonCanonicalDatabaseName)
        ));
        assert!(OpaqueFileKey::new("db_0000000000000000000000000000000A".to_owned()).is_err());
    }

    #[test]
    fn duplicate_file_key_snapshot_is_rejected_before_recovery_changes_anything() {
        let key = OpaqueFileKey::new("db_0000000000000000000000000000000e".to_owned()).unwrap();
        let marker = MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1);
        let mut root = FakeRoot {
            manifest: Some(RootManifest::lower_case_table_names_1()),
            ..FakeRoot::default()
        };
        for name in ["left", "right"] {
            root.registry.entries.insert(
                DatabaseName::parse(name).unwrap(),
                RegistryEntry {
                    file_key: key.clone(),
                    state: DatabaseState::Ready,
                },
            );
        }
        root.files.insert(
            key.clone(),
            FakeFile {
                marker,
                identity: key,
            },
        );
        let expected_entries = root.registry.clone();
        let expected_files = root.files.clone();
        let mutations = Rc::clone(&root.mutations);
        assert!(matches!(
            DatabaseRegistry::open_or_create(root),
            Err(RegistryError::DuplicateOpaqueFileKey)
        ));
        assert_eq!(mutations.get(), 0);
        assert_eq!(expected_entries.entries.len(), 2);
        assert_eq!(expected_files.len(), 1);
    }

    #[test]
    fn allocator_collision_leaves_existing_database_and_registry_unchanged() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        registry.create("first").unwrap();
        let expected_snapshot = registry.snapshot.clone();
        let expected_files = registry.root.files.clone();
        registry.root.next_key = 1;
        assert_eq!(
            registry.create("second"),
            Err(RegistryError::DuplicateOpaqueFileKey)
        );
        assert_eq!(registry.snapshot, expected_snapshot);
        assert_eq!(registry.root.files, expected_files);
    }

    #[test]
    fn create_preflight_collision_preserves_physical_file_and_registry() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        let key = OpaqueFileKey::new("db_00000000000000000000000000000001".to_owned()).unwrap();
        let marker = MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1);
        registry.root.files.insert(
            key.clone(),
            FakeFile {
                marker,
                identity: key,
            },
        );
        let expected_files = registry.root.files.clone();
        assert_eq!(
            registry.create("new_database"),
            Err(RegistryError::DuplicateOpaqueFileKey)
        );
        assert_eq!(registry.root.files, expected_files);
        assert!(registry.create("another").is_ok());
    }

    #[test]
    fn backend_write_failures_poison_every_public_operation() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        registry.create("ready").unwrap();
        let lease = registry.acquire("ready").unwrap();
        registry.root.fail_replace = true;
        assert_eq!(
            registry.create("fails_before_file_write"),
            Err(RegistryError::Backend)
        );
        assert_eq!(registry.create("new"), Err(RegistryError::RegistryPoisoned));
        assert!(matches!(
            registry.acquire("ready"),
            Err(RegistryError::RegistryPoisoned)
        ));
        assert_eq!(
            registry.drop_database("ready"),
            Err(RegistryError::RegistryPoisoned)
        );
        assert_eq!(
            registry.contains("ready"),
            Err(RegistryError::RegistryPoisoned)
        );
        assert_eq!(
            registry.ready_databases(),
            Err(RegistryError::RegistryPoisoned)
        );
        assert_eq!(
            registry.release(lease),
            Err(RegistryError::RegistryPoisoned)
        );
    }

    #[test]
    fn create_file_write_failure_poison_registry_after_creating_is_durable() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        registry.root.fail_create = true;
        assert_eq!(
            registry.create("fails_after_creating_write"),
            Err(RegistryError::Backend)
        );
        assert!(matches!(
            registry.acquire("fails_after_creating_write"),
            Err(RegistryError::RegistryPoisoned)
        ));
    }

    #[test]
    fn valid_mysql_file_swapped_between_keys_is_not_matching() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        let left = registry.create("left").unwrap();
        let right = registry.create("right").unwrap();
        let left_key = registry.snapshot.entries[&left].file_key.clone();
        let right_key = registry.snapshot.entries[&right].file_key.clone();
        let left_file = registry.root.files.remove(&left_key).unwrap();
        let right_file = registry.root.files.remove(&right_key).unwrap();
        registry.root.files.insert(left_key, right_file);
        registry.root.files.insert(right_key, left_file);

        assert!(matches!(
            registry.acquire("left"),
            Err(RegistryError::DatabaseMarkerMismatch(actual)) if actual == left
        ));
    }

    #[test]
    fn recovery_unlinks_missing_or_partial_files_idempotently() {
        let creating_name = DatabaseName::parse("creating").unwrap();
        let creating_key =
            OpaqueFileKey::new("db_0000000000000000000000000000000c".to_owned()).unwrap();
        let mut root = FakeRoot {
            manifest: Some(RootManifest::lower_case_table_names_1()),
            ..FakeRoot::default()
        };
        root.registry.entries.insert(
            creating_name,
            RegistryEntry {
                file_key: creating_key,
                state: DatabaseState::Creating,
            },
        );
        let registry = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(registry.ready_databases().unwrap().is_empty());
        let DatabaseRegistry { root, _lock, .. } = registry;
        drop(_lock);
        let registry = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(registry.ready_databases().unwrap().is_empty());
    }

    #[test]
    fn recovery_refuses_to_unlink_a_marker_mismatch() {
        let mut registry = DatabaseRegistry::open_or_create(FakeRoot::default()).unwrap();
        let name = registry.create("partial").unwrap();
        let key = registry.snapshot.entries[&name].file_key.clone();
        registry.snapshot.entries.get_mut(&name).unwrap().state = DatabaseState::Creating;
        registry.root.files.insert(
            key.clone(),
            FakeFile {
                marker: MySqlOwnerMarkerV2 {
                    version: 1,
                    owner: FrontendOwner::MySql,
                    lower_case_table_names: 1,
                    reserved_bits: 0,
                },
                identity: key.clone(),
            },
        );

        assert_eq!(
            registry.recover_incomplete_operations(),
            Err(RegistryError::DatabaseMarkerMismatch(name.clone()))
        );
        assert!(registry.root.files.contains_key(&key));
        assert_eq!(
            registry.snapshot.entries[&name].state,
            DatabaseState::Creating
        );
    }

    #[test]
    fn marker_mismatch_never_acquires_a_ready_database() {
        let name = DatabaseName::parse("safe").unwrap();
        let key = OpaqueFileKey::new("db_0000000000000000000000000000000d".to_owned()).unwrap();
        let mut root = FakeRoot {
            manifest: Some(RootManifest::lower_case_table_names_1()),
            ..FakeRoot::default()
        };
        root.registry.entries.insert(
            name.clone(),
            RegistryEntry {
                file_key: key.clone(),
                state: DatabaseState::Ready,
            },
        );
        root.files.insert(
            key.clone(),
            FakeFile {
                marker: MySqlOwnerMarkerV2 {
                    version: 1,
                    owner: FrontendOwner::MySql,
                    lower_case_table_names: 1,
                    reserved_bits: 0,
                },
                identity: key,
            },
        );
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(matches!(
            registry.acquire("safe"),
            Err(RegistryError::DatabaseMarkerMismatch(actual)) if actual == name
        ));
    }
}
