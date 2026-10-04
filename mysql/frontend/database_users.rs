//! Who is using one opened database, so that a `DROP DATABASE` waits for
//! them and nobody uses the database once it is gone.
//!
//! Measured on MySQL 8.4.11: a `DROP DATABASE` waits for a session running a
//! statement on the database or holding a transaction that read one of its
//! tables, and a statement another session starts while the drop waits waits
//! behind it. A session that merely has the database selected holds nothing
//! up; once the database is gone, its statements on it answer 1049.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long a statement waits for a database or a table another session is
/// using before answering 1205: MySQL's `lock_wait_timeout` starts at a year.
pub const DEFAULT_METADATA_LOCK_WAIT: Duration = Duration::from_secs(31_536_000);

/// The connections using one opened database, which every connection to it
/// and the catalog that drops it share.
#[derive(Default)]
pub(crate) struct DatabaseUsers {
    state: Mutex<UsersState>,
    changed: Condvar,
    dropped: AtomicBool,
    /// How many transactions on the database have ended, which a statement
    /// that met another transaction's uncommitted write waits on to change.
    transactions_ended: AtomicU64,
    waiting_for_a_transaction_to_end: AtomicUsize,
    transaction_end_wait: Mutex<()>,
    a_transaction_ended: Condvar,
    #[cfg(test)]
    wakeups: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct UsersState {
    /// How many connections are running a statement on the database or hold
    /// a transaction open on it.
    using: usize,
    /// Set while a `DROP DATABASE` waits for them or removes the database.
    dropping: bool,
}

/// Why a `DROP DATABASE` did not get to remove the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropWaitError {
    /// Another `DROP DATABASE` removed it first.
    AlreadyDropped,
    /// A connection kept using it for longer than the drop waits.
    TimedOut,
}

impl DatabaseUsers {
    /// Waits until no connection uses the database, then keeps every new
    /// statement on it waiting while the caller removes it.
    ///
    /// A drop that another drop of the same database is already making waits
    /// for that one to finish first.
    pub(crate) fn wait_to_drop(&self, wait: Duration) -> Result<DropInProgress<'_>, DropWaitError> {
        let deadline = Instant::now().checked_add(wait);
        let mut state = self.lock();
        while state.dropping {
            state = self.wait_until(state, deadline)?;
        }
        if self.dropped.load(Ordering::SeqCst) {
            return Err(DropWaitError::AlreadyDropped);
        }
        state.dropping = true;
        while state.using > 0 {
            state = match self.wait_until(state, deadline) {
                Ok(state) => state,
                Err(error) => {
                    // The statements waiting behind this drop go ahead as
                    // though it had never been asked for.
                    let mut state = self.lock();
                    state.dropping = false;
                    drop(state);
                    self.wake_the_waiters();
                    return Err(error);
                }
            };
        }
        Ok(DropInProgress {
            users: self,
            finished: false,
        })
    }

    fn wait_until<'a>(
        &'a self,
        state: MutexGuard<'a, UsersState>,
        deadline: Option<Instant>,
    ) -> Result<MutexGuard<'a, UsersState>, DropWaitError> {
        let Some(deadline) = deadline else {
            return Ok(self
                .changed
                .wait(state)
                .expect("MySQL database users mutex poisoned"));
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(DropWaitError::TimedOut);
        }
        Ok(self
            .changed
            .wait_timeout(state, left)
            .expect("MySQL database users mutex poisoned")
            .0)
    }

    fn lock(&self) -> MutexGuard<'_, UsersState> {
        self.state
            .lock()
            .expect("MySQL database users mutex poisoned")
    }

    fn wake_the_waiters(&self) {
        #[cfg(test)]
        self.wakeups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.changed.notify_all();
    }
}

/// A `DROP DATABASE` that has waited for every user and keeps new ones out.
///
/// Letting it go without [`DropInProgress::finish`] lets them back in, which
/// is what a drop that failed has to do.
pub(crate) struct DropInProgress<'a> {
    users: &'a DatabaseUsers,
    finished: bool,
}

impl DropInProgress<'_> {
    /// Records that the database is gone, which every statement waiting on it
    /// and every later one answers 1049 for.
    pub(crate) fn finish(mut self) {
        let mut state = self.users.lock();
        self.users.dropped.store(true, Ordering::SeqCst);
        state.dropping = false;
        drop(state);
        self.finished = true;
        self.users.wake_the_waiters();
    }
}

impl Drop for DropInProgress<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.users.lock();
        state.dropping = false;
        drop(state);
        self.users.wake_the_waiters();
    }
}

/// Why a statement did not get to start on its database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlStatementNotStarted {
    /// Another session dropped the database: MySQL's 1049.
    Dropped(MySqlDatabaseDropped),
    /// A `DROP DATABASE` kept the database for longer than the statement
    /// waits: MySQL's 1205.
    WaitTimedOut,
}

/// A statement asked for a database another session dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlDatabaseDropped {
    /// The database's name, which MySQL's 1049 names.
    pub database: String,
}

impl fmt::Display for MySqlDatabaseDropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unknown database '{}'", self.database)
    }
}

impl std::error::Error for MySqlDatabaseDropped {}

/// One connection's use of its database, shared by every clone of the
/// connection.
pub(crate) struct DatabaseUser {
    users: Arc<DatabaseUsers>,
    database: String,
    using: Mutex<Use>,
}

/// How long a connection counts as using its database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    Not,
    /// Until the statement running now ends.
    ForTheStatement,
    /// Until the transaction ends, because it read the database.
    ForTheTransaction,
}

impl DatabaseUser {
    pub(crate) fn new(users: Arc<DatabaseUsers>, database: String) -> Self {
        Self {
            users,
            database,
            using: Mutex::new(Use::Not),
        }
    }

    /// Counts this connection as using its database until
    /// [`DatabaseUser::stop_using`], waiting at most `wait` first for a drop
    /// in progress.
    ///
    /// Measured on MySQL 8.4.11: a statement on a database a `DROP DATABASE`
    /// is waiting to drop waits the session's own `lock_wait_timeout`, and
    /// answers 1205 once it runs out.
    pub(crate) fn start_using(&self, wait: Duration) -> Result<(), MySqlStatementNotStarted> {
        let mut using = self.lock_using();
        if *using != Use::Not {
            return Ok(());
        }
        let deadline = Instant::now().checked_add(wait);
        let mut state = self.users.lock();
        loop {
            if self.users.dropped.load(Ordering::SeqCst) {
                return Err(MySqlStatementNotStarted::Dropped(MySqlDatabaseDropped {
                    database: self.database.clone(),
                }));
            }
            if !state.dropping {
                break;
            }
            state = self
                .users
                .wait_until(state, deadline)
                .map_err(|_| MySqlStatementNotStarted::WaitTimedOut)?;
        }
        state.using = state
            .using
            .checked_add(1)
            .expect("MySQL database user count must not overflow");
        *using = Use::ForTheStatement;
        Ok(())
    }

    /// Keeps counting this connection until its transaction ends, which
    /// [`DatabaseUser::stop_using`] is told of.
    ///
    /// Measured on MySQL 8.4.11: a `DROP DATABASE` waits for a transaction
    /// that read one of the database's tables, and not for one that has only
    /// begun, taken a savepoint or run `SELECT 1`.
    pub(crate) fn keep_for_the_transaction(&self) {
        let mut using = self.lock_using();
        assert_ne!(
            *using,
            Use::Not,
            "only a connection running a statement on its database can keep it"
        );
        *using = Use::ForTheTransaction;
    }

    /// Stops counting this connection once its statement ends, unless its
    /// transaction read the database before.
    pub(crate) fn stop_using_unless_the_transaction_keeps_it(&self) {
        let mut using = self.lock_using();
        if *using == Use::ForTheTransaction {
            return;
        }
        self.stop_counting(&mut using);
    }

    pub(crate) fn stop_using(&self) {
        self.stop_counting(&mut self.lock_using());
    }

    fn stop_counting(&self, using: &mut Use) {
        if *using == Use::Not {
            return;
        }
        let mut state = self.users.lock();
        state.using = state
            .using
            .checked_sub(1)
            .expect("a counted MySQL database user was counted once");
        let a_drop_waits_for_the_users = state.dropping;
        drop(state);
        *using = Use::Not;
        if a_drop_waits_for_the_users {
            self.users.wake_the_waiters();
        }
    }

    /// A user of the same database that counts apart from this one, for
    /// work done on the database beside the connection's own statements.
    pub(crate) fn another_on_the_same_database(&self) -> DatabaseUser {
        DatabaseUser::new(Arc::clone(&self.users), self.database.clone())
    }

    pub(crate) fn database_was_dropped(&self) -> bool {
        self.users.dropped.load(Ordering::SeqCst)
    }

    pub(crate) fn transactions_ended(&self) -> u64 {
        self.users.transactions_ended.load(Ordering::SeqCst)
    }

    pub(crate) fn note_a_transaction_ended(&self) {
        self.users.transactions_ended.fetch_add(1, Ordering::SeqCst);
        if self
            .users
            .waiting_for_a_transaction_to_end
            .load(Ordering::SeqCst)
            == 0
        {
            return;
        }
        drop(self.lock_transaction_end_wait());
        self.users.a_transaction_ended.notify_all();
    }

    /// Waits until a transaction on the database ends after `ended_before`
    /// was read from [`DatabaseUser::transactions_ended`], answering false
    /// once `deadline` passes first.
    pub(crate) fn wait_for_a_transaction_to_end(
        &self,
        ended_before: u64,
        deadline: Option<Instant>,
    ) -> bool {
        self.users
            .waiting_for_a_transaction_to_end
            .fetch_add(1, Ordering::SeqCst);
        let ended = self.wait_while_no_transaction_ends(ended_before, deadline);
        self.users
            .waiting_for_a_transaction_to_end
            .fetch_sub(1, Ordering::SeqCst);
        ended
    }

    fn wait_while_no_transaction_ends(&self, ended_before: u64, deadline: Option<Instant>) -> bool {
        let mut wait = self.lock_transaction_end_wait();
        while self.users.transactions_ended.load(Ordering::SeqCst) == ended_before {
            wait = match deadline {
                None => self
                    .users
                    .a_transaction_ended
                    .wait(wait)
                    .expect("MySQL ended transactions mutex poisoned"),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    self.users
                        .a_transaction_ended
                        .wait_timeout(wait, left)
                        .expect("MySQL ended transactions mutex poisoned")
                        .0
                }
            };
        }
        true
    }

    fn lock_transaction_end_wait(&self) -> MutexGuard<'_, ()> {
        self.users
            .transaction_end_wait
            .lock()
            .expect("MySQL ended transactions mutex poisoned")
    }

    fn lock_using(&self) -> MutexGuard<'_, Use> {
        self.using
            .lock()
            .expect("MySQL database user mutex poisoned")
    }
}

impl Drop for DatabaseUser {
    fn drop(&mut self) {
        self.stop_using();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    const A_LONG_WAIT: Duration = Duration::from_secs(10);

    fn user(users: &Arc<DatabaseUsers>) -> DatabaseUser {
        DatabaseUser::new(Arc::clone(users), "reports".to_owned())
    }

    #[test]
    fn a_connection_that_uses_nothing_holds_no_drop_up() {
        let users = Arc::new(DatabaseUsers::default());
        let idle = user(&users);
        users.wait_to_drop(Duration::ZERO).unwrap().finish();
        assert!(idle.database_was_dropped());
        assert_eq!(
            idle.start_using(A_LONG_WAIT),
            Err(MySqlStatementNotStarted::Dropped(MySqlDatabaseDropped {
                database: "reports".to_owned()
            }))
        );
    }

    #[test]
    fn a_drop_gives_up_on_a_user_that_stays_and_lets_it_carry_on() {
        let users = Arc::new(DatabaseUsers::default());
        let busy = user(&users);
        busy.start_using(A_LONG_WAIT).unwrap();
        assert_eq!(
            users.wait_to_drop(Duration::from_millis(20)).err(),
            Some(DropWaitError::TimedOut)
        );
        busy.stop_using();
        busy.start_using(A_LONG_WAIT).unwrap();
        assert!(!busy.database_was_dropped());
    }

    #[test]
    fn a_drop_waits_for_its_user_and_a_new_statement_waits_for_the_drop() {
        let users = Arc::new(DatabaseUsers::default());
        let busy = Arc::new(user(&users));
        busy.start_using(A_LONG_WAIT).unwrap();
        let finisher = {
            let busy = Arc::clone(&busy);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                busy.stop_using();
            })
        };
        let dropping = users.wait_to_drop(Duration::from_secs(10)).unwrap();
        finisher.join().unwrap();
        let latecomer = {
            let latecomer = user(&users);
            thread::spawn(move || latecomer.start_using(A_LONG_WAIT))
        };
        thread::sleep(Duration::from_millis(50));
        dropping.finish();
        assert!(latecomer.join().unwrap().is_err());
    }

    #[test]
    fn a_statement_waiting_behind_a_drop_gives_up_after_its_own_wait() {
        let users = Arc::new(DatabaseUsers::default());
        let busy = user(&users);
        busy.start_using(A_LONG_WAIT).unwrap();
        let dropper = {
            let users = Arc::clone(&users);
            thread::spawn(move || users.wait_to_drop(A_LONG_WAIT).map(DropInProgress::finish))
        };
        while !users.lock().dropping {
            thread::yield_now();
        }
        let latecomer = user(&users);
        assert_eq!(
            latecomer.start_using(Duration::from_millis(20)),
            Err(MySqlStatementNotStarted::WaitTimedOut)
        );
        busy.stop_using();
        assert_eq!(dropper.join().unwrap(), Ok(()));
    }

    #[test]
    fn a_failed_drop_lets_the_statements_waiting_behind_it_go_ahead() {
        let users = Arc::new(DatabaseUsers::default());
        let dropping = users.wait_to_drop(Duration::ZERO).unwrap();
        let latecomer = {
            let latecomer = user(&users);
            thread::spawn(move || latecomer.start_using(A_LONG_WAIT))
        };
        thread::sleep(Duration::from_millis(50));
        drop(dropping);
        assert_eq!(latecomer.join().unwrap(), Ok(()));
    }

    #[test]
    fn a_second_drop_finds_the_database_already_gone() {
        let users = Arc::new(DatabaseUsers::default());
        users.wait_to_drop(Duration::ZERO).unwrap().finish();
        assert_eq!(
            users.wait_to_drop(Duration::ZERO).err(),
            Some(DropWaitError::AlreadyDropped)
        );
    }

    #[test]
    fn a_transaction_that_read_keeps_counting_until_it_ends() {
        let users = Arc::new(DatabaseUsers::default());
        let reader = user(&users);
        reader.start_using(A_LONG_WAIT).unwrap();
        reader.keep_for_the_transaction();
        reader.stop_using_unless_the_transaction_keeps_it();
        assert_eq!(
            users.wait_to_drop(Duration::ZERO).err(),
            Some(DropWaitError::TimedOut)
        );
        reader.stop_using();
        assert!(users.wait_to_drop(Duration::ZERO).is_ok());
    }

    #[test]
    fn a_statement_that_read_nothing_stops_counting_when_it_ends() {
        let users = Arc::new(DatabaseUsers::default());
        let idle = user(&users);
        idle.start_using(A_LONG_WAIT).unwrap();
        idle.stop_using_unless_the_transaction_keeps_it();
        assert!(users.wait_to_drop(Duration::ZERO).is_ok());
    }

    #[test]
    fn a_statement_ending_with_no_drop_waiting_wakes_nobody() {
        let users = Arc::new(DatabaseUsers::default());
        let busy = user(&users);
        for _ in 0..3 {
            busy.start_using(A_LONG_WAIT).unwrap();
            busy.stop_using();
        }
        assert_eq!(users.wakeups.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn a_transaction_ending_wakes_a_statement_waiting_for_one() {
        let users = Arc::new(DatabaseUsers::default());
        let waiting = user(&users);
        let ending = user(&users);
        let before = waiting.transactions_ended();
        let waiter = thread::spawn(move || {
            waiting.wait_for_a_transaction_to_end(before, Instant::now().checked_add(A_LONG_WAIT))
        });
        while users
            .waiting_for_a_transaction_to_end
            .load(Ordering::SeqCst)
            == 0
        {
            thread::yield_now();
        }
        ending.note_a_transaction_ended();
        assert!(waiter.join().unwrap());
        assert_eq!(ending.transactions_ended(), before + 1);
        assert_eq!(
            users
                .waiting_for_a_transaction_to_end
                .load(Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn a_statement_stops_waiting_for_a_transaction_end_at_its_deadline() {
        let users = Arc::new(DatabaseUsers::default());
        let waiting = user(&users);
        let before = waiting.transactions_ended();
        assert!(!waiting.wait_for_a_transaction_to_end(
            before,
            Instant::now().checked_add(Duration::from_millis(20))
        ));
        waiting.note_a_transaction_ended();
        assert!(waiting.wait_for_a_transaction_to_end(before, Some(Instant::now())));
    }

    #[test]
    fn a_transaction_ending_with_nobody_waiting_wakes_nobody() {
        let users = Arc::new(DatabaseUsers::default());
        let held = users.transaction_end_wait.lock().unwrap();
        let ending = user(&users);
        let ended = thread::spawn(move || ending.note_a_transaction_ended());
        ended.join().unwrap();
        drop(held);
        assert_eq!(user(&users).transactions_ended(), 1);
    }

    #[test]
    fn a_user_let_go_stops_counting() {
        let users = Arc::new(DatabaseUsers::default());
        let busy = user(&users);
        busy.start_using(A_LONG_WAIT).unwrap();
        drop(busy);
        assert!(users.wait_to_drop(Duration::ZERO).is_ok());
    }
}
