//! Durable, dialect-neutral allocation of monotonically increasing row-id ranges.
//!
//! The allocator owns a small append-only sidecar file. A range becomes visible
//! only after its record has been synced. Callers therefore must not wrap this
//! operation in a user transaction: a rolled-back statement intentionally burns
//! its range.
//! A final partial record is an unacknowledged torn append and is overwritten at
//! the next record boundary. A malformed full record is corruption and stops
//! allocation.

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
    reservation_in_progress: AtomicBool,
    poisoned: AtomicBool,
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
            reservation_in_progress: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
        });
        open_allocators.insert(identity, Arc::downgrade(&shared));
        Ok(Self { shared })
    }

    #[cfg(test)]
    fn from_file(
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
                reservation_in_progress: AtomicBool::new(false),
                poisoned: AtomicBool::new(false),
            }),
        }
    }

    /// Begins a reservation. Repeatedly call [`RangeReservation::step`] until
    /// it returns [`IOResult::Done`].
    pub fn reserve(&self, key: AutoIncrementKey, count: u64) -> Result<RangeReservation> {
        if self.shared.poisoned.load(Ordering::Acquire) {
            return Err(LimboError::InternalError(
                "auto-increment allocator was dropped with I/O still pending".to_owned(),
            ));
        }
        if count == 0 {
            return Err(LimboError::InvalidArgument(
                "auto-increment reservation count must be greater than zero".to_owned(),
            ));
        }

        Ok(RangeReservation {
            shared: self.shared.clone(),
            key,
            count,
            state: ReservationState::Start,
            holds_lock: false,
        })
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
    count: u64,
    state: ReservationState,
    holds_lock: bool,
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
    },
    Writing {
        completion: Completion,
        buffer: Arc<Buffer>,
        range: ReservedRange,
        short_write: Arc<AtomicBool>,
    },
    Syncing {
        completion: Completion,
        range: ReservedRange,
    },
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
            } => self.finish_read(completion, buffer, append_offset),
            ReservationState::Writing {
                completion,
                buffer,
                range,
                short_write,
            } => self.finish_write(completion, buffer, range, short_write),
            ReservationState::Syncing { completion, range } => self.finish_sync(completion, range),
            ReservationState::Finished => self.fail(LimboError::InternalError(
                "auto-increment reservation was stepped after completion".to_owned(),
            )),
        }
    }

    fn start(&mut self) -> IOResultOr<ReservedRange> {
        if self
            .shared
            .reservation_in_progress
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return self.fail(LimboError::Busy);
        }

        if let Err(error) = self.shared.file.lock_file(true) {
            self.shared
                .reservation_in_progress
                .store(false, Ordering::Release);
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
            return match self.shared.open_mode {
                AllocatorOpenMode::Create => self.begin_write_header(),
                AllocatorOpenMode::Reopen => self.fail(LimboError::Corrupt(
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

    fn begin_read_log(&mut self, file_size: u64) -> IOResultOr<ReservedRange> {
        let log_len = file_size - HEADER_LEN as u64;
        let complete_len = log_len / RECORD_LEN as u64 * RECORD_LEN as u64;
        let append_offset = HEADER_LEN as u64 + complete_len;
        if complete_len == 0 {
            return self.begin_write(append_offset, 0);
        }
        let read_len = match usize::try_from(complete_len) {
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
        let completion = match self.shared.file.pread(HEADER_LEN as u64, completion) {
            Ok(completion) => completion,
            Err(error) => return self.fail(error),
        };
        self.state = ReservationState::Reading {
            completion: completion.clone(),
            buffer,
            append_offset,
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
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::Reading {
                completion: completion.clone(),
                buffer,
                append_offset,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
        }

        let high_water = match scan_log(buffer.as_slice(), self.key) {
            Ok(high_water) => high_water,
            Err(error) => return self.fail(error),
        };
        self.begin_write(append_offset, high_water)
    }

    fn begin_write(&mut self, append_offset: u64, high_water: u64) -> IOResultOr<ReservedRange> {
        let write_end = match append_offset.checked_add(RECORD_LEN as u64) {
            Some(write_end) => write_end,
            None => return self.fail(LimboError::TooBig),
        };
        if write_end > MAX_LOG_BYTES {
            return self.fail(LimboError::TooBig);
        }
        let first = match high_water.checked_add(1) {
            Some(first) => first,
            None => return self.fail(LimboError::IntegerOverflow),
        };
        let last = match first.checked_add(self.count - 1) {
            Some(last) => last,
            None => return self.fail(LimboError::IntegerOverflow),
        };
        let range = ReservedRange { first, last };
        let buffer = Arc::new(Buffer::new(encode_record(self.key, last).to_vec()));
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
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_write(
        &mut self,
        completion: Completion,
        buffer: Arc<Buffer>,
        range: ReservedRange,
        short_write: Arc<AtomicBool>,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::Writing {
                completion: completion.clone(),
                buffer,
                range,
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
        self.state = ReservationState::Syncing {
            completion: completion.clone(),
            range,
        };
        Ok(IOResult::IO(IOCompletions(completion)))
    }

    fn finish_sync(
        &mut self,
        completion: Completion,
        range: ReservedRange,
    ) -> IOResultOr<ReservedRange> {
        if !completion.finished() {
            self.state = ReservationState::Syncing {
                completion: completion.clone(),
                range,
            };
            return Ok(IOResult::IO(IOCompletions(completion)));
        }
        if let Some(error) = completion.get_error() {
            return self.fail(error.into());
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
            self.poison();
            return Err(error);
        }
        self.holds_lock = false;
        self.shared
            .reservation_in_progress
            .store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for RangeReservation {
    fn drop(&mut self) {
        if self.has_pending_io() {
            self.poison();
            return;
        }
        if let Err(error) = self.release_lock() {
            tracing::error!(%error, "failed to unlock dropped auto-increment reservation");
        }
    }
}

impl RangeReservation {
    fn poison(&self) {
        self.shared.poisoned.store(true, Ordering::Release);
        let mut poisoned = match POISONED_ALLOCATORS.lock() {
            Ok(poisoned) => poisoned,
            Err(poisoned) => {
                tracing::error!("auto-increment poison registry was poisoned; recovering lock");
                poisoned.into_inner()
            }
        };
        if !poisoned
            .iter()
            .any(|allocator| Arc::ptr_eq(allocator, &self.shared))
        {
            poisoned.push(self.shared.clone());
        }
        tracing::error!(
            "dropped auto-increment reservation with I/O still pending; allocator is poisoned"
        );
    }

    fn has_pending_io(&self) -> bool {
        match &self.state {
            ReservationState::ReadingHeader { completion, .. }
            | ReservationState::WritingHeader { completion, .. }
            | ReservationState::SyncingHeader { completion }
            | ReservationState::Reading { completion, .. }
            | ReservationState::Writing { completion, .. }
            | ReservationState::Syncing { completion, .. } => !completion.finished(),
            ReservationState::Start | ReservationState::Finished => false,
        }
    }
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

fn scan_log(bytes: &[u8], requested_key: AutoIncrementKey) -> Result<u64> {
    if !bytes.len().is_multiple_of(RECORD_LEN) {
        return Err(LimboError::Corrupt(
            "auto-increment log read did not end at a record boundary".to_owned(),
        ));
    }

    let mut high_waters = BTreeMap::new();
    for record in bytes.chunks_exact(RECORD_LEN) {
        let (key, high_water) = decode_record(record)?;
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

    Ok(high_waters.get(&requested_key).copied().unwrap_or(0))
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
        );
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
            if self.sync_calls.fetch_add(1, Ordering::AcqRel) == 1 {
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
        });
        let allocator = DurableRangeAllocator::from_file(
            failing,
            DATABASE_A,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        );
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
        );
        assert_eq!(
            reserve(&io, &reopened, KEY_A, 1),
            ReservedRange { first: 2, last: 2 }
        );
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
