//! Empties a database's WAL on a thread of its own.
//!
//! A write large enough leaves the WAL holding more than the engine's own
//! checkpoint empties, and emptying it copies every frame into the database
//! and syncs it: measured, about two seconds after an UPDATE of 128,000 rows.
//! Done inside the statement that crossed the bound, that time lands on one
//! client's answer. So a session only asks, and this thread does the work over
//! a connection of its own, which leaves the session free to run its next
//! statement meanwhile.
//!
//! The same thread syncs, about once a second, the records the AUTO_INCREMENT
//! counters of MVCC databases wrote without a sync.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use turso_core::storage::auto_increment::CommitLoggedMarks;
use turso_core::{
    CheckpointMode, Database, LimboError, MvccCheckpointWritingRowsFirst, Result, IO,
};

use crate::database_users::DatabaseUser;

/// The thread one catalog empties its databases' WALs on, stopped and joined
/// when the catalog goes.
pub(crate) struct WalKeeper {
    handle: WalKeeperHandle,
    thread: Option<JoinHandle<()>>,
}

/// What a session asks the keeper through.
#[derive(Clone)]
pub(crate) struct WalKeeperHandle {
    requests: Sender<Request>,
    /// A failure the thread met that no session has been told of yet.
    failure: Arc<Mutex<Option<LimboError>>>,
    /// How many WALs the thread has emptied.
    #[cfg(test)]
    pub(crate) truncated: Arc<std::sync::atomic::AtomicUsize>,
    /// How many times the thread has tried to empty one.
    #[cfg(test)]
    pub(crate) attempts: Arc<std::sync::atomic::AtomicUsize>,
    /// How many times the thread's try to empty one was refused.
    #[cfg(test)]
    pub(crate) refusals: Arc<std::sync::atomic::AtomicUsize>,
}

enum Request {
    /// The database is held weakly, so a request never keeps one open. The
    /// user counts the keeper as using a catalog's database while it empties
    /// the WAL, which writes the database file, so that a drop never runs
    /// beside it; a database dropped, or being dropped, is left alone.
    Truncate(Weak<Database>, Option<DatabaseUser>),
    /// Held weakly too: the database's store owns the marks.
    SyncCounterRecords(Weak<CommitLoggedMarks>, Arc<dyn IO>),
    #[cfg(test)]
    Answer(Sender<()>),
    Stop,
}

impl WalKeeper {
    pub(crate) fn start() -> Self {
        let (requests, received) = mpsc::channel();
        let handle = WalKeeperHandle {
            requests,
            failure: Arc::default(),
            #[cfg(test)]
            truncated: Arc::default(),
            #[cfg(test)]
            attempts: Arc::default(),
            #[cfg(test)]
            refusals: Arc::default(),
        };
        let thread = std::thread::Builder::new()
            .name("turso-mysql-wal-keeper".to_owned())
            .spawn({
                let handle = handle.clone();
                move || keep(received, handle)
            })
            .expect("the WAL keeper thread must start");
        Self {
            handle,
            thread: Some(thread),
        }
    }

    pub(crate) fn handle(&self) -> WalKeeperHandle {
        self.handle.clone()
    }
}

impl Drop for WalKeeper {
    fn drop(&mut self) {
        // A session still holding a handle keeps the channel open, so the
        // thread is told to stop rather than left to notice.
        let _ = self.handle.requests.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the WAL keeper thread must not panic");
        }
    }
}

impl WalKeeperHandle {
    /// Asks for one database's WAL to be emptied, and answers a failure the
    /// thread met since a session was last told, if there was one.
    pub(crate) fn ask_to_truncate(
        &self,
        database: &Weak<Database>,
        user: Option<DatabaseUser>,
    ) -> Result<()> {
        if let Some(failure) = self
            .failure
            .lock()
            .expect("WAL keeper failure mutex poisoned")
            .take()
        {
            return Err(failure);
        }
        // The thread outlives every session of its catalog, so a request only
        // goes unread once the catalog is closing, when there is nothing left
        // to empty the WAL for.
        let _ = self
            .requests
            .send(Request::Truncate(database.clone(), user));
        Ok(())
    }

    /// Asks the thread to sync, about once a second from now on, the records
    /// one MVCC database's counters write without a sync.
    pub(crate) fn keep_counter_records_synced(
        &self,
        marks: &Arc<CommitLoggedMarks>,
        io: Arc<dyn IO>,
    ) {
        let _ = self
            .requests
            .send(Request::SyncCounterRecords(Arc::downgrade(marks), io));
    }

    /// Waits for every request sent before this one to be done.
    #[cfg(test)]
    pub(crate) fn wait_for_the_requests_before(&self) {
        let (answered, answer) = mpsc::channel();
        self.requests
            .send(Request::Answer(answered))
            .expect("the WAL keeper thread must be running");
        answer.recv().expect("the WAL keeper thread must answer");
    }
}

/// How long a WAL that sessions kept busy is left alone before the keeper
/// tries it again.
///
/// Emptying a WAL takes the database's write lock, and finds out only then
/// whether a session's snapshot still needs the frames. Under a steady load
/// one always does, and every statement that ends with the WAL past its bound
/// asks again, so the keeper tried back to back: measured with eight
/// sysbench sessions inserting, it held the write lock for 25 to 50% of the
/// time, about 1,200 attempts a second, nearly all of them refused.
pub(crate) const PAUSE_AFTER_A_BUSY_WAL: Duration = Duration::from_millis(50);

/// How long the keeper goes on trying a WAL that sessions keep busy, trying
/// again each time a session lets go of the write lock.
///
/// Under a steady write load some session holds the write lock nearly all
/// the time. Measured with eight sessions inserting, a keeper that tried once
/// found it held in nine tries out of ten, and the WAL grew past 150 MB in
/// eight seconds without being emptied once.
pub(crate) const TIME_TO_WAIT_FOR_A_BUSY_WAL: Duration = Duration::from_millis(100);

/// How long the keeper holds new transactions of an MVCC database back
/// while it waits for the running ones to end so that it can checkpoint.
/// Measured with eight sysbench sessions running `oltp_read_write`, the
/// transactions then running end within a few milliseconds.
pub(crate) const TIME_TO_HOLD_NEW_TRANSACTIONS: Duration = Duration::from_millis(100);

/// How long an MVCC database whose running transactions outlasted
/// [`TIME_TO_HOLD_NEW_TRANSACTIONS`] is left alone before the keeper holds
/// new transactions back again, so that a long transaction costs the other
/// sessions one short wait a second rather than one after every statement.
pub(crate) const PAUSE_AFTER_A_LONG_TRANSACTION: Duration = Duration::from_secs(1);

/// 64 MiB of 4 KiB pages.
pub(crate) const WAL_FRAMES_BEFORE_TRUNCATING: u64 = 16_384;

/// At most how many times the keeper writes, without holding new
/// transactions back, the rows committed while it wrote the last ones.
/// Each time writes fewer, and what is left is written while no
/// transaction runs. Measured with sysbench `oltp_insert` at 64 threads,
/// writing 17,000 rows the first time took 45 to 160 ms, in which
/// sessions committed another 17,000 index and table rows, which then
/// took 40 ms to write while new transactions waited.
pub(crate) const TIMES_TO_WRITE_THE_ROWS_COMMITTED_MEANWHILE: usize = 3;

/// Below how many rows written at once the keeper stops writing the rows
/// committed meanwhile and holds new transactions back.
pub(crate) const ROWS_FEW_ENOUGH_TO_WRITE_WHILE_HOLDING: usize = 1_024;

/// How often the keeper syncs the records an MVCC database's counters wrote
/// without a sync. InnoDB writes the counter into its redo log, which MySQL
/// writes to disk about once a second even when no transaction commits, so a
/// power loss there hands out again at most about a second's worth of the
/// numbers no committed row holds. The same holds here.
pub(crate) const COUNTER_RECORD_SYNC_INTERVAL: Duration = Duration::from_secs(1);

fn keep(received: Receiver<Request>, handle: WalKeeperHandle) {
    let mut left_busy: Vec<(Weak<Database>, Instant)> = Vec::new();
    let mut counter_records = CounterRecordSyncs::default();
    loop {
        let waited = match counter_records.time_left(Instant::now()) {
            Some(left) => received.recv_timeout(left),
            None => received.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        if let Err(error) = counter_records.sync_if_due(Instant::now()) {
            *handle
                .failure
                .lock()
                .expect("WAL keeper failure mutex poisoned") = Some(error);
        }
        let request = match waited {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Every statement past the bound asks until the WAL is emptied, so
        // the requests that piled up meanwhile are read together and each
        // database is emptied once.
        let mut databases: Vec<(Weak<Database>, Option<DatabaseUser>)> = Vec::new();
        let mut stop = false;
        #[cfg(test)]
        let mut answers = Vec::new();
        for request in std::iter::once(request).chain(received.try_iter()) {
            match request {
                Request::Truncate(database, user) => {
                    if !databases.iter().any(|(kept, _)| kept.ptr_eq(&database)) {
                        databases.push((database, user));
                    }
                }
                Request::SyncCounterRecords(marks, io) => {
                    counter_records.keep(marks, io, Instant::now());
                }
                #[cfg(test)]
                Request::Answer(answer) => answers.push(answer),
                Request::Stop => stop = true,
            }
        }
        left_busy
            .retain(|(database, until)| database.strong_count() > 0 && Instant::now() < *until);
        for (weak_database, user) in databases {
            if left_busy
                .iter()
                .any(|(busy, _)| busy.ptr_eq(&weak_database))
            {
                continue;
            }
            let Some(database) = weak_database.upgrade() else {
                continue;
            };
            if user
                .as_ref()
                .is_some_and(|user| user.start_using(std::time::Duration::ZERO).is_err())
            {
                continue;
            }
            #[cfg(test)]
            handle
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (truncated, pause) = if database.mvcc_enabled() {
                (
                    checkpoint_the_mvcc_log(&database, user.as_ref()),
                    PAUSE_AFTER_A_LONG_TRANSACTION,
                )
            } else {
                let truncated = truncate(&database, || {
                    #[cfg(test)]
                    handle
                        .refusals
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                });
                (truncated, PAUSE_AFTER_A_BUSY_WAL)
            };
            drop(user);
            match truncated {
                Ok(Emptied::Yes) => {
                    #[cfg(test)]
                    handle
                        .truncated
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(Emptied::KeptBusy) => left_busy.push((weak_database, Instant::now() + pause)),
                Err(error) => {
                    *handle
                        .failure
                        .lock()
                        .expect("WAL keeper failure mutex poisoned") = Some(error);
                }
            }
        }
        #[cfg(test)]
        for answer in answers {
            let _ = answer.send(());
        }
        if stop {
            break;
        }
    }
    let _ = counter_records.sync_now();
}

/// The counters' sidecars the keeper syncs, and when it syncs them next.
#[derive(Default)]
struct CounterRecordSyncs {
    sidecars: Vec<(Weak<CommitLoggedMarks>, Arc<dyn IO>)>,
    next_sync: Option<Instant>,
}

impl CounterRecordSyncs {
    fn keep(&mut self, marks: Weak<CommitLoggedMarks>, io: Arc<dyn IO>, now: Instant) {
        self.sidecars.retain(|(kept, _)| kept.strong_count() > 0);
        if !self.sidecars.iter().any(|(kept, _)| kept.ptr_eq(&marks)) {
            self.sidecars.push((marks, io));
        }
        self.next_sync
            .get_or_insert(now + COUNTER_RECORD_SYNC_INTERVAL);
    }

    fn time_left(&self, now: Instant) -> Option<Duration> {
        self.next_sync
            .map(|next_sync| next_sync.saturating_duration_since(now))
    }

    fn sync_if_due(&mut self, now: Instant) -> Result<()> {
        if self.next_sync.is_none_or(|next_sync| now < next_sync) {
            return Ok(());
        }
        let synced = self.sync_now();
        self.next_sync = (!self.sidecars.is_empty()).then(|| now + COUNTER_RECORD_SYNC_INTERVAL);
        synced
    }

    /// Syncs every sidecar and answers the first failure. A sidecar whose
    /// sync failed refuses every later one, the checkpoint's included, so
    /// that the logical log keeps the marks.
    fn sync_now(&mut self) -> Result<()> {
        self.sidecars.retain(|(marks, _)| marks.strong_count() > 0);
        let mut synced = Ok(());
        for (marks, io) in &self.sidecars {
            let Some(marks) = marks.upgrade() else {
                continue;
            };
            let sidecar_synced = sync_written_records(&marks, io.as_ref());
            if synced.is_ok() {
                synced = sidecar_synced;
            }
        }
        synced
    }
}

fn sync_written_records(marks: &CommitLoggedMarks, io: &dyn IO) -> Result<()> {
    match marks.sync_written_records()? {
        Some(completion) => io.wait_for_completion(completion),
        None => Ok(()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Emptied {
    Yes,
    KeptBusy,
}

/// Empties one database's WAL over a connection of its own. A session that
/// holds the write lock keeps the WAL busy until it lets go, which the keeper
/// waits for during [`TIME_TO_WAIT_FOR_A_BUSY_WAL`]. A session reading a
/// snapshot older than the WAL keeps it busy until that snapshot ends, which
/// the keeper does not wait for; a later request tries again.
///
/// The frames are first copied into the database file without the write
/// lock, so the truncation, which holds it, has little left to copy.
fn truncate(database: &Arc<Database>, on_refusal: impl Fn()) -> Result<Emptied> {
    let connection = database.connect()?;
    let result = copy_then_truncate(&connection, on_refusal);
    connection.close()?;
    result
}

fn copy_then_truncate(
    connection: &Arc<turso_core::Connection>,
    on_refusal: impl Fn(),
) -> Result<Emptied> {
    let copied = copy_into_the_database_file(connection)?;
    let started = Instant::now();
    loop {
        match connection.checkpoint(CheckpointMode::Truncate {
            upper_bound_inclusive: None,
        }) {
            Ok(_) => return Ok(Emptied::Yes),
            Err(LimboError::Busy) | Err(LimboError::BusySnapshot) => on_refusal(),
            Err(error) => return Err(error),
        }
        if copied.is_some_and(|wal_end| a_snapshot_ends_before(connection, wal_end)) {
            return Ok(Emptied::KeptBusy);
        }
        let Some(left) = TIME_TO_WAIT_FOR_A_BUSY_WAL.checked_sub(started.elapsed()) else {
            return Ok(Emptied::KeptBusy);
        };
        if connection.get_pager().wait_for_lock_release(left).is_none() {
            return Ok(Emptied::KeptBusy);
        }
    }
}

/// Checkpoints an MVCC database, which empties its logical log and its WAL,
/// over a connection of its own.
///
/// The checkpoint first writes the rows committed so far while the
/// sessions keep running, then holds new transactions back, answering
/// `Busy` to the sessions' busy handlers, while the running ones end, for
/// at most [`TIME_TO_HOLD_NEW_TRANSACTIONS`]; it tries each time a
/// transaction on the database ends. Once none runs it writes what
/// committed meanwhile and commits it all. The sweep of the row versions
/// in memory, which needs no transaction held back, runs after they may
/// begin again. Measured with sysbench `oltp_write_only` at 8 threads,
/// that takes the writes of the rows (21 ms) and the sweep (30 ms) out of
/// the 78 ms new transactions used to wait for one checkpoint.
pub(crate) fn checkpoint_the_mvcc_log(
    database: &Arc<Database>,
    user: Option<&DatabaseUser>,
) -> Result<Emptied> {
    let store = database
        .get_mv_store()
        .clone()
        .expect("an MVCC database has a store");
    let connection = database.connect()?;
    if !an_mvcc_checkpoint_is_due(&connection)? {
        connection.close()?;
        return Ok(Emptied::Yes);
    }
    let checkpointed = match connection.begin_mvcc_checkpoint_writing_rows_first() {
        Ok(mut checkpoint) => match write_the_rows_committed_meanwhile(&mut checkpoint) {
            Ok(()) => {
                store.hold_new_transactions();
                let finished = finish_once_no_transaction_runs(&mut checkpoint, user);
                store.let_new_transactions_begin();
                finished
            }
            Err(error) => Err(error),
        },
        Err(LimboError::Busy) | Err(LimboError::BusySnapshot) => Ok(Emptied::KeptBusy),
        Err(error) => Err(error),
    };
    if matches!(checkpointed, Ok(Emptied::Yes)) {
        store.sweep_what_the_checkpoint_left();
    }
    connection.close()?;
    checkpointed
}

/// Whether an MVCC database's logical log grew past the engine's
/// checkpoint threshold, or its WAL past [`WAL_FRAMES_BEFORE_TRUNCATING`].
/// Sessions ask the keeper after every statement while it is, so the keeper
/// reads it again before each checkpoint: the requests sent while it ran the
/// last one would each run another.
pub(crate) fn an_mvcc_checkpoint_is_due(connection: &turso_core::Connection) -> Result<bool> {
    let log_past_its_bound = connection.mv_store().as_ref().is_some_and(|store| {
        u64::try_from(store.checkpoint_threshold())
            .is_ok_and(|threshold| store.logical_log_offset() >= threshold)
    });
    Ok(log_past_its_bound || connection.wal_state()?.max_frame > WAL_FRAMES_BEFORE_TRUNCATING)
}

fn write_the_rows_committed_meanwhile(
    checkpoint: &mut MvccCheckpointWritingRowsFirst,
) -> Result<()> {
    let mut written = checkpoint.rows_written();
    for _ in 0..TIMES_TO_WRITE_THE_ROWS_COMMITTED_MEANWHILE {
        if written < ROWS_FEW_ENOUGH_TO_WRITE_WHILE_HOLDING {
            break;
        }
        written = checkpoint.write_the_rows_committed_since()?;
    }
    Ok(())
}

fn finish_once_no_transaction_runs(
    checkpoint: &mut MvccCheckpointWritingRowsFirst,
    user: Option<&DatabaseUser>,
) -> Result<Emptied> {
    let held_until = Instant::now() + TIME_TO_HOLD_NEW_TRANSACTIONS;
    loop {
        let ended_before = user.map(DatabaseUser::transactions_ended);
        match checkpoint.finish_once_no_transaction_runs() {
            Ok(Some(_)) => return Ok(Emptied::Yes),
            Ok(None) => {}
            Err(LimboError::Busy) | Err(LimboError::BusySnapshot) => return Ok(Emptied::KeptBusy),
            Err(error) => return Err(error),
        }
        if Instant::now() >= held_until {
            return Ok(Emptied::KeptBusy);
        }
        match (user, ended_before) {
            (Some(user), Some(ended_before)) => {
                user.wait_for_a_transaction_to_end(ended_before, Some(held_until));
            }
            _ => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Copies what it can into the database file without the write lock, and
/// answers the last frame the WAL held when it was done, or `None` when
/// another checkpoint kept it from starting.
fn copy_into_the_database_file(connection: &Arc<turso_core::Connection>) -> Result<Option<u64>> {
    match connection.checkpoint(CheckpointMode::Passive {
        upper_bound_inclusive: None,
    }) {
        Ok(copied) => Ok(Some(copied.wal_max_frame)),
        Err(LimboError::Busy) | Err(LimboError::BusySnapshot) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether a session reads a snapshot that ends before `wal_end`. The frames
/// after its end cannot be copied into the database file while it reads, so
/// waiting for the write lock cannot empty the WAL.
fn a_snapshot_ends_before(connection: &Arc<turso_core::Connection>, wal_end: u64) -> bool {
    connection
        .get_pager()
        .min_pinned_read_frame()
        .is_some_and(|snapshot_end| snapshot_end < wal_end)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use turso_core::io::{FileId, FileSyncType};
    use turso_core::storage::auto_increment::{
        AllocatorDatabaseIdentity, AllocatorOpenMode, AutoIncrementKey, DurableRangeAllocator,
    };
    use turso_core::{
        Buffer, Clock, Completion, File, IOExt as _, MemoryIO, MonotonicInstant, OpenFlags,
        WallClockInstant,
    };

    use super::*;

    #[test]
    fn counter_records_are_synced_once_a_second_has_passed_and_only_then() {
        let io = Arc::new(SyncCountingIo::default());
        let allocator = DurableRangeAllocator::open(
            io.as_ref(),
            "counter.sidecar",
            AllocatorDatabaseIdentity::new(*b"keeper-database1").unwrap(),
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )
        .unwrap();
        let marks = allocator.let_commits_log_marks().unwrap();
        let key = AutoIncrementKey::new(*b"keeper-counter-1").unwrap();
        let take_a_number = || {
            let mut reservation = allocator.reserve(key, 1).unwrap();
            io.block(|| reservation.step()).unwrap();
        };
        let mut syncs = CounterRecordSyncs::default();
        let started = Instant::now();
        assert_eq!(syncs.time_left(started), None);
        syncs.keep(Arc::downgrade(&marks), io.clone(), started);
        assert_eq!(syncs.time_left(started), Some(COUNTER_RECORD_SYNC_INTERVAL));

        for _ in 0..20 {
            take_a_number();
        }
        let after_the_header = io.syncs();
        syncs
            .sync_if_due(started + Duration::from_millis(999))
            .unwrap();
        assert_eq!(io.syncs(), after_the_header);
        assert!(marks.has_unsynced_records());

        syncs
            .sync_if_due(started + COUNTER_RECORD_SYNC_INTERVAL)
            .unwrap();
        assert_eq!(io.syncs(), after_the_header + 1);
        assert!(!marks.has_unsynced_records());
        assert_eq!(
            syncs.time_left(started + COUNTER_RECORD_SYNC_INTERVAL),
            Some(COUNTER_RECORD_SYNC_INTERVAL)
        );

        syncs
            .sync_if_due(started + 2 * COUNTER_RECORD_SYNC_INTERVAL)
            .unwrap();
        assert_eq!(io.syncs(), after_the_header + 1);

        take_a_number();
        syncs
            .sync_if_due(started + 3 * COUNTER_RECORD_SYNC_INTERVAL)
            .unwrap();
        assert_eq!(io.syncs(), after_the_header + 2);

        drop(marks);
        syncs
            .sync_if_due(started + 4 * COUNTER_RECORD_SYNC_INTERVAL)
            .unwrap();
        assert_eq!(syncs.time_left(started), None);
    }

    struct SyncCountingIo {
        inner: MemoryIO,
        syncs: Arc<AtomicUsize>,
    }

    impl Default for SyncCountingIo {
        fn default() -> Self {
            Self {
                inner: MemoryIO::new(),
                syncs: Arc::default(),
            }
        }
    }

    impl SyncCountingIo {
        fn syncs(&self) -> usize {
            self.syncs.load(Ordering::SeqCst)
        }
    }

    impl Clock for SyncCountingIo {
        fn current_time_monotonic(&self) -> MonotonicInstant {
            self.inner.current_time_monotonic()
        }

        fn current_time_wall_clock(&self) -> WallClockInstant {
            self.inner.current_time_wall_clock()
        }
    }

    impl IO for SyncCountingIo {
        fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
            Ok(Arc::new(SyncCountingFile {
                inner: self.inner.open_file(path, flags, direct)?,
                syncs: self.syncs.clone(),
            }))
        }

        fn remove_file(&self, path: &str) -> Result<()> {
            self.inner.remove_file(path)
        }
    }

    struct SyncCountingFile {
        inner: Arc<dyn File>,
        syncs: Arc<AtomicUsize>,
    }

    impl File for SyncCountingFile {
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
            self.syncs.fetch_add(1, Ordering::SeqCst);
            self.inner.sync(completion, sync_type)
        }

        fn size(&self) -> Result<u64> {
            self.inner.size()
        }

        fn truncate(&self, len: u64, completion: Completion) -> Result<Completion> {
            self.inner.truncate(len, completion)
        }
    }
}
