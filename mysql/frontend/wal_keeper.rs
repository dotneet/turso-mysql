//! Empties a database's WAL on a thread of its own.
//!
//! A write large enough leaves the WAL holding more than the engine's own
//! checkpoint empties, and emptying it copies every frame into the database
//! and syncs it: measured, about two seconds after an UPDATE of 128,000 rows.
//! Done inside the statement that crossed the bound, that time lands on one
//! client's answer. So a session only asks, and this thread does the work over
//! a connection of its own, which leaves the session free to run its next
//! statement meanwhile.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use turso_core::{CheckpointMode, Database, LimboError, Result};

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

fn keep(received: Receiver<Request>, handle: WalKeeperHandle) {
    let mut left_busy: Vec<(Weak<Database>, Instant)> = Vec::new();
    while let Ok(request) = received.recv() {
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
            return;
        }
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
/// The checkpoint runs only once no transaction is open, so new ones are
/// held back, answering `Busy` to the sessions' busy handlers, while the
/// running ones end, for at most [`TIME_TO_HOLD_NEW_TRANSACTIONS`]. The
/// keeper tries again each time a transaction on the database ends.
pub(crate) fn checkpoint_the_mvcc_log(
    database: &Arc<Database>,
    user: Option<&DatabaseUser>,
) -> Result<Emptied> {
    let store = database
        .get_mv_store()
        .clone()
        .expect("an MVCC database has a store");
    let connection = database.connect()?;
    store.hold_new_transactions();
    let held_until = Instant::now() + TIME_TO_HOLD_NEW_TRANSACTIONS;
    let checkpointed = loop {
        let ended_before = user.map(DatabaseUser::transactions_ended);
        match connection.checkpoint(CheckpointMode::Truncate {
            upper_bound_inclusive: None,
        }) {
            Ok(_) => break Ok(Emptied::Yes),
            Err(LimboError::Busy) | Err(LimboError::BusySnapshot) => {}
            Err(error) => break Err(error),
        }
        if Instant::now() >= held_until {
            break Ok(Emptied::KeptBusy);
        }
        match (user, ended_before) {
            (Some(user), Some(ended_before)) => {
                user.wait_for_a_transaction_to_end(ended_before, Some(held_until));
            }
            _ => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    store.let_new_transactions_begin();
    connection.close()?;
    checkpointed
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
