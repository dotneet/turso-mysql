//! Durable, dialect-neutral allocation of monotonically increasing row-id ranges.
//!
//! The allocator owns a small append-only sidecar file. A range becomes visible
//! only after its record has been written, and by default synced. Callers
//! therefore must not wrap this operation in a user transaction: a rolled-back
//! statement intentionally burns its range.
//!
//! An MVCC database can instead write the marks into its own commits (see
//! [`DurableRangeAllocator::let_commits_log_marks`]). A record is then only
//! written, which a crash of the process does not lose, and a commit writes
//! every mark written since into its logical-log frame, so the mark of a
//! number a committed row holds is durable exactly when that row is. A
//! checkpoint syncs the sidecar before it drops those frames from the log.
//!
//! A record that fails its check ends the log: everything from it on is an
//! append that was never synced and that a power loss left torn, and the next
//! write truncates it away. A checked record whose mark does not rise is
//! corruption and stops allocation.

use std::{
    collections::{BTreeMap, HashMap},
    mem,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex, Weak,
    },
};

use crate::{
    io::{File, FileId, FileSyncType, OpenFlags, IO},
    storage::lock_release::LockReleaseSignal,
    types::{IOCompletions, IOResultOr},
    Buffer, Completion, CompletionError, IOResult, LimboError, Result,
};

const HEADER_MAGIC: [u8; 8] = *b"TURSOAI1";
const HEADER_VERSION: u16 = 1;
const HEADER_LEN: usize = 32;
const RECORD_MAGIC: [u8; 4] = *b"TAI1";
const RECORD_VERSION: u16 = 1;
const RECORD_LEN: usize = 36;
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

static OPEN_ALLOCATORS: LazyLock<Mutex<HashMap<AllocatorFileIdentity, Weak<AllocatorShared>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// A pending backend operation can retain only a Completion, not the File that
// owns its OS lock. Keep a poisoned sidecar alive for the rest of the process.
static POISONED_ALLOCATORS: LazyLock<Mutex<Vec<Arc<AllocatorShared>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

#[derive(Hash, Eq, PartialEq)]
struct AllocatorFileIdentity {
    file_id: FileId,
    io_address: Option<usize>,
}

/// Stable identity of the database that owns one allocator sidecar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatorDatabaseIdentity([u8; 16]);

impl AllocatorDatabaseIdentity {
    pub fn new(bytes: [u8; 16]) -> Result<Self> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(LimboError::InvalidArgument(
                "auto-increment database identity must not be all zero".to_owned(),
            ));
        }
        Ok(Self(bytes))
    }
}

/// Controls whether an empty sidecar may be initialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocatorOpenMode {
    Create,
    Reopen,
}

/// Stable identity for one auto-increment counter.
///
/// A frontend must derive this from durable table identity, not from a display
/// name that can be renamed or case-folded.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AutoIncrementKey([u8; 16]);

impl AutoIncrementKey {
    pub fn new(bytes: [u8; 16]) -> Result<Self> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(LimboError::InvalidArgument(
                "auto-increment allocator key must not be all zero".to_owned(),
            ));
        }
        Ok(Self(bytes))
    }

    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// A range that was made durable by [`DurableRangeAllocator`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservedRange {
    first: u64,
    last: u64,
}

impl ReservedRange {
    pub const fn first(self) -> u64 {
        self.first
    }

    pub const fn last(self) -> u64 {
        self.last
    }
}

/// One value written to an AUTO_INCREMENT column in a VALUES row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertAutoIncrementValue {
    Generated,
    Explicit(u64),
}

/// The counter state durably reserved for one ordered INSERT batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservedInsertValues {
    pub high_water_before: u64,
    pub high_water_after: u64,
}

/// Opens and reserves from a durable append-only high-water log.
///
/// Calls to [`Self::open`] for one sidecar share an in-process gate. The
/// sidecar is opened without the normal database-wide file lock because each
/// reservation takes and releases its own exclusive lock while it reads,
/// appends, and syncs one record.
#[derive(Clone)]
pub struct DurableRangeAllocator {
    shared: Arc<AllocatorShared>,
}

struct AllocatorShared {
    file: Arc<dyn File>,
    sync_type: FileSyncType,
    database_identity: AllocatorDatabaseIdentity,
    open_mode: AllocatorOpenMode,
    operation_in_progress: AtomicBool,
    operations_finished: LockReleaseSignal,
    poisoned: AtomicBool,
    /// What the log held as this process last read or wrote it, so that an
    /// operation reads only the records appended since. Without it every
    /// reservation read and checked every record ever written, which made
    /// each insert into a counted table slower than the last.
    scanned: Mutex<Option<ScannedLog>>,
    commits: Mutex<Weak<CommitLoggedMarks>>,
}

/// The high-water marks of every key in the log's first `complete_len` bytes
/// past the header, all of them checked.
struct ScannedLog {
    complete_len: u64,
    high_waters: BTreeMap<AutoIncrementKey, u64>,
}

impl DurableRangeAllocator {
    pub fn open(
        io: &dyn IO,
        path: &str,
        database_identity: AllocatorDatabaseIdentity,
        open_mode: AllocatorOpenMode,
        sync_type: FileSyncType,
    ) -> Result<Self> {
        let flags = match open_mode {
            AllocatorOpenMode::Create => OpenFlags::Create | OpenFlags::NoLock,
            AllocatorOpenMode::Reopen => OpenFlags::NoLock,
        };
        let file = io.open_file(path, flags, false)?;
        let file_id = file.file_id()?;
        let identity = AllocatorFileIdentity {
            file_id,
            // In-memory and simulator backends use synthetic identities based
            // only on a path. Do not accidentally share their separate stores.
            io_address: (file_id.dev == 0).then_some(io as *const dyn IO as *const () as usize),
        };
        let mut open_allocators = OPEN_ALLOCATORS.lock().map_err(|_| {
            LimboError::InternalError("auto-increment allocator registry is poisoned".to_owned())
        })?;
        open_allocators.retain(|_, allocator| allocator.strong_count() != 0);
        if let Some(shared) = open_allocators.get(&identity).and_then(Weak::upgrade) {
            if shared.sync_type != sync_type
                || shared.database_identity != database_identity
                || shared.open_mode != open_mode
            {
                return Err(LimboError::InvalidArgument(
                    "auto-increment allocator was opened with incompatible settings".to_owned(),
                ));
            }
            return Ok(Self { shared });
        }

        let shared = Arc::new(AllocatorShared {
            file,
            sync_type,
            database_identity,
            open_mode,
            operation_in_progress: AtomicBool::new(false),
            operations_finished: LockReleaseSignal::default(),
            poisoned: AtomicBool::new(false),
            scanned: Mutex::new(None),
            commits: Mutex::new(Weak::new()),
        });
        open_allocators.insert(identity, Arc::downgrade(&shared));
        Ok(Self { shared })
    }

    /// Builds an allocator from a retained sidecar descriptor.
    ///
    /// A capability-owning frontend uses this after it has opened and checked
    /// the sidecar through its own root handle. This function never resolves a
    /// path or creates a replacement sidecar. Filesystem-backed descriptors
    /// share the same in-process gate as [`Self::open`].
    pub fn from_file(
        file: Arc<dyn File>,
        database_identity: AllocatorDatabaseIdentity,
        open_mode: AllocatorOpenMode,
        sync_type: FileSyncType,
    ) -> Result<Self> {
        let file_id = file.file_id()?;
        // Synthetic file identities do not identify the backing IO instance.
        // Do not merge two independently retained non-filesystem handles.
        if file_id.dev == 0 {
            return Ok(Self::from_retained_file(
                file,
                database_identity,
                open_mode,
                sync_type,
            ));
        }

        let identity = AllocatorFileIdentity {
            file_id,
            io_address: None,
        };
        let mut open_allocators = OPEN_ALLOCATORS.lock().map_err(|_| {
            LimboError::InternalError("auto-increment allocator registry is poisoned".to_owned())
        })?;
        open_allocators.retain(|_, allocator| allocator.strong_count() != 0);
        if let Some(shared) = open_allocators.get(&identity).and_then(Weak::upgrade) {
            if shared.sync_type != sync_type
                || shared.database_identity != database_identity
                || shared.open_mode != open_mode
            {
                return Err(LimboError::InvalidArgument(
                    "auto-increment allocator was opened with incompatible settings".to_owned(),
                ));
            }
            return Ok(Self { shared });
        }

        let allocator = Self::from_retained_file(file, database_identity, open_mode, sync_type);
        open_allocators.insert(identity, Arc::downgrade(&allocator.shared));
        Ok(allocator)
    }

    /// Wraps one already-open Unix sidecar descriptor without resolving its path.
    ///
    /// The caller supplies a diagnostic identity only; this constructor never
    /// interprets it as a path or creates a file.
    pub fn from_std_file(
        file: std::fs::File,
        debug_identity: String,
        database_identity: AllocatorDatabaseIdentity,
        open_mode: AllocatorOpenMode,
        sync_type: FileSyncType,
    ) -> Result<Self> {
        let file = crate::io::file_from_std(file, debug_identity, OpenFlags::NoLock)?;
        Self::from_file(file, database_identity, open_mode, sync_type)
    }

    fn from_retained_file(
        file: Arc<dyn File>,
        database_identity: AllocatorDatabaseIdentity,
        open_mode: AllocatorOpenMode,
        sync_type: FileSyncType,
    ) -> Self {
        Self {
            shared: Arc::new(AllocatorShared {
                file,
                sync_type,
                database_identity,
                open_mode,
                operation_in_progress: AtomicBool::new(false),
                operations_finished: LockReleaseSignal::default(),
                poisoned: AtomicBool::new(false),
                scanned: Mutex::new(None),
                commits: Mutex::new(Weak::new()),
            }),
        }
    }

    /// Starts durable sidecar initialization without reserving an ID range.
    ///
    /// A registry must complete this operation before publishing a newly
    /// created sidecar. Repeating it after a completed initialization only
    /// validates the immutable header.
    pub fn initialize(&self) -> Result<AllocatorSidecarOperation> {
        if self.shared.open_mode != AllocatorOpenMode::Create {
            return Err(LimboError::InvalidArgument(
                "only a newly created auto-increment sidecar may be initialized".to_owned(),
            ));
        }
        self.begin_sidecar_operation(AllocatorSidecarOperationKind::Initialize)
    }

    /// Starts immutable-header verification for a retained sidecar.
    ///
    /// Reopen callers use this before accepting the descriptor. It never
    /// writes, including when the sidecar is empty.
    pub fn verify(&self) -> Result<AllocatorSidecarOperation> {
        self.begin_sidecar_operation(AllocatorSidecarOperationKind::Verify)
    }

    fn begin_sidecar_operation(
        &self,
        kind: AllocatorSidecarOperationKind,
    ) -> Result<AllocatorSidecarOperation> {
        self.ensure_usable()?;
        Ok(AllocatorSidecarOperation {
            shared: self.shared.clone(),
            kind,
            state: AllocatorSidecarOperationState::Start,
            holds_lock: false,
        })
    }
    /// Begins a reservation. Repeatedly call [`RangeReservation::step`] until
    /// it returns [`IOResult::Done`].
    pub fn reserve(&self, key: AutoIncrementKey, count: u64) -> Result<RangeReservation> {
        self.ensure_usable()?;
        if count == 0 {
            return Err(LimboError::InvalidArgument(
                "auto-increment reservation count must be greater than zero".to_owned(),
            ));
        }

        Ok(RangeReservation {
            shared: self.shared.clone(),
            key,
            kind: ReservationKind::Reserve { count },
            state: ReservationState::Start,
            holds_lock: false,
            retain_lock: false,
        })
    }

    /// Reserves one VALUES batch while applying explicit IDs in row order.
    ///
    /// At the first generated row MySQL reserves as many IDs as the whole
    /// VALUES batch contains. Unused slots remain spent after the statement.
    pub fn reserve_insert_values(
        &self,
        key: AutoIncrementKey,
        values: Vec<InsertAutoIncrementValue>,
    ) -> Result<InsertValuesReservation> {
        self.ensure_usable()?;
        if values.is_empty() || !values.contains(&InsertAutoIncrementValue::Generated) {
            return Err(LimboError::InvalidArgument(
                "mixed auto-increment reservation needs a generated row".to_owned(),
            ));
        }
        Ok(InsertValuesReservation(RangeReservation {
            shared: self.shared.clone(),
            key,
            kind: ReservationKind::InsertValues { values },
            state: ReservationState::Start,
            holds_lock: false,
            retain_lock: false,
        }))
    }

    /// Begins a read of one key's current high-water mark.
    ///
    /// Repeatedly call [`HighWaterQuery::step`] until it returns
    /// [`IOResult::Done`]. A key that has never been used reads as zero.
    pub fn peek_high_water(&self, key: AutoIncrementKey) -> Result<HighWaterQuery> {
        self.ensure_usable()?;
        Ok(HighWaterQuery(RangeReservation {
            shared: self.shared.clone(),
            key,
            kind: ReservationKind::Peek,
            state: ReservationState::Start,
            holds_lock: false,
            retain_lock: false,
        }))
    }

    /// Holds the sidecar lock while a statement applies IDs in row order.
    pub fn lease_high_water(&self, key: AutoIncrementKey) -> Result<HighWaterLease> {
        self.ensure_usable()?;
        Ok(HighWaterLease {
            reservation: RangeReservation {
                shared: self.shared.clone(),
                key,
                kind: ReservationKind::Peek,
                state: ReservationState::Start,
                holds_lock: false,
                retain_lock: true,
            },
            high_water: None,
            pending_target: None,
        })
    }

    /// Durably raises one key's high-water mark to at least `high_water`.
    ///
    /// Repeatedly call [`RangeReservation::step`] until it returns
    /// [`IOResult::Done`]. The returned range is empty when the existing mark
    /// was already high enough.
    pub fn advance_past(&self, key: AutoIncrementKey, high_water: u64) -> Result<RangeReservation> {
        self.ensure_usable()?;
        if high_water == 0 {
            return Err(LimboError::InvalidArgument(
                "auto-increment high-water mark must be greater than zero".to_owned(),
            ));
        }

        Ok(RangeReservation {
            shared: self.shared.clone(),
            key,
            kind: ReservationKind::AdvancePast { high_water },
            state: ReservationState::Start,
            holds_lock: false,
            retain_lock: false,
        })
    }

    pub fn let_commits_log_marks(&self) -> Result<Arc<CommitLoggedMarks>> {
        self.ensure_usable()?;
        let mut commits = self
            .shared
            .commits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(logged_marks) = commits.upgrade() {
            return Ok(logged_marks);
        }
        let logged_marks = Arc::new(CommitLoggedMarks {
            file: self.shared.file.clone(),
            sync_type: self.shared.sync_type,
            not_yet_logged: Mutex::new(BTreeMap::new()),
        });
        *commits = Arc::downgrade(&logged_marks);
        Ok(logged_marks)
    }

    pub fn last_seen_high_water(&self, key: AutoIncrementKey) -> Result<Option<u64>> {
        self.ensure_usable()?;
        if self.shared.operation_in_progress.load(Ordering::Acquire) {
            return Err(LimboError::Busy);
        }
        let scanned = self
            .shared
            .scanned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(scanned
            .as_ref()
            .map(|log| log.high_waters.get(&key).copied().unwrap_or(0)))
    }

    pub fn operations_finished(&self) -> u64 {
        self.shared.operations_finished.releases()
    }

    pub fn wait_for_an_operation_to_finish(
        &self,
        seen: u64,
        timeout: std::time::Duration,
    ) -> Option<bool> {
        self.shared
            .operations_finished
            .wait_for_release_after(seen, timeout)
    }

    fn ensure_usable(&self) -> Result<()> {
        if self.shared.poisoned.load(Ordering::Acquire) {
            return Err(LimboError::InternalError(
                "auto-increment allocator was dropped with I/O still pending".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum AllocatorSidecarOperationKind {
    Initialize,
    Verify,
}

/// One re-entrant initialize or verify operation for an allocator sidecar.
///
/// Initialization writes and syncs only the immutable identity header. It does
/// not reserve a counter value, so publishing a fresh logical database cannot
/// consume an ID before an `AUTO_INCREMENT` table exists.
pub struct AllocatorSidecarOperation {
    shared: Arc<AllocatorShared>,
    kind: AllocatorSidecarOperationKind,
    state: AllocatorSidecarOperationState,
    holds_lock: bool,
}

enum AllocatorSidecarOperationState {
    Start,
    ReadingHeader {
        completion: Completion,
        buffer: Arc<Buffer>,
    },
    WritingHeader {
        completion: Completion,
        buffer: Arc<Buffer>,
        short_write: Arc<AtomicBool>,
    },
    SyncingHeader {
        completion: Completion,
    },
    Finished,
}

impl AllocatorSidecarOperation {
    /// Advances this operation once. Call again after each returned I/O completion.
    pub fn step(&mut self) -> IOResultOr<()> {
        let state = mem::replace(&mut self.state, AllocatorSidecarOperationState::Finished);
        match state {
            AllocatorSidecarOperationState::Start => self.start(),
            AllocatorSidecarOperationState::ReadingHeader { completion, buffer } => {
                self.finish_read_header(completion, buffer)
            }
            AllocatorSidecarOperationState::WritingHeader {
                completion,
                buffer,
                short_write,
            } => self.finish_write_header(completion, buffer, short_write),
            AllocatorSidecarOperationState::SyncingHeader { completion } => {
                self.finish_sync_header(completion)
            }
            AllocatorSidecarOperationState::Finished => self.fail(LimboError::InternalError(
                "auto-increment sidecar operation was stepped after completion".to_owned(),
            )),
        }
    }

    fn start(&mut self) -> IOResultOr<()> {
        if self
            .shared
            .operation_in_progress
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return self.fail(LimboError::Busy);
        }
        if let Err(error) = self.shared.file.lock_file(true) {
            let_the_next_operation_start(&self.shared);
            return self.fail(error);
        }
        self.holds_lock = true;
        let file_size = match self.shared.file.size() {
            Ok(size) => size,
            Err(error) => return self.fail(error),
        };
        if file_size > MAX_LOG_BYTES {
            return self.fail(LimboError::TooBig);
        }
        if file_size == 0 {
            return match self.kind {
                AllocatorSidecarOperationKind::Initialize => self.begin_write_header(),
                AllocatorSidecarOperationKind::Verify => self.fail(LimboError::Corrupt(
                    "auto-increment sidecar is missing its durable header".to_owned(),
                )),
            };
        }
        if file_size < HEADER_LEN as u64 {
            return self.fail(LimboError::Corrupt(
                "auto-increment sidecar has a torn header".to_owned(),
            ));
        }
        let buffer = Arc::new(Buffer::new_temporary(HEADER_LEN));
        let completion = Completion::new_read(buffer.clone(), move |result| {
            let Ok((_, bytes_read)) = result else {
                return None;
            };
            if bytes_read != HEADER_LEN as i32 {
                return Some(CompletionError::ShortRead {
                    page_idx: 0,
                    expected: HEADER_LEN,
                    actual: bytes_read.max(0) as usize,
                });
            }
            None
        });
        let completion = match self.shared.file.pread(0, completion) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = AllocatorSidecarOperationState::ReadingHeader {
            completion: completion.clone(),
            buffer,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_read_header(
        &mut self,
        completion: Completion,
        buffer: Arc<Buffer>,
    ) -> IOResultOr<()> {
        if !completion.finished() {
            self.state = AllocatorSidecarOperationState::ReadingHeader {
                completion: completion.clone(),
                buffer,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        if let Err(error) = decode_header(buffer.as_slice(), self.shared.database_identity) {
            return self.fail(error);
        }
        match self.kind {
            // A prior header write can have reached the device before its
            // sync reported failure. Retrying initialization must sync that
            // already-valid header before the registry may publish it.
            AllocatorSidecarOperationKind::Initialize => self.begin_sync_header(),
            AllocatorSidecarOperationKind::Verify => self.finish(),
        }
    }

    fn begin_write_header(&mut self) -> IOResultOr<()> {
        let buffer = Arc::new(Buffer::new(
            encode_header(self.shared.database_identity).to_vec(),
        ));
        let short_write = Arc::new(AtomicBool::new(false));
        let expected = buffer.len() as i32;
        let short_write_for_callback = short_write.clone();
        let completion = Completion::new_write(move |result| {
            if let Ok(bytes_written) = result {
                if bytes_written != expected {
                    short_write_for_callback.store(true, Ordering::Release);
                }
            }
        });
        let completion = match self.shared.file.pwrite(0, buffer.clone(), completion) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = AllocatorSidecarOperationState::WritingHeader {
            completion: completion.clone(),
            buffer,
            short_write,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_write_header(
        &mut self,
        completion: Completion,
        buffer: Arc<Buffer>,
        short_write: Arc<AtomicBool>,
    ) -> IOResultOr<()> {
        if !completion.finished() {
            self.state = AllocatorSidecarOperationState::WritingHeader {
                completion: completion.clone(),
                buffer,
                short_write,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        if short_write.load(Ordering::Acquire) {
            return self.fail(CompletionError::ShortWrite.into());
        }
        self.begin_sync_header()
    }

    fn begin_sync_header(&mut self) -> IOResultOr<()> {
        let completion = Completion::new_sync(|_| {});
        let completion = match self.shared.file.sync(completion, self.shared.sync_type) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = AllocatorSidecarOperationState::SyncingHeader {
            completion: completion.clone(),
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_sync_header(&mut self, completion: Completion) -> IOResultOr<()> {
        if !completion.finished() {
            self.state = AllocatorSidecarOperationState::SyncingHeader {
                completion: completion.clone(),
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        self.finish()
    }

    fn finish(&mut self) -> IOResultOr<()> {
        if let Err(error) = self.release_lock() {
            self.state = AllocatorSidecarOperationState::Finished;
            return Err(error.into());
        }
        self.state = AllocatorSidecarOperationState::Finished;
        Ok(IOResult::Done(()))
    }

    fn fail(&mut self, error: LimboError) -> IOResultOr<()> {
        self.state = AllocatorSidecarOperationState::Finished;
        if let Err(unlock_error) = self.release_lock() {
            tracing::error!(%unlock_error, "failed to unlock auto-increment sidecar operation after failure");
        }
        Err(error.into())
    }

    fn release_lock(&mut self) -> Result<()> {
        if !self.holds_lock {
            return Ok(());
        }
        if let Err(error) = self.shared.file.unlock_file() {
            self.holds_lock = false;
            poison_allocator(&self.shared);
            return Err(error);
        }
        self.holds_lock = false;
        let_the_next_operation_start(&self.shared);
        Ok(())
    }
}

impl Drop for AllocatorSidecarOperation {
    fn drop(&mut self) {
        if self.is_unfinished() {
            poison_allocator(&self.shared);
            return;
        }
        if let Err(error) = self.release_lock() {
            tracing::error!(%error, "failed to unlock dropped auto-increment sidecar operation");
        }
    }
}

impl AllocatorSidecarOperation {
    fn is_unfinished(&self) -> bool {
        !matches!(
            self.state,
            AllocatorSidecarOperationState::Start | AllocatorSidecarOperationState::Finished
        )
    }
}

/// One re-entrant range reservation operation.
///
/// A reservation holds the sidecar lock from before its read until after fsync.
/// Dropping with pending I/O poisons the allocator and retains the lock, because
/// a late completion could otherwise race a later reservation.
pub struct RangeReservation {
    shared: Arc<AllocatorShared>,
    key: AutoIncrementKey,
    kind: ReservationKind,
    state: ReservationState,
    holds_lock: bool,
    retain_lock: bool,
}

/// One re-entrant read of an allocator key's high-water mark.
pub struct HighWaterQuery(RangeReservation);

pub struct InsertValuesReservation(RangeReservation);

pub struct HighWaterLease {
    reservation: RangeReservation,
    high_water: Option<u64>,
    pending_target: Option<u64>,
}

impl HighWaterLease {
    pub fn read(&mut self) -> IOResultOr<u64> {
        if self.high_water.is_some() {
            return Err(LimboError::InternalError(
                "auto-increment lease has already been read".to_owned(),
            )
            .into());
        }
        Ok(match self.reservation.step()? {
            IOResult::Done(range) => {
                self.high_water = Some(range.last());
                IOResult::Done(range.last())
            }
            IOResult::IO(completions) => IOResult::IO(completions),
        })
    }

    pub fn advance_past(&mut self, target: u64) -> IOResultOr<u64> {
        let current = self.high_water.ok_or_else(|| {
            LimboError::InternalError("auto-increment lease was not read".to_owned())
        })?;
        if let Some(pending) = self.pending_target {
            if pending != target {
                return Err(LimboError::InternalError(
                    "auto-increment lease target changed during I/O".to_owned(),
                )
                .into());
            }
        } else if target <= current {
            return Ok(IOResult::Done(current));
        } else {
            self.pending_target = Some(target);
        }
        let result = if self.reservation.is_leased() {
            self.reservation.begin_locked_advance(target, current)?
        } else {
            self.reservation.step()?
        };
        Ok(match result {
            IOResult::Done(range) => {
                self.high_water = Some(range.last());
                self.pending_target = None;
                IOResult::Done(range.last())
            }
            IOResult::IO(completions) => IOResult::IO(completions),
        })
    }

    pub fn release(mut self) -> Result<()> {
        if self.pending_target.is_some() || !self.reservation.is_leased() {
            return Err(LimboError::InternalError(
                "auto-increment lease has pending I/O".to_owned(),
            ));
        }
        self.reservation.release_lock()
    }
}

impl InsertValuesReservation {
    pub fn step(&mut self) -> IOResultOr<ReservedInsertValues> {
        Ok(match self.0.step()? {
            IOResult::Done(range) => IOResult::Done(ReservedInsertValues {
                high_water_before: range.first - 1,
                high_water_after: range.last,
            }),
            IOResult::IO(completions) => IOResult::IO(completions),
        })
    }
}

impl HighWaterQuery {
    /// Advances this read once. Call again after each returned I/O completion.
    pub fn step(&mut self) -> IOResultOr<u64> {
        Ok(match self.0.step()? {
            IOResult::Done(range) => IOResult::Done(range.last()),
            IOResult::IO(completions) => IOResult::IO(completions),
        })
    }
}

enum ReservationKind {
    Reserve {
        count: u64,
    },
    AdvancePast {
        high_water: u64,
    },
    InsertValues {
        values: Vec<InsertAutoIncrementValue>,
    },
    /// Reads one key's mark without moving it or writing anything.
    Peek,
}

enum ReservationState {
    Start,
    ReadingHeader {
        completion: Completion,
        buffer: Arc<Buffer>,
        file_size: u64,
    },
    WritingHeader {
        completion: Completion,
        buffer: Arc<Buffer>,
        short_write: Arc<AtomicBool>,
    },
    SyncingHeader {
        completion: Completion,
    },
    Reading {
        completion: Completion,
        buffer: Arc<Buffer>,
        append_offset: u64,
        /// Where past the header the read began: 0 for the whole log, or
        /// where the records this process already checked end.
        scanned_from: u64,
    },
    Writing {
        completion: Completion,
        buffer: Arc<Buffer>,
        range: ReservedRange,
        short_write: Arc<AtomicBool>,
        append_offset: u64,
    },
    Syncing {
        completion: Completion,
        range: ReservedRange,
        append_offset: u64,
    },
    TruncatingTornTail {
        completion: Completion,
        log_end: u64,
        high_water: u64,
    },
    SyncingTruncatedTail {
        completion: Completion,
        log_end: u64,
        high_water: u64,
    },
    SyncingExisting {
        completion: Completion,
        high_water: u64,
    },
    Leased,
    Finished,
}

impl RangeReservation {
    pub fn step(&mut self) -> IOResultOr<ReservedRange> {
        let state = mem::replace(&mut self.state, ReservationState::Finished);
        match state {
            ReservationState::Start => self.start(),
            ReservationState::ReadingHeader {
                completion,
                buffer,
                file_size,
            } => self.finish_read_header(completion, buffer, file_size),
            ReservationState::WritingHeader {
                completion,
                buffer,
                short_write,
            } => self.finish_write_header(completion, buffer, short_write),
            ReservationState::SyncingHeader { completion } => self.finish_sync_header(completion),
            ReservationState::Reading {
                completion,
                buffer,
                append_offset,
                scanned_from,
            } => self.finish_read(completion, buffer, append_offset, scanned_from),
            ReservationState::Writing {
                completion,
                buffer,
                range,
                short_write,
                append_offset,
            } => self.finish_write(completion, buffer, range, short_write, append_offset),
            ReservationState::Syncing {
                completion,
                range,
                append_offset,
            } => self.finish_sync(completion, range, append_offset),
            ReservationState::TruncatingTornTail {
                completion,
                log_end,
                high_water,
            } => self.finish_truncate_torn_tail(completion, log_end, high_water),
            ReservationState::SyncingTruncatedTail {
                completion,
                log_end,
                high_water,
            } => self.finish_sync_truncated_tail(completion, log_end, high_water),
            ReservationState::SyncingExisting {
                completion,
                high_water,
            } => self.finish_sync_existing(completion, high_water),
            ReservationState::Leased | ReservationState::Finished => {
                self.fail(LimboError::InternalError(
                    "auto-increment reservation was stepped after completion".to_owned(),
                ))
            }
        }
    }

    fn start(&mut self) -> IOResultOr<ReservedRange> {
        if self
            .shared
            .operation_in_progress
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return self.fail(LimboError::Busy);
        }

        if let Err(error) = self.shared.file.lock_file(true) {
            let_the_next_operation_start(&self.shared);
            return self.fail(error);
        }
        self.holds_lock = true;

        let file_size = match self.shared.file.size() {
            Ok(size) => size,
            Err(error) => return self.fail(error),
        };
        if file_size > MAX_LOG_BYTES {
            return self.fail(LimboError::TooBig);
        }

        if file_size == 0 {
            return match (&self.kind, self.shared.open_mode) {
                // A read writes nothing, including the header a reservation
                // would create here. An empty sidecar has handed out nothing.
                (ReservationKind::Peek, AllocatorOpenMode::Create) => {
                    self.finish(ReservedRange { first: 0, last: 0 })
                }
                (_, AllocatorOpenMode::Create) => self.begin_write_header(),
                (_, AllocatorOpenMode::Reopen) => self.fail(LimboError::Corrupt(
                    "auto-increment sidecar is missing its durable header".to_owned(),
                )),
            };
        }
        if file_size < HEADER_LEN as u64 {
            return self.fail(LimboError::Corrupt(
                "auto-increment sidecar has a torn header".to_owned(),
            ));
        }

        let buffer = Arc::new(Buffer::new_temporary(HEADER_LEN));
        let completion = Completion::new_read(buffer.clone(), move |result| {
            let Ok((_, bytes_read)) = result else {
                return None;
            };
            if bytes_read != HEADER_LEN as i32 {
                return Some(CompletionError::ShortRead {
                    page_idx: 0,
                    expected: HEADER_LEN,
                    actual: bytes_read.max(0) as usize,
                });
            }
            None
        });
        let completion = match self.shared.file.pread(0, completion) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::ReadingHeader {
            completion: completion.clone(),
            buffer,
            file_size,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_read_header(
        &mut self,
        completion: Completion,
        buffer: Arc<Buffer>,
        file_size: u64,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::ReadingHeader {
                completion: completion.clone(),
                buffer,
                file_size,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        if let Err(error) = decode_header(buffer.as_slice(), self.shared.database_identity) {
            return self.fail(error);
        }
        self.begin_read_log(file_size)
    }

    /// Reads the records past those this process already checked: the log
    /// only grows, so a record once checked stays as it was, and a log
    /// shorter than the one checked is read again from the start. The header
    /// is read and checked every time, being one short read.
    fn begin_read_log(&mut self, file_size: u64) -> IOResultOr<ReservedRange> {
        let complete_len = complete_log_len(file_size);
        let scanned_len = self
            .scanned()
            .as_ref()
            .map(|scanned| scanned.complete_len)
            .filter(|scanned_len| *scanned_len <= complete_len);
        match scanned_len {
            Some(scanned_len) => self.begin_read_records(scanned_len, complete_len),
            None => {
                *self.scanned() = None;
                self.begin_read_records(0, complete_len)
            }
        }
    }

    /// Reads the records between `scanned_len` and `complete_len` bytes past
    /// the header, those already checked being in [`AllocatorShared::scanned`].
    fn begin_read_records(
        &mut self,
        scanned_len: u64,
        complete_len: u64,
    ) -> IOResultOr<ReservedRange> {
        let append_offset = HEADER_LEN as u64 + complete_len;
        if complete_len == scanned_len {
            return self.finish_scan(Vec::new(), append_offset, scanned_len);
        }
        let read_len = match usize::try_from(complete_len - scanned_len) {
            Ok(len) => len,
            Err(_) => return self.fail(LimboError::TooBig),
        };
        let buffer = Arc::new(Buffer::new_temporary(read_len));
        let expected = read_len;
        let completion = Completion::new_read(buffer.clone(), move |result| {
            let Ok((_, bytes_read)) = result else {
                return None;
            };
            if bytes_read != expected as i32 {
                return Some(CompletionError::ShortRead {
                    page_idx: 0,
                    expected,
                    actual: bytes_read.max(0) as usize,
                });
            }
            None
        });
        let completion = match self
            .shared
            .file
            .pread(HEADER_LEN as u64 + scanned_len, completion)
        {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::Reading {
            completion: completion.clone(),
            buffer,
            append_offset,
            scanned_from: scanned_len,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn begin_write_header(&mut self) -> IOResultOr<ReservedRange> {
        let buffer = Arc::new(Buffer::new(
            encode_header(self.shared.database_identity).to_vec(),
        ));
        let short_write = Arc::new(AtomicBool::new(false));
        let expected = buffer.len() as i32;
        let short_write_for_callback = short_write.clone();
        let completion = Completion::new_write(move |result| {
            if let Ok(bytes_written) = result {
                if bytes_written != expected {
                    short_write_for_callback.store(true, Ordering::Release);
                }
            }
        });
        let completion = match self.shared.file.pwrite(0, buffer.clone(), completion) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::WritingHeader {
            completion: completion.clone(),
            buffer,
            short_write,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_write_header(
        &mut self,
        completion: Completion,
        buffer: Arc<Buffer>,
        short_write: Arc<AtomicBool>,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::WritingHeader {
                completion: completion.clone(),
                buffer,
                short_write,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        if short_write.load(Ordering::Acquire) {
            return self.fail(CompletionError::ShortWrite.into());
        }
        let completion = Completion::new_sync(|_| {});
        let completion = match self.shared.file.sync(completion, self.shared.sync_type) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::SyncingHeader {
            completion: completion.clone(),
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_sync_header(&mut self, completion: Completion) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::SyncingHeader {
                completion: completion.clone(),
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        self.begin_write(HEADER_LEN as u64, 0)
    }

    fn finish_read(
        &mut self,
        completion: Completion,
        buffer: Arc<Buffer>,
        append_offset: u64,
        scanned_from: u64,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::Reading {
                completion: completion.clone(),
                buffer,
                append_offset,
                scanned_from,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        self.finish_scan(buffer.as_slice().to_vec(), append_offset, scanned_from)
    }

    /// Checks the records read past `scanned_from`, keeps every mark the log
    fn finish_scan(
        &mut self,
        records: Vec<u8>,
        append_offset: u64,
        scanned_from: u64,
    ) -> IOResultOr<ReservedRange> {
        let checked = match scanned_from {
            0 => Some(BTreeMap::new()),
            _ => self.scanned().take().map(|scanned| scanned.high_waters),
        };
        let Some(mut high_waters) = checked else {
            return self.fail(LimboError::InternalError(
                "auto-increment log was read past records nobody checked".to_owned(),
            ));
        };
        let checked_len = match scan_records(&records, &mut high_waters) {
            Ok(checked_len) => checked_len,
            Err(error) => return self.fail(error),
        };
        let high_water = high_waters.get(&self.key).copied().unwrap_or(0);
        let complete_len = scanned_from + checked_len as u64;
        *self.scanned() = Some(ScannedLog {
            complete_len,
            high_waters,
        });
        let log_end = HEADER_LEN as u64 + complete_len;
        let only_reads = matches!(self.kind, ReservationKind::Peek) && !self.retain_lock;
        if log_end < append_offset && !only_reads {
            return self.begin_truncate_torn_tail(log_end, high_water);
        }
        self.begin_write(log_end, high_water)
    }

    fn begin_truncate_torn_tail(
        &mut self,
        log_end: u64,
        high_water: u64,
    ) -> IOResultOr<ReservedRange> {
        let completion = match self
            .shared
            .file
            .truncate(log_end, Completion::new_trunc(|_| {}))
        {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::TruncatingTornTail {
            completion: completion.clone(),
            log_end,
            high_water,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_truncate_torn_tail(
        &mut self,
        completion: Completion,
        log_end: u64,
        high_water: u64,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::TruncatingTornTail {
                completion: completion.clone(),
                log_end,
                high_water,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        let completion = Completion::new_sync(|_| {});
        let completion = match self.shared.file.sync(completion, self.shared.sync_type) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::SyncingTruncatedTail {
            completion: completion.clone(),
            log_end,
            high_water,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_sync_truncated_tail(
        &mut self,
        completion: Completion,
        log_end: u64,
        high_water: u64,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::SyncingTruncatedTail {
                completion: completion.clone(),
                log_end,
                high_water,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        self.begin_write(log_end, high_water)
    }

    fn begin_write(&mut self, append_offset: u64, high_water: u64) -> IOResultOr<ReservedRange> {
        let range = match &self.kind {
            // A read appends nothing, so there is also nothing to sync: the log
            // was read under this operation's own exclusive lock.
            ReservationKind::Peek => {
                return self.finish(ReservedRange {
                    first: high_water,
                    last: high_water,
                });
            }
            ReservationKind::Reserve { count } => {
                let first = match high_water.checked_add(1) {
                    Some(first) => first,
                    None => return self.fail(LimboError::IntegerOverflow),
                };
                let last = match first.checked_add(count - 1) {
                    Some(last) => last,
                    None => return self.fail(LimboError::IntegerOverflow),
                };
                ReservedRange { first, last }
            }
            ReservationKind::AdvancePast {
                high_water: requested,
            } => {
                if *requested <= high_water {
                    if self.commits_logging_marks().is_some() {
                        return self.finish(ReservedRange {
                            first: high_water,
                            last: high_water,
                        });
                    }
                    return self.begin_sync_existing(high_water);
                }
                ReservedRange {
                    first: *requested,
                    last: *requested,
                }
            }
            ReservationKind::InsertValues { values } => {
                let mut current = high_water;
                let mut reserved_end = high_water;
                let mut reserved = false;
                for value in values {
                    match value {
                        InsertAutoIncrementValue::Generated => {
                            if !reserved {
                                reserved_end = match current.checked_add(values.len() as u64) {
                                    Some(end) => end,
                                    None => return self.fail(LimboError::IntegerOverflow),
                                };
                                reserved = true;
                            }
                            current = match current.checked_add(1) {
                                Some(next) => next,
                                None => return self.fail(LimboError::IntegerOverflow),
                            };
                        }
                        InsertAutoIncrementValue::Explicit(id) => current = current.max(*id),
                    }
                }
                let first = match high_water.checked_add(1) {
                    Some(first) => first,
                    None => return self.fail(LimboError::IntegerOverflow),
                };
                ReservedRange {
                    first,
                    last: current.max(reserved_end),
                }
            }
        };
        let write_end = match append_offset.checked_add(RECORD_LEN as u64) {
            Some(write_end) => write_end,
            None => return self.fail(LimboError::TooBig),
        };
        if write_end > MAX_LOG_BYTES {
            return self.fail(LimboError::TooBig);
        }
        let buffer = Arc::new(Buffer::new(encode_record(self.key, range.last).to_vec()));
        let short_write = Arc::new(AtomicBool::new(false));
        let expected = buffer.len() as i32;
        let short_write_for_callback = short_write.clone();
        let completion = Completion::new_write(move |result| {
            if let Ok(bytes_written) = result {
                if bytes_written != expected {
                    short_write_for_callback.store(true, Ordering::Release);
                }
            }
        });
        let completion = match self
            .shared
            .file
            .pwrite(append_offset, buffer.clone(), completion)
        {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::Writing {
            completion: completion.clone(),
            buffer,
            range,
            short_write,
            append_offset,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_write(
        &mut self,
        completion: Completion,
        buffer: Arc<Buffer>,
        range: ReservedRange,
        short_write: Arc<AtomicBool>,
        append_offset: u64,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::Writing {
                completion: completion.clone(),
                buffer,
                range,
                short_write,
                append_offset,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        if short_write.load(Ordering::Acquire) {
            return self.fail(CompletionError::ShortWrite.into());
        }
        if let Some(commits) = self.commits_logging_marks() {
            commits.written(self.key, range.last);
            return self.finish_record(range, append_offset);
        }

        let completion = Completion::new_sync(|_| {});
        let completion = match self.shared.file.sync(completion, self.shared.sync_type) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::Syncing {
            completion: completion.clone(),
            range,
            append_offset,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_sync(
        &mut self,
        completion: Completion,
        range: ReservedRange,
        append_offset: u64,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::Syncing {
                completion: completion.clone(),
                range,
                append_offset,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        self.finish_record(range, append_offset)
    }

    fn finish_record(
        &mut self,
        range: ReservedRange,
        append_offset: u64,
    ) -> IOResultOr<ReservedRange> {
        // The record is in the log at the end of what was checked, so it
        // joins it; anywhere else, the next operation reads the log again.
        let mut scanned = self.scanned();
        match scanned.as_mut() {
            Some(log) if HEADER_LEN as u64 + log.complete_len == append_offset => {
                log.complete_len += RECORD_LEN as u64;
                log.high_waters.insert(self.key, range.last);
            }
            _ => *scanned = None,
        }
        drop(scanned);
        self.finish(range)
    }

    fn begin_sync_existing(&mut self, high_water: u64) -> IOResultOr<ReservedRange> {
        let completion = Completion::new_sync(|_| {});
        let completion = match self.shared.file.sync(completion, self.shared.sync_type) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::SyncingExisting {
            completion: completion.clone(),
            high_water,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_sync_existing(
        &mut self,
        completion: Completion,
        high_water: u64,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::SyncingExisting {
                completion: completion.clone(),
                high_water,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }
        self.finish(ReservedRange {
            first: high_water,
            last: high_water,
        })
    }

    fn finish(&mut self, range: ReservedRange) -> IOResultOr<ReservedRange> {
        if self.retain_lock {
            self.state = ReservationState::Leased;
            return Ok(IOResult::Done(range));
        }
        if let Err(error) = self.release_lock() {
            self.state = ReservationState::Finished;
            return Err(error.into());
        }
        self.state = ReservationState::Finished;
        Ok(IOResult::Done(range))
    }

    fn fail(&mut self, error: LimboError) -> IOResultOr<ReservedRange> {
        self.state = ReservationState::Finished;
        // What failed under the lock may have left the log other than what was
        // checked. One that never got the lock — answered Busy because another
        // operation holds it — must leave that operation's marks alone.
        if self.holds_lock {
            *self.scanned() = None;
        }
        if let Err(unlock_error) = self.release_lock() {
            tracing::error!(%unlock_error, "failed to unlock auto-increment allocator after failure");
        }
        Err(error.into())
    }

    fn release_lock(&mut self) -> Result<()> {
        if !self.holds_lock {
            return Ok(());
        }
        if let Err(error) = self.shared.file.unlock_file() {
            self.holds_lock = false;
            poison_allocator(&self.shared);
            return Err(error);
        }
        self.holds_lock = false;
        let_the_next_operation_start(&self.shared);
        Ok(())
    }
}

impl Drop for RangeReservation {
    fn drop(&mut self) {
        if self.is_unfinished() {
            poison_allocator(&self.shared);
            return;
        }
        if let Err(error) = self.release_lock() {
            tracing::error!(%error, "failed to unlock dropped auto-increment reservation");
        }
    }
}

impl RangeReservation {
    fn commits_logging_marks(&self) -> Option<Arc<CommitLoggedMarks>> {
        self.shared
            .commits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade()
    }

    fn scanned(&self) -> std::sync::MutexGuard<'_, Option<ScannedLog>> {
        self.shared
            .scanned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn is_leased(&self) -> bool {
        matches!(self.state, ReservationState::Leased)
    }

    fn begin_locked_advance(&mut self, target: u64, current: u64) -> IOResultOr<ReservedRange> {
        if !self.is_leased() || !self.holds_lock || !self.retain_lock {
            return self.fail(LimboError::InternalError(
                "auto-increment lease does not hold the sidecar lock".to_owned(),
            ));
        }
        let size = match self.shared.file.size() {
            Ok(size) => size,
            Err(error) => return self.fail(error),
        };
        if size > MAX_LOG_BYTES {
            return self.fail(LimboError::TooBig);
        }
        self.kind = ReservationKind::AdvancePast { high_water: target };
        if size == 0 {
            return self.begin_write_header();
        }
        if size < HEADER_LEN as u64 {
            return self.fail(LimboError::Corrupt(
                "auto-increment sidecar has a torn header".to_owned(),
            ));
        }
        let append_offset =
            HEADER_LEN as u64 + (size - HEADER_LEN as u64) / RECORD_LEN as u64 * RECORD_LEN as u64;
        self.begin_write(append_offset, current)
    }

    fn is_unfinished(&self) -> bool {
        !matches!(
            self.state,
            ReservationState::Start | ReservationState::Leased | ReservationState::Finished
        )
    }
}

pub struct CommitLoggedMarks {
    file: Arc<dyn File>,
    sync_type: FileSyncType,
    not_yet_logged: Mutex<BTreeMap<AutoIncrementKey, u64>>,
}

impl std::fmt::Debug for CommitLoggedMarks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitLoggedMarks")
            .field("not_yet_logged", &*self.not_yet_logged())
            .finish()
    }
}

impl CommitLoggedMarks {
    pub fn marks_to_log(&self) -> Vec<(AutoIncrementKey, u64)> {
        self.not_yet_logged()
            .iter()
            .map(|(key, high_water)| (*key, *high_water))
            .collect()
    }

    pub fn logged(&self, marks: &[(AutoIncrementKey, u64)]) {
        let mut not_yet_logged = self.not_yet_logged();
        for (key, high_water) in marks {
            if not_yet_logged
                .get(key)
                .is_some_and(|written| written <= high_water)
            {
                not_yet_logged.remove(key);
            }
        }
    }

    pub fn sync_sidecar(&self) -> Result<Completion> {
        self.file.sync(Completion::new_sync(|_| {}), self.sync_type)
    }

    fn written(&self, key: AutoIncrementKey, high_water: u64) {
        let mut not_yet_logged = self.not_yet_logged();
        let mark = not_yet_logged.entry(key).or_insert(high_water);
        *mark = (*mark).max(high_water);
    }

    fn not_yet_logged(&self) -> std::sync::MutexGuard<'_, BTreeMap<AutoIncrementKey, u64>> {
        self.not_yet_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn let_the_next_operation_start(shared: &AllocatorShared) {
    shared.operation_in_progress.store(false, Ordering::Release);
    shared.operations_finished.released();
}

fn poison_allocator(shared: &Arc<AllocatorShared>) {
    shared.poisoned.store(true, Ordering::Release);
    let mut poisoned = match POISONED_ALLOCATORS.lock() {
        Ok(poisoned) => poisoned,
        Err(poisoned) => {
            tracing::error!("auto-increment poison registry was poisoned; recovering lock");
            poisoned.into_inner()
        }
    };
    if !poisoned
        .iter()
        .any(|allocator| Arc::ptr_eq(allocator, shared))
    {
        poisoned.push(shared.clone());
    }
    tracing::error!(
        "dropped auto-increment operation with I/O still pending; allocator is poisoned"
    );
}

fn encode_header(database_identity: AllocatorDatabaseIdentity) -> [u8; HEADER_LEN] {
    let mut header = [0; HEADER_LEN];
    header[0..8].copy_from_slice(&HEADER_MAGIC);
    header[8..10].copy_from_slice(&HEADER_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
    header[12..28].copy_from_slice(&database_identity.0);
    let checksum = crc32c::crc32c(&header[..28]);
    header[28..32].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn decode_header(bytes: &[u8], expected_identity: AllocatorDatabaseIdentity) -> Result<()> {
    if bytes.len() != HEADER_LEN {
        return Err(LimboError::Corrupt(
            "auto-increment sidecar header has the wrong length".to_owned(),
        ));
    }
    if bytes[0..8] != HEADER_MAGIC {
        return Err(LimboError::Corrupt(
            "auto-increment sidecar has an invalid header magic".to_owned(),
        ));
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    if version != HEADER_VERSION {
        return Err(LimboError::Corrupt(format!(
            "auto-increment sidecar has unsupported header version {version}"
        )));
    }
    let header_len = u16::from_le_bytes([bytes[10], bytes[11]]);
    if header_len as usize != HEADER_LEN {
        return Err(LimboError::Corrupt(format!(
            "auto-increment sidecar has invalid header length {header_len}"
        )));
    }
    let expected_crc = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    if crc32c::crc32c(&bytes[..28]) != expected_crc {
        return Err(LimboError::Corrupt(
            "auto-increment sidecar header checksum mismatch".to_owned(),
        ));
    }
    if bytes[12..28] != expected_identity.0 {
        return Err(LimboError::Corrupt(
            "auto-increment sidecar belongs to another database".to_owned(),
        ));
    }
    Ok(())
}

/// The bytes past the header that hold whole records, a torn tail left out.
fn complete_log_len(file_size: u64) -> u64 {
    (file_size - HEADER_LEN as u64) / RECORD_LEN as u64 * RECORD_LEN as u64
}

/// Checks each record in `bytes` and keeps its mark in `high_waters`, each
fn scan_records(bytes: &[u8], high_waters: &mut BTreeMap<AutoIncrementKey, u64>) -> Result<usize> {
    if !bytes.len().is_multiple_of(RECORD_LEN) {
        return Err(LimboError::Corrupt(
            "auto-increment log read did not end at a record boundary".to_owned(),
        ));
    }
    for (index, record) in bytes.chunks_exact(RECORD_LEN).enumerate() {
        let Ok((key, high_water)) = decode_record(record) else {
            return Ok(index * RECORD_LEN);
        };
        if high_water == 0 {
            return Err(LimboError::Corrupt(
                "auto-increment log contains a zero high-water mark".to_owned(),
            ));
        }
        let previous = high_waters.insert(key, high_water).unwrap_or(0);
        if high_water <= previous {
            return Err(LimboError::Corrupt(
                "auto-increment log high-water marks are not strictly increasing".to_owned(),
            ));
        }
    }
    Ok(bytes.len())
}

fn encode_record(key: AutoIncrementKey, high_water: u64) -> [u8; RECORD_LEN] {
    let mut record = [0; RECORD_LEN];
    record[0..4].copy_from_slice(&RECORD_MAGIC);
    record[4..6].copy_from_slice(&RECORD_VERSION.to_le_bytes());
    record[6..8].copy_from_slice(&(RECORD_LEN as u16).to_le_bytes());
    record[8..24].copy_from_slice(&key.0);
    record[24..32].copy_from_slice(&high_water.to_le_bytes());
    let checksum = crc32c::crc32c(&record[..32]);
    record[32..36].copy_from_slice(&checksum.to_le_bytes());
    record
}

fn decode_record(record: &[u8]) -> Result<(AutoIncrementKey, u64)> {
    if record.len() != RECORD_LEN {
        return Err(LimboError::Corrupt(
            "auto-increment log record has the wrong length".to_owned(),
        ));
    }
    if record[0..4] != RECORD_MAGIC {
        return Err(LimboError::Corrupt(
            "auto-increment log has an invalid record magic".to_owned(),
        ));
    }
    let version = u16::from_le_bytes([record[4], record[5]]);
    if version != RECORD_VERSION {
        return Err(LimboError::Corrupt(format!(
            "auto-increment log has unsupported record version {version}"
        )));
    }
    let record_len = u16::from_le_bytes([record[6], record[7]]);
    if record_len as usize != RECORD_LEN {
        return Err(LimboError::Corrupt(format!(
            "auto-increment log record has invalid length {record_len}"
        )));
    }
    let expected_crc = u32::from_le_bytes([record[32], record[33], record[34], record[35]]);
    let actual_crc = crc32c::crc32c(&record[..32]);
    if actual_crc != expected_crc {
        return Err(LimboError::Corrupt(
            "auto-increment log record checksum mismatch".to_owned(),
        ));
    }

    let mut key_bytes = [0; 16];
    key_bytes.copy_from_slice(&record[8..24]);
    let key = AutoIncrementKey::new(key_bytes).map_err(|_| {
        LimboError::Corrupt("auto-increment log contains an invalid key".to_owned())
    })?;
    let high_water = u64::from_le_bytes([
        record[24], record[25], record[26], record[27], record[28], record[29], record[30],
        record[31],
    ]);
    Ok((key, high_water))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Barrier,
        },
        thread,
    };

    use super::*;
    use crate::{
        io::{Clock, FileId, MemoryIO, IO},
        IOExt, MonotonicInstant, WallClockInstant,
    };

    const KEY_A: AutoIncrementKey = AutoIncrementKey(*b"table-key-000001");
    const KEY_B: AutoIncrementKey = AutoIncrementKey(*b"table-key-000002");
    const DATABASE_A: AllocatorDatabaseIdentity = AllocatorDatabaseIdentity(*b"database-key-001");
    const DATABASE_B: AllocatorDatabaseIdentity = AllocatorDatabaseIdentity(*b"database-key-002");

    fn open_allocator(io: &dyn IO) -> DurableRangeAllocator {
        DurableRangeAllocator::open(
            io,
            "auto-increment.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap()
    }

    fn reserve(
        io: &dyn IO,
        allocator: &DurableRangeAllocator,
        key: AutoIncrementKey,
        count: u64,
    ) -> ReservedRange {
        let mut reservation = allocator.reserve(key, count).unwrap();
        io.block(|| reservation.step()).unwrap()
    }

    #[test]
    fn ordered_insert_reservation_keeps_unused_slots_and_explicit_jumps() {
        use InsertAutoIncrementValue::{Explicit, Generated};

        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        assert_eq!(reserve(&io, &allocator, KEY_A, 10).last(), 10);
        let mut operation = allocator
            .reserve_insert_values(KEY_A, vec![Generated, Explicit(2), Generated])
            .unwrap();
        assert_eq!(
            io.block(|| operation.step()).unwrap(),
            ReservedInsertValues {
                high_water_before: 10,
                high_water_after: 13,
            }
        );
        let mut operation = allocator
            .reserve_insert_values(KEY_A, vec![Generated, Explicit(40), Generated])
            .unwrap();
        assert_eq!(
            io.block(|| operation.step()).unwrap(),
            ReservedInsertValues {
                high_water_before: 13,
                high_water_after: 41,
            }
        );
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).first(), 42);
    }

    fn advance_past(
        io: &dyn IO,
        allocator: &DurableRangeAllocator,
        key: AutoIncrementKey,
        high_water: u64,
    ) -> ReservedRange {
        let mut operation = allocator.advance_past(key, high_water).unwrap();
        io.block(|| operation.step()).unwrap()
    }

    #[test]
    fn high_water_lease_keeps_other_reservations_out_until_release() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 1);
        let mut lease = allocator.lease_high_water(KEY_A).unwrap();
        assert_eq!(io.block(|| lease.read()).unwrap(), 1);
        assert!(matches!(
            allocator.reserve(KEY_A, 1).unwrap().step(),
            Err(error) if matches!(*error, LimboError::Busy)
        ));
        assert_eq!(io.block(|| lease.advance_past(4)).unwrap(), 4);
        assert_eq!(io.block(|| lease.advance_past(50)).unwrap(), 50);
        lease.release().unwrap();
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).first(), 51);
    }

    fn peek_high_water(
        io: &dyn IO,
        allocator: &DurableRangeAllocator,
        key: AutoIncrementKey,
    ) -> u64 {
        let mut query = allocator.peek_high_water(key).unwrap();
        io.block(|| query.step()).unwrap()
    }

    fn initialize(io: &dyn IO, allocator: &DurableRangeAllocator) {
        let mut operation = allocator.initialize().unwrap();
        io.block(|| operation.step()).unwrap();
    }

    fn verify(io: &dyn IO, allocator: &DurableRangeAllocator) {
        let mut operation = allocator.verify().unwrap();
        io.block(|| operation.step()).unwrap();
    }

    fn write_bytes(io: &dyn IO, file: Arc<dyn File>, offset: u64, bytes: Vec<u8>) {
        let completion = file
            .pwrite(
                offset,
                Arc::new(Buffer::new(bytes)),
                Completion::new_write(|_| {}),
            )
            .unwrap();
        io.wait_for_completion(completion).unwrap();
    }

    struct ReadCountingIo {
        inner: MemoryIO,
        bytes_read: Arc<std::sync::atomic::AtomicU64>,
        syncs: Arc<std::sync::atomic::AtomicU64>,
    }

    impl ReadCountingIo {
        fn new() -> Self {
            Self {
                inner: MemoryIO::new(),
                bytes_read: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                syncs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }
        }

        fn syncs(&self) -> u64 {
            self.syncs.load(Ordering::Acquire)
        }
    }

    impl Clock for ReadCountingIo {
        fn current_time_monotonic(&self) -> MonotonicInstant {
            self.inner.current_time_monotonic()
        }

        fn current_time_wall_clock(&self) -> WallClockInstant {
            self.inner.current_time_wall_clock()
        }
    }

    impl IO for ReadCountingIo {
        fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
            Ok(Arc::new(ReadCountingFile {
                inner: self.inner.open_file(path, flags, direct)?,
                bytes_read: self.bytes_read.clone(),
                syncs: self.syncs.clone(),
            }))
        }

        fn remove_file(&self, path: &str) -> Result<()> {
            self.inner.remove_file(path)
        }

        fn file_id(&self, path: &str) -> Result<FileId> {
            self.inner.file_id(path)
        }
    }

    struct ReadCountingFile {
        inner: Arc<dyn File>,
        bytes_read: Arc<std::sync::atomic::AtomicU64>,
        syncs: Arc<std::sync::atomic::AtomicU64>,
    }

    impl File for ReadCountingFile {
        fn file_id(&self) -> Result<FileId> {
            self.inner.file_id()
        }

        fn lock_file(&self, exclusive: bool) -> Result<()> {
            self.inner.lock_file(exclusive)
        }

        fn unlock_file(&self) -> Result<()> {
            self.inner.unlock_file()
        }

        fn pread(&self, pos: u64, completion: Completion) -> Result<Completion> {
            self.bytes_read
                .fetch_add(completion.as_read().buf().len() as u64, Ordering::AcqRel);
            self.inner.pread(pos, completion)
        }

        fn pwrite(
            &self,
            pos: u64,
            buffer: Arc<Buffer>,
            completion: Completion,
        ) -> Result<Completion> {
            self.inner.pwrite(pos, buffer, completion)
        }

        fn sync(&self, completion: Completion, sync_type: FileSyncType) -> Result<Completion> {
            self.syncs.fetch_add(1, Ordering::AcqRel);
            self.inner.sync(completion, sync_type)
        }

        fn size(&self) -> Result<u64> {
            self.inner.size()
        }

        fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
            self.inner.truncate(len, completion)
        }
    }

    #[test]
    fn while_commits_log_the_marks_a_record_is_written_without_a_sync() {
        let io = ReadCountingIo::new();
        let allocator = open_allocator(&io);
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 1);
        let syncs_before = io.syncs();

        let commits = allocator.let_commits_log_marks().unwrap();
        assert_eq!(reserve(&io, &allocator, KEY_A, 2).last(), 3);
        assert_eq!(advance_past(&io, &allocator, KEY_A, 2).last(), 3);
        assert_eq!(advance_past(&io, &allocator, KEY_B, 7).last(), 7);
        assert_eq!(io.syncs(), syncs_before);
        assert_eq!(commits.marks_to_log(), vec![(KEY_A, 3), (KEY_B, 7)]);

        commits.logged(&[(KEY_A, 3)]);
        assert_eq!(commits.marks_to_log(), vec![(KEY_B, 7)]);
        assert!(Arc::ptr_eq(
            &commits,
            &allocator.let_commits_log_marks().unwrap()
        ));

        drop(commits);
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 4);
        assert_eq!(io.syncs(), syncs_before + 1);
    }

    #[test]
    fn a_commit_forgets_only_the_marks_it_logged() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        let commits = allocator.let_commits_log_marks().unwrap();
        assert_eq!(reserve(&io, &allocator, KEY_A, 3).last(), 3);
        let logged_marks = commits.marks_to_log();
        assert_eq!(reserve(&io, &allocator, KEY_A, 2).last(), 5);
        commits.logged(&logged_marks);
        assert_eq!(commits.marks_to_log(), vec![(KEY_A, 5)]);
    }

    #[test]
    fn a_record_that_fails_its_check_ends_the_log_and_a_write_truncates_the_rest() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        assert_eq!(reserve(&io, &allocator, KEY_A, 2).last(), 2);
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 3);
        drop(allocator);
        let file = io
            .open_file(
                "auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let second_record = (HEADER_LEN + RECORD_LEN) as u64;
        write_bytes(&io, file.clone(), second_record, vec![0; RECORD_LEN]);
        write_bytes(
            &io,
            file.clone(),
            second_record + RECORD_LEN as u64,
            encode_record(KEY_A, 1).to_vec(),
        );
        let torn_size = file.size().unwrap();

        let reopened = open_allocator(&io);
        assert_eq!(peek_high_water(&io, &reopened, KEY_A), 2);
        assert_eq!(file.size().unwrap(), torn_size);
        assert_eq!(reserve(&io, &reopened, KEY_A, 1).first(), 3);
        assert_eq!(file.size().unwrap(), second_record + RECORD_LEN as u64);
        drop(reopened);

        let reopened = open_allocator(&io);
        assert_eq!(reserve(&io, &reopened, KEY_A, 1).first(), 4);
    }

    struct HandleIdentityIo {
        inner: MemoryIO,
        path_lookup_called: AtomicBool,
    }

    impl HandleIdentityIo {
        fn new() -> Self {
            Self {
                inner: MemoryIO::new(),
                path_lookup_called: AtomicBool::new(false),
            }
        }
    }

    impl Clock for HandleIdentityIo {
        fn current_time_monotonic(&self) -> MonotonicInstant {
            self.inner.current_time_monotonic()
        }

        fn current_time_wall_clock(&self) -> WallClockInstant {
            self.inner.current_time_wall_clock()
        }
    }

    impl IO for HandleIdentityIo {
        fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
            let inner = self.inner.open_file(path, flags, direct)?;
            Ok(Arc::new(HandleIdentityFile {
                inner,
                identity: FileId { dev: 99, ino: 7 },
            }))
        }

        fn remove_file(&self, path: &str) -> Result<()> {
            self.inner.remove_file(path)
        }

        fn file_id(&self, _path: &str) -> Result<FileId> {
            self.path_lookup_called.store(true, Ordering::Release);
            Err(LimboError::InternalError(
                "path identity lookup is a rename race".to_owned(),
            ))
        }
    }

    struct HandleIdentityFile {
        inner: Arc<dyn File>,
        identity: FileId,
    }

    impl File for HandleIdentityFile {
        fn file_id(&self) -> Result<FileId> {
            Ok(self.identity)
        }

        fn lock_file(&self, exclusive: bool) -> Result<()> {
            self.inner.lock_file(exclusive)
        }

        fn unlock_file(&self) -> Result<()> {
            self.inner.unlock_file()
        }

        fn pread(&self, pos: u64, completion: Completion) -> Result<Completion> {
            self.inner.pread(pos, completion)
        }

        fn pwrite(
            &self,
            pos: u64,
            buffer: Arc<Buffer>,
            completion: Completion,
        ) -> Result<Completion> {
            self.inner.pwrite(pos, buffer, completion)
        }

        fn sync(&self, completion: Completion, sync_type: FileSyncType) -> Result<Completion> {
            self.inner.sync(completion, sync_type)
        }

        fn size(&self) -> Result<u64> {
            self.inner.size()
        }

        fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
            self.inner.truncate(len, completion)
        }
    }

    #[test]
    fn open_uses_the_opened_handle_identity_not_a_second_path_lookup() {
        let io = HandleIdentityIo::new();
        let allocator = DurableRangeAllocator::open(
            &io,
            "renamed-sidecar.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert!(!io.path_lookup_called.load(Ordering::Acquire));
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).first(), 1);
    }

    #[test]
    fn initialize_makes_a_fresh_sidecar_reopenable_without_burning_a_range() {
        let io = MemoryIO::new();
        let created = DurableRangeAllocator::open(
            &io,
            "initialized-sidecar.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        initialize(&io, &created);
        verify(&io, &created);
        drop(created);

        let reopened = DurableRangeAllocator::open(
            &io,
            "initialized-sidecar.test",
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        verify(&io, &reopened);
        assert_eq!(reserve(&io, &reopened, KEY_A, 1).first(), 1);
    }

    #[test]
    fn verify_rejects_an_empty_or_foreign_retained_sidecar_without_writing() {
        let io = MemoryIO::new();
        let file = io
            .open_file(
                "retained-sidecar.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let allocator = DurableRangeAllocator::from_file(
            file.clone(),
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        let mut verify_empty = allocator.verify().unwrap();
        assert!(matches!(
            io.block(|| verify_empty.step()),
            Err(LimboError::Corrupt(_))
        ));
        assert_eq!(file.size().unwrap(), 0);

        write_bytes(&io, file.clone(), 0, encode_header(DATABASE_B).to_vec());
        let mut verify_foreign = allocator.verify().unwrap();
        assert!(matches!(
            io.block(|| verify_foreign.step()),
            Err(LimboError::Corrupt(_))
        ));
    }

    #[test]
    fn reopen_mode_cannot_initialize_an_empty_sidecar() {
        let io = MemoryIO::new();
        let file = io
            .open_file(
                "reopen-empty-sidecar.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let allocator = DurableRangeAllocator::from_file(
            file.clone(),
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();

        assert!(matches!(
            allocator.initialize(),
            Err(LimboError::InvalidArgument(_))
        ));
        assert_eq!(file.size().unwrap(), 0);
    }

    #[test]
    fn peeking_reads_the_mark_without_moving_or_writing_it() {
        let io = MemoryIO::new();
        let file = io
            .open_file(
                "peek-sidecar.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let allocator = DurableRangeAllocator::from_file(
            file.clone(),
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();

        // An empty sidecar has handed nothing out, and reading it must not even
        // write the header a reservation would create here.
        assert_eq!(peek_high_water(&io, &allocator, KEY_A), 0);
        assert_eq!(file.size().unwrap(), 0);
        assert_eq!(peek_high_water(&io, &allocator, KEY_A), 0);
        assert_eq!(file.size().unwrap(), 0);
        assert_eq!(
            reserve(&io, &allocator, KEY_A, 3),
            ReservedRange { first: 1, last: 3 }
        );

        // Reading changes not one byte of the log.
        let after_reserve = file.size().unwrap();
        assert_eq!(peek_high_water(&io, &allocator, KEY_A), 3);
        assert_eq!(peek_high_water(&io, &allocator, KEY_B), 0);
        assert_eq!(file.size().unwrap(), after_reserve);
        // Reading did not move the mark: the next reservation continues from 3.
        assert_eq!(
            reserve(&io, &allocator, KEY_A, 1),
            ReservedRange { first: 4, last: 4 }
        );
        assert_eq!(peek_high_water(&io, &allocator, KEY_A), 4);

        // Nothing was appended, so a reopened allocator agrees.
        let reopened = DurableRangeAllocator::from_file(
            file,
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(peek_high_water(&io, &reopened, KEY_A), 4);
        assert_eq!(peek_high_water(&io, &reopened, KEY_B), 0);
    }

    #[test]
    fn reserves_contiguous_ranges_and_recovers_from_a_fresh_allocator() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);

        assert_eq!(
            reserve(&io, &allocator, KEY_A, 1),
            ReservedRange { first: 1, last: 1 }
        );
        assert_eq!(
            reserve(&io, &allocator, KEY_A, 3),
            ReservedRange { first: 2, last: 4 }
        );
        assert_eq!(
            reserve(&io, &allocator, KEY_B, 2),
            ReservedRange { first: 1, last: 2 }
        );

        let reopened = open_allocator(&io);
        assert_eq!(
            reserve(&io, &reopened, KEY_A, 2),
            ReservedRange { first: 5, last: 6 }
        );
    }

    #[test]
    fn advance_past_is_monotonic_and_survives_reopen() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);

        assert_eq!(
            advance_past(&io, &allocator, KEY_A, 10),
            ReservedRange {
                first: 10,
                last: 10
            }
        );
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).first(), 11);
        assert_eq!(advance_past(&io, &allocator, KEY_A, 5).last(), 11);

        let reopened = open_allocator(&io);
        assert_eq!(reserve(&io, &reopened, KEY_A, 1).first(), 12);
    }

    #[test]
    fn empty_sidecars_require_explicit_creation_and_wrong_identity_is_corrupt() {
        let io = MemoryIO::new();
        assert!(AllocatorDatabaseIdentity::new([0; 16]).is_err());
        assert!(DurableRangeAllocator::open(
            &io,
            "missing-auto-increment.test",
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .is_err());

        let file = io
            .open_file(
                "empty-auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let empty = DurableRangeAllocator::open(
            &io,
            "empty-auto-increment.test",
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        let mut reservation = empty.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            io.block(|| reservation.step()),
            Err(LimboError::Corrupt(_))
        ));
        drop(file);

        let live_create = DurableRangeAllocator::open(
            &io,
            "live-create-empty.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert!(matches!(
            DurableRangeAllocator::open(
                &io,
                "live-create-empty.test",
                DATABASE_A,
                AllocatorOpenMode::Reopen,
                FileSyncType::Fsync,
            ),
            Err(LimboError::InvalidArgument(_))
        ));
        drop(live_create);

        let created = DurableRangeAllocator::open(
            &io,
            "wrong-identity.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &created, KEY_A, 1).last(), 1);
        drop(created);
        let wrong_identity = DurableRangeAllocator::open(
            &io,
            "wrong-identity.test",
            DATABASE_B,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        let mut reservation = wrong_identity.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            io.block(|| reservation.step()),
            Err(LimboError::Corrupt(_))
        ));
    }

    #[cfg(feature = "fs")]
    #[test]
    fn reopened_sidecar_uses_a_new_file_handle_after_all_allocators_drop() {
        use crate::PlatformIO;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auto-increment.sidecar");
        let path = path.to_str().unwrap();
        let io = PlatformIO::new().unwrap();
        let created = DurableRangeAllocator::open(
            &io,
            path,
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &created, KEY_A, 1).last(), 1);
        drop(created);

        let reopened = DurableRangeAllocator::open(
            &io,
            path,
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &reopened, KEY_A, 1).first(), 2);
    }

    #[test]
    fn rejects_zero_count_and_counter_overflow() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        assert!(matches!(
            allocator.reserve(KEY_A, 0),
            Err(LimboError::InvalidArgument(_))
        ));

        let file = io
            .open_file(
                "auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        write_bytes(&io, file.clone(), 0, encode_header(DATABASE_A).to_vec());
        write_bytes(
            &io,
            file,
            HEADER_LEN as u64,
            encode_record(KEY_A, u64::MAX).to_vec(),
        );
        let mut reservation = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            io.block(|| reservation.step()),
            Err(LimboError::IntegerOverflow)
        ));
    }

    #[test]
    fn append_must_fit_before_submitting_a_record_write() {
        let io = MemoryIO::new();
        let file = io
            .open_file(
                "append-boundary.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let allocator = DurableRangeAllocator::from_file(
            file.clone(),
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        let mut reservation = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            reservation.begin_write(MAX_LOG_BYTES - RECORD_LEN as u64 + 1, 0),
            Err(error) if matches!(*error, LimboError::TooBig)
        ));
        assert_eq!(file.size().unwrap(), 0);
    }

    #[test]
    fn remove_and_recreate_produces_a_new_memory_file_identity() {
        let io = MemoryIO::new();
        let original = DurableRangeAllocator::open(
            &io,
            "recreated-auto-increment.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &original, KEY_A, 1).last(), 1);
        io.remove_file("recreated-auto-increment.test").unwrap();

        let recreated = DurableRangeAllocator::open(
            &io,
            "recreated-auto-increment.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &recreated, KEY_A, 1).first(), 1);
        assert!(!Arc::ptr_eq(&original.shared, &recreated.shared));
    }

    #[test]
    fn torn_tail_cannot_lower_an_acknowledged_high_water_mark() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        assert_eq!(reserve(&io, &allocator, KEY_A, 2).last(), 2);

        let file = io
            .open_file(
                "auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        write_bytes(
            &io,
            file.clone(),
            (HEADER_LEN + RECORD_LEN) as u64,
            vec![0xA5; 3],
        );
        let reopened = open_allocator(&io);
        assert_eq!(reserve(&io, &reopened, KEY_A, 1).first(), 3);
        assert_eq!(file.size().unwrap(), (HEADER_LEN + RECORD_LEN * 2) as u64);

        write_bytes(&io, file, 0, vec![0; 1]);
        let corrupt = open_allocator(&io);
        let mut reservation = corrupt.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            io.block(|| reservation.step()),
            Err(LimboError::Corrupt(_))
        ));
    }

    /// Each reservation used to read and check every record the log had ever
    /// held, so each insert into a counted table took longer than the one
    /// before. One reads the header and nothing it has already checked.
    #[test]
    fn a_reservation_reads_only_the_records_appended_since_the_last() {
        let io = ReadCountingIo::new();
        let allocator = open_allocator(&io);
        for _ in 0..2_000 {
            reserve(&io, &allocator, KEY_A, 1);
        }
        let before = io.bytes_read.load(Ordering::Acquire);
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).first(), 2_001);
        assert_eq!(
            io.bytes_read.load(Ordering::Acquire) - before,
            HEADER_LEN as u64
        );
    }

    /// Another process appends to the same sidecar under its own lock; the
    /// records past those this one checked are read before it goes on.
    #[test]
    fn a_reservation_reads_records_another_handle_appended() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        assert_eq!(reserve(&io, &allocator, KEY_A, 2).last(), 2);
        let file = io
            .open_file(
                "auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let end = file.size().unwrap();
        write_bytes(&io, file.clone(), end, encode_record(KEY_A, 40).to_vec());
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).first(), 41);

        // A record that does not rise is caught in what was appended too.
        let end = file.size().unwrap();
        write_bytes(&io, file, end, encode_record(KEY_A, 7).to_vec());
        let mut reservation = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            io.block(|| reservation.step()),
            Err(LimboError::Corrupt(_))
        ));
    }

    /// An operation turned away because another holds the sidecar must not
    /// drop the marks that other one is reading past: here the first is
    /// waiting on its read of the records another handle appended when the
    /// second is turned away, and the first then goes on.
    #[test]
    fn a_reservation_turned_away_leaves_the_running_one_its_checked_marks() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        // The second reservation reads the log the first created, and keeps
        // its marks.
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 1);
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 2);
        let file = io
            .open_file(
                "auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let end = file.size().unwrap();
        write_bytes(&io, file, end, encode_record(KEY_B, 5).to_vec());

        let mut running = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(running.step(), Ok(IOResult::IO(_))));
        assert!(matches!(running.step(), Ok(IOResult::IO(_))));
        let mut turned_away = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(turned_away.step(), Err(error) if matches!(*error, LimboError::Busy)));
        assert_eq!(io.block(|| running.step()).unwrap().first(), 3);
        assert_eq!(reserve(&io, &allocator, KEY_B, 1).first(), 6);
    }

    #[test]
    fn the_last_seen_mark_is_what_the_last_operation_left_and_busy_while_one_runs() {
        let io = MemoryIO::new();
        let allocator = open_allocator(&io);
        assert_eq!(allocator.last_seen_high_water(KEY_A).unwrap(), None);
        assert_eq!(reserve(&io, &allocator, KEY_A, 3).last(), 3);
        let mut peek = allocator.peek_high_water(KEY_A).unwrap();
        assert_eq!(io.block(|| peek.step()).unwrap(), 3);
        assert_eq!(allocator.last_seen_high_water(KEY_A).unwrap(), Some(3));
        assert_eq!(allocator.last_seen_high_water(KEY_B).unwrap(), Some(0));

        let mut running = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(running.step(), Ok(IOResult::IO(_))));
        assert!(matches!(
            allocator.last_seen_high_water(KEY_A),
            Err(LimboError::Busy)
        ));
        assert_eq!(io.block(|| running.step()).unwrap().first(), 4);
        assert_eq!(allocator.last_seen_high_water(KEY_A).unwrap(), Some(4));
    }

    #[test]
    fn a_waiter_turned_away_wakes_when_the_running_operation_finishes() {
        let io = Arc::new(MemoryIO::new());
        let allocator = Arc::new(open_allocator(io.as_ref()));
        let mut running = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(running.step(), Ok(IOResult::IO(_))));
        let seen = allocator.operations_finished();
        let mut turned_away = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(turned_away.step(), Err(error) if matches!(*error, LimboError::Busy)));
        let waiter = thread::spawn({
            let allocator = allocator.clone();
            move || {
                let started = std::time::Instant::now();
                let finished = allocator
                    .wait_for_an_operation_to_finish(seen, std::time::Duration::from_secs(30));
                (finished, started.elapsed())
            }
        });
        thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(io.block(|| running.step()).unwrap().first(), 1);
        let (finished, waited) = waiter.join().unwrap();
        assert_eq!(finished, Some(true));
        assert!(
            waited < std::time::Duration::from_secs(10),
            "waited {waited:?}"
        );
        assert_eq!(reserve(io.as_ref(), &allocator, KEY_A, 1).first(), 2);
    }

    #[test]
    fn concurrent_reservations_do_not_overlap() {
        let io = Arc::new(MemoryIO::new());
        let allocator = Arc::new(open_allocator(io.as_ref()));
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let io = io.clone();
            let allocator = allocator.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                loop {
                    let mut reservation = allocator.reserve(KEY_A, 4).unwrap();
                    match io.block(|| reservation.step()) {
                        Ok(range) => return range,
                        Err(LimboError::Busy) => thread::yield_now(),
                        Err(error) => panic!("reservation failed: {error}"),
                    }
                }
            }));
        }
        barrier.wait();
        let mut ranges = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        ranges.sort_by_key(|range| range.first());
        assert_eq!(
            ranges,
            vec![
                ReservedRange { first: 1, last: 4 },
                ReservedRange { first: 5, last: 8 }
            ]
        );
    }

    struct FailingSyncFile {
        inner: Arc<dyn File>,
        sync_calls: AtomicUsize,
        fail_on_sync_call: usize,
    }

    struct FailingUnlockIo {
        inner: MemoryIO,
    }

    impl FailingUnlockIo {
        fn new() -> Self {
            Self {
                inner: MemoryIO::new(),
            }
        }
    }

    impl Clock for FailingUnlockIo {
        fn current_time_monotonic(&self) -> MonotonicInstant {
            self.inner.current_time_monotonic()
        }

        fn current_time_wall_clock(&self) -> WallClockInstant {
            self.inner.current_time_wall_clock()
        }
    }

    impl IO for FailingUnlockIo {
        fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
            Ok(Arc::new(FailingUnlockFile {
                inner: self.inner.open_file(path, flags, direct)?,
            }))
        }

        fn remove_file(&self, path: &str) -> Result<()> {
            self.inner.remove_file(path)
        }
    }

    struct FailingUnlockFile {
        inner: Arc<dyn File>,
    }

    impl File for FailingUnlockFile {
        fn file_id(&self) -> Result<FileId> {
            self.inner.file_id()
        }

        fn lock_file(&self, exclusive: bool) -> Result<()> {
            self.inner.lock_file(exclusive)
        }

        fn unlock_file(&self) -> Result<()> {
            Err(LimboError::InternalError(
                "injected unlock failure".to_owned(),
            ))
        }

        fn pread(&self, pos: u64, completion: Completion) -> Result<Completion> {
            self.inner.pread(pos, completion)
        }

        fn pwrite(
            &self,
            pos: u64,
            buffer: Arc<Buffer>,
            completion: Completion,
        ) -> Result<Completion> {
            self.inner.pwrite(pos, buffer, completion)
        }

        fn sync(&self, completion: Completion, sync_type: FileSyncType) -> Result<Completion> {
            self.inner.sync(completion, sync_type)
        }

        fn size(&self) -> Result<u64> {
            self.inner.size()
        }

        fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
            self.inner.truncate(len, completion)
        }
    }

    impl File for FailingSyncFile {
        fn file_id(&self) -> Result<FileId> {
            self.inner.file_id()
        }

        fn lock_file(&self, exclusive: bool) -> Result<()> {
            self.inner.lock_file(exclusive)
        }

        fn unlock_file(&self) -> Result<()> {
            self.inner.unlock_file()
        }

        fn pread(&self, pos: u64, completion: Completion) -> Result<Completion> {
            self.inner.pread(pos, completion)
        }

        fn pwrite(
            &self,
            pos: u64,
            buffer: Arc<Buffer>,
            completion: Completion,
        ) -> Result<Completion> {
            self.inner.pwrite(pos, buffer, completion)
        }

        fn sync(&self, completion: Completion, sync_type: FileSyncType) -> Result<Completion> {
            if self.sync_calls.fetch_add(1, Ordering::AcqRel) == self.fail_on_sync_call {
                return Err(LimboError::InternalError(
                    "injected sync failure".to_owned(),
                ));
            }
            self.inner.sync(completion, sync_type)
        }

        fn size(&self) -> Result<u64> {
            self.inner.size()
        }

        fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
            self.inner.truncate(len, completion)
        }
    }

    #[test]
    fn sync_failure_never_publishes_a_range_and_burns_the_written_value() {
        let io = MemoryIO::new();
        let inner = io
            .open_file(
                "sync-failure.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let failing = Arc::new(FailingSyncFile {
            inner: inner.clone(),
            sync_calls: AtomicUsize::new(0),
            fail_on_sync_call: 1,
        });
        let allocator = DurableRangeAllocator::from_file(
            failing,
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        let mut reservation = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            io.block(|| reservation.step()),
            Err(LimboError::InternalError(message)) if message == "injected sync failure"
        ));

        let reopened = DurableRangeAllocator::from_file(
            inner,
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(
            reserve(&io, &reopened, KEY_A, 1),
            ReservedRange { first: 2, last: 2 }
        );
    }

    #[test]
    fn lease_sync_failure_releases_the_lock_and_preserves_the_written_mark() {
        let io = MemoryIO::new();
        let inner = io
            .open_file(
                "lease-sync-failure.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let failing = Arc::new(FailingSyncFile {
            inner: inner.clone(),
            sync_calls: AtomicUsize::new(0),
            fail_on_sync_call: 2,
        });
        let allocator = DurableRangeAllocator::from_file(
            failing,
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 1);
        let mut lease = allocator.lease_high_water(KEY_A).unwrap();
        assert_eq!(io.block(|| lease.read()).unwrap(), 1);
        assert!(matches!(
            io.block(|| lease.advance_past(50)),
            Err(LimboError::InternalError(message)) if message == "injected sync failure"
        ));
        drop(lease);

        let reopened = DurableRangeAllocator::from_file(
            inner,
            DATABASE_A,
            AllocatorOpenMode::Reopen,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &reopened, KEY_A, 1).first(), 51);
    }

    #[test]
    fn retrying_initialization_syncs_a_header_left_by_a_failed_sync() {
        let io = MemoryIO::new();
        let inner = io
            .open_file(
                "initialize-sync-failure.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        let failing = Arc::new(FailingSyncFile {
            inner: inner.clone(),
            sync_calls: AtomicUsize::new(0),
            fail_on_sync_call: 0,
        });
        let allocator = DurableRangeAllocator::from_file(
            failing,
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        let mut operation = allocator.initialize().unwrap();
        assert!(matches!(
            io.block(|| operation.step()),
            Err(LimboError::InternalError(message)) if message == "injected sync failure"
        ));
        drop(allocator);

        let reopened = DurableRangeAllocator::from_file(
            inner,
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        initialize(&io, &reopened);
        verify(&io, &reopened);
    }

    #[test]
    fn unlock_failure_poisoned_the_sidecar_and_later_open_rejects_it() {
        let io = FailingUnlockIo::new();
        let allocator = DurableRangeAllocator::open(
            &io,
            "unlock-failure.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        let mut reservation = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            io.block(|| reservation.step()),
            Err(LimboError::InternalError(message)) if message == "injected unlock failure"
        ));
        assert!(matches!(
            allocator.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));
        drop(allocator);

        let reopened = DurableRangeAllocator::open(
            &io,
            "unlock-failure.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert!(matches!(
            reopened.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));
    }

    #[cfg(feature = "io_memory_yield")]
    #[test]
    fn reentry_waits_for_each_io_completion_without_a_second_write() {
        use crate::io::MemoryYieldIO;

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        let mut reservation = allocator.reserve(KEY_A, 2).unwrap();

        assert!(matches!(reservation.step().unwrap(), IOResult::IO(_)));
        assert!(matches!(reservation.step().unwrap(), IOResult::IO(_)));
        let file = io
            .open_file(
                "auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        assert_eq!(file.size().unwrap(), HEADER_LEN as u64);

        io.step().unwrap();
        assert!(matches!(reservation.step().unwrap(), IOResult::IO(_)));
        io.step().unwrap();
        assert!(matches!(reservation.step().unwrap(), IOResult::IO(_)));
        assert_eq!(file.size().unwrap(), (HEADER_LEN + RECORD_LEN) as u64);
        io.step().unwrap();
        assert!(matches!(reservation.step().unwrap(), IOResult::IO(_)));
        io.step().unwrap();
        assert!(matches!(
            reservation.step().unwrap(),
            IOResult::Done(ReservedRange { first: 1, last: 2 })
        ));
    }

    #[cfg(feature = "io_memory_yield")]
    #[test]
    fn sidecar_initialization_reentry_writes_and_syncs_the_header_once() {
        use crate::io::MemoryYieldIO;

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        let mut initialize = allocator.initialize().unwrap();

        assert!(matches!(initialize.step().unwrap(), IOResult::IO(_)));
        assert!(matches!(initialize.step().unwrap(), IOResult::IO(_)));
        let file = io
            .open_file(
                "auto-increment.test",
                OpenFlags::Create | OpenFlags::NoLock,
                false,
            )
            .unwrap();
        assert_eq!(file.size().unwrap(), HEADER_LEN as u64);

        io.step().unwrap();
        assert!(matches!(initialize.step().unwrap(), IOResult::IO(_)));
        io.step().unwrap();
        assert!(matches!(initialize.step().unwrap(), IOResult::Done(())));
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).first(), 1);
    }

    #[cfg(feature = "io_memory_yield")]
    #[test]
    fn dropping_after_an_intermediate_completion_poisoned_the_allocator() {
        use crate::io::MemoryYieldIO;

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        let mut initialization = allocator.initialize().unwrap();
        assert!(matches!(initialization.step().unwrap(), IOResult::IO(_)));
        io.step().unwrap();
        drop(initialization);

        assert!(matches!(
            allocator.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        let mut reservation = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(reservation.step().unwrap(), IOResult::IO(_)));
        io.step().unwrap();
        drop(reservation);
        assert!(matches!(
            allocator.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));
    }

    #[cfg(feature = "io_memory_yield")]
    #[test]
    fn dropping_a_pending_reservation_poisoned_the_shared_allocator() {
        use crate::io::MemoryYieldIO;

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        let mut writing = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(writing.step().unwrap(), IOResult::IO(_)));
        drop(writing);
        assert!(matches!(
            allocator.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));
        let reopened = open_allocator(&io);
        assert!(matches!(
            reopened.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        assert_eq!(reserve(&io, &allocator, KEY_A, 1).last(), 1);
        let mut reading = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(reading.step().unwrap(), IOResult::IO(_)));
        drop(reading);
        assert!(matches!(
            allocator.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        let mut syncing = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(syncing.step().unwrap(), IOResult::IO(_)));
        io.step().unwrap();
        assert!(matches!(syncing.step().unwrap(), IOResult::IO(_)));
        drop(syncing);
        assert!(matches!(
            allocator.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));
    }

    #[cfg(feature = "io_memory_yield")]
    #[test]
    fn pending_drop_retains_the_file_even_when_the_poison_registry_was_poisoned() {
        let _ = std::panic::catch_unwind(|| {
            let _guard = POISONED_ALLOCATORS.lock().unwrap();
            panic!("poison the retention mutex");
        });

        use crate::io::MemoryYieldIO;

        let io = MemoryYieldIO::new();
        let allocator = open_allocator(&io);
        let mut reservation = allocator.reserve(KEY_A, 1).unwrap();
        assert!(matches!(reservation.step().unwrap(), IOResult::IO(_)));
        drop(reservation);
        drop(allocator);

        let reopened = open_allocator(&io);
        assert!(matches!(
            reopened.reserve(KEY_A, 1),
            Err(LimboError::InternalError(_))
        ));
    }

    #[cfg(feature = "io_memory_yield")]
    #[test]
    fn opening_the_same_sidecar_twice_shares_the_in_process_gate() {
        use crate::io::MemoryYieldIO;

        let io = MemoryYieldIO::new();
        let first = open_allocator(&io);
        let second = open_allocator(&io);
        let mut first_reservation = first.reserve(KEY_A, 1).unwrap();
        assert!(matches!(first_reservation.step().unwrap(), IOResult::IO(_)));

        let mut second_reservation = second.reserve(KEY_A, 1).unwrap();
        assert!(matches!(
            second_reservation.step(),
            Err(error) if matches!(*error, LimboError::Busy)
        ));
        drop(first_reservation);
    }

    #[cfg(feature = "io_memory_yield")]
    #[test]
    fn remove_and_recreate_changes_the_memory_yield_file_identity() {
        use crate::io::MemoryYieldIO;

        let io = MemoryYieldIO::new();
        let original = DurableRangeAllocator::open(
            &io,
            "recreated-yield-auto-increment.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &original, KEY_A, 1).last(), 1);
        io.remove_file("recreated-yield-auto-increment.test")
            .unwrap();

        let recreated = DurableRangeAllocator::open(
            &io,
            "recreated-yield-auto-increment.test",
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        assert_eq!(reserve(&io, &recreated, KEY_A, 1).first(), 1);
        assert!(!Arc::ptr_eq(&original.shared, &recreated.shared));
    }
}
