//! What a transaction sees of what other sessions commit while it runs.
//!
//! The engine gives an explicit transaction one read snapshot, taken at its
//! first read and held until it ends, and one write lock over the whole
//! database, taken at its first write. MySQL's two levels come out of those
//! two facts:
//!
//! - `REPEATABLE READ` is the snapshot held for the whole transaction.
//! - `READ COMMITTED` lets the snapshot go at every statement until the
//!   transaction writes. After that nothing else can commit, so the snapshot
//!   it holds is already the latest one.
//! - `SERIALIZABLE` is kept the way `REPEATABLE READ` is, which already gives
//!   it: every write waits for the one write lock and is refused when another
//!   session committed since the transaction's snapshot, so the transactions
//!   that write run as if one after another in the order they commit, and one
//!   that only reads sees the database as one of them left it.
//!
//! What `REPEATABLE READ` cannot give is MySQL's current read. Measured on
//! MySQL 8.4.11: a transaction that read before another session committed can
//! still update a row afterwards, and a later read of a row it did not touch
//! still sees the old value. A snapshot here is the whole database at one
//! moment, so it cannot mix rows from two moments. Such a write fails with
//! the engine's stale-snapshot error instead, which the server answers the way
//! MySQL answers a transaction it cannot finish: 1213, with the transaction
//! rolled back.
//!
//! In MVCC mode (`TURSO_MYSQL_EXPERIMENTAL_MVCC`) writers do not wait for one
//! another, so the levels are kept by moving the transaction's snapshot to
//! the latest commit before a statement, which keeps the rows the transaction
//! wrote:
//!
//! - `READ COMMITTED` moves it before every statement, written or not.
//! - `REPEATABLE READ` moves it before every statement until the first plain
//!   `SELECT` of a table, and holds it from then on. Measured on MySQL
//!   8.4.11, that `SELECT` is where InnoDB takes the read view: writes,
//!   locking reads, savepoints and prepares before it do not take it. `WITH
//!   CONSISTENT SNAPSHOT` holds the snapshot `BEGIN` took.
//! - `SERIALIZABLE` begins with the engine's plain `BEGIN` instead of `BEGIN
//!   CONCURRENT`, and its snapshot is never moved: it reads from its first
//!   statement and takes the database's one exclusive write slot at its first
//!   write, which is refused with 1213 once another transaction has
//!   committed since it began reading.

use super::*;

/// A transaction isolation level this server can keep.
///
/// `READ UNCOMMITTED` would need other sessions' unwritten rows, so it is not
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MySqlIsolationLevel {
    ReadCommitted,
    /// MySQL's default level.
    #[default]
    RepeatableRead,
    /// Run as `REPEATABLE READ` is, or in MVCC mode as an exclusive
    /// transaction; see the module's notes.
    Serializable,
}

impl MySqlIsolationLevel {
    /// Reads a level the way `SET TRANSACTION ISOLATION LEVEL` spells it, or
    /// the way `@@transaction_isolation` does, with a hyphen for the space.
    pub fn from_name(name: &str) -> Option<Self> {
        let words = name.replace('-', " ");
        let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
        if words.eq_ignore_ascii_case("READ COMMITTED") {
            Some(Self::ReadCommitted)
        } else if words.eq_ignore_ascii_case("REPEATABLE READ") {
            Some(Self::RepeatableRead)
        } else if words.eq_ignore_ascii_case("SERIALIZABLE") {
            Some(Self::Serializable)
        } else {
            None
        }
    }

    /// The level as `@@transaction_isolation` reads it back.
    pub const fn variable_value(self) -> &'static str {
        match self {
            Self::ReadCommitted => "READ-COMMITTED",
            Self::RepeatableRead => "REPEATABLE-READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }
}

/// The isolation of the transaction a session is in, and of the next one.
#[derive(Debug, Default)]
pub(super) struct TransactionIsolation {
    /// The level of the transaction the session is in, or last began.
    current: MySqlIsolationLevel,
    /// The level the next transaction begins at, as the client last asked.
    next: MySqlIsolationLevel,
    /// Whether the statement running now began a transaction.
    began_transaction: bool,
    /// Whether the transaction has made its first consistent read, the
    /// plain `SELECT` of a table where MySQL fixes what a `REPEATABLE READ`
    /// transaction reads from then on.
    read_view_taken: bool,
}

/// What a transaction command left for the client to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MySqlTransactionOutcome {
    /// `WITH CONSISTENT SNAPSHOT` was asked for at a level that has none.
    ///
    /// Measured on MySQL 8.4.11: warning 138 under `READ COMMITTED`, and the
    /// transaction begins all the same.
    pub consistent_snapshot_ignored: bool,
}

impl MySqlConnection {
    /// Readies the session for one statement from the client.
    ///
    /// `next` is the level a transaction the statement begins will run at,
    /// `session_read_only` whether one begun without saying is read-only, and
    /// `written_zero` what a 0 written into a counted column means under the
    /// session's `sql_mode`. Inside a `READ COMMITTED` transaction the
    /// snapshot the last statement read from is let go, so this one reads what
    /// is committed now. In MVCC mode the snapshot is moved to now instead,
    /// for `READ COMMITTED` and for a `REPEATABLE READ` transaction that has
    /// not made its first consistent read; a transaction that wrote before
    /// another session changed the schema is rolled back with 1213 there,
    /// since its writes could no longer commit.
    pub fn prepare_for_client_statement(
        &self,
        next: MySqlIsolationLevel,
        session_read_only: bool,
        written_zero: WrittenZero,
    ) -> std::result::Result<(), MySqlQueryError> {
        *self.written_zero.lock().unwrap() = written_zero;
        *self.session_read_only.lock().unwrap() = session_read_only;
        let (current, read_view_taken) = {
            let mut isolation = self.transaction_isolation.lock().unwrap();
            isolation.next = next;
            isolation.began_transaction = false;
            (isolation.current, isolation.read_view_taken)
        };
        if self.inner.get_auto_commit() {
            return Ok(());
        }
        if !self.inner.mvcc_enabled() {
            if current == MySqlIsolationLevel::ReadCommitted {
                self.inner
                    .release_read_snapshot()
                    .map_err(MySqlQueryError::Engine)?;
            }
            return Ok(());
        }
        let reads_afresh = match current {
            MySqlIsolationLevel::ReadCommitted => true,
            MySqlIsolationLevel::RepeatableRead => !read_view_taken,
            MySqlIsolationLevel::Serializable => false,
        };
        if !reads_afresh {
            return Ok(());
        }
        match self.inner.refresh_read_snapshot() {
            Err(LimboError::SchemaConflict) => {
                self.roll_back_after_serialization_failure()?;
                Err(MySqlQueryError::Engine(LimboError::SchemaConflict))
            }
            refreshed => refreshed.map_err(MySqlQueryError::Engine),
        }
    }

    /// Records that the statement running now is a consistent read of a
    /// table: a plain `SELECT`, not one that locks the rows it reads.
    ///
    /// Measured on MySQL 8.4.11: a `REPEATABLE READ` transaction takes its
    /// read view at its first such read. A write, a `SELECT ... FOR UPDATE`
    /// or `FOR SHARE`, a `SAVEPOINT`, a `SELECT` of no table and a prepare
    /// before it all leave the transaction reading what other sessions
    /// commit.
    pub(super) fn note_consistent_read(&self) {
        self.transaction_isolation.lock().unwrap().read_view_taken = true;
    }

    /// Runs `prepare`, a `COM_STMT_PREPARE`, so that a transaction that had
    /// not taken its read snapshot still has not taken it afterwards.
    ///
    /// Preparing reads this server's own catalog on the session's connection,
    /// and inside a transaction the engine takes the snapshot at that first
    /// read. MySQL takes a transaction's read view at its first read of a
    /// table, which a prepare is not. Measured on MySQL 8.4.11: a session that
    /// begins, prepares an `INSERT` while another session holds uncommitted
    /// rows, and runs it after that session commits, writes its row. Keeping
    /// the snapshot the prepare took would give that write up with 1213.
    /// Nothing the prepare read reaches the client as rows, so letting it go
    /// cannot leave the transaction acting on data it saw at another moment.
    pub fn prepare_without_starting_the_snapshot<T>(&self, prepare: impl FnOnce() -> T) -> T {
        let unread = self.transaction_has_not_read() && !self.inner.mvcc_enabled();
        let prepared = prepare();
        if unread {
            self.inner
                .release_read_snapshot()
                .expect("a transaction that had not read lets go of what its prepare read");
        }
        prepared
    }

    fn transaction_has_not_read(&self) -> bool {
        !self.inner.get_auto_commit() && !self.inner.has_read_snapshot()
    }

    /// Whether the statement about to run takes the snapshot it reads from
    /// itself: the session is in no transaction, or in one that has not read.
    pub fn statement_takes_its_own_snapshot(&self) -> bool {
        // In MVCC mode `BEGIN CONCURRENT` takes the snapshot, and a conflict
        // ends the whole transaction, so only a statement of its own runs again.
        if self.inner.mvcc_enabled() {
            return self.inner.get_auto_commit();
        }
        self.inner.get_auto_commit() || !self.inner.has_read_snapshot()
    }

    /// Readies the session to run again a statement that took its own
    /// snapshot and found it stale before it could write.
    ///
    /// A transaction the statement began itself, as the first statement with
    /// autocommit off does, is rolled back whole; it holds nothing else. One
    /// begun before the statement keeps what it has and lets the snapshot go.
    pub fn start_a_stale_statement_again(
        &self,
        began_in_a_transaction: bool,
    ) -> std::result::Result<(), MySqlQueryError> {
        if began_in_a_transaction {
            return self
                .inner
                .release_read_snapshot()
                .map_err(MySqlQueryError::Engine);
        }
        self.roll_back_after_serialization_failure()
    }

    /// Reports whether the statement that just ran began a transaction, which
    /// is what uses up a level set for the next transaction alone.
    pub fn began_transaction(&self) -> bool {
        self.transaction_isolation.lock().unwrap().began_transaction
    }

    /// The level of the transaction the session is in, or last began.
    pub fn transaction_isolation(&self) -> MySqlIsolationLevel {
        self.transaction_isolation.lock().unwrap().current
    }

    /// Ends a transaction whose snapshot went stale before it could write.
    ///
    /// Measured on MySQL 8.4.11: after 1213 the transaction is rolled back
    /// and the session is no longer in one, so the next statement runs on its
    /// own.
    pub fn roll_back_after_serialization_failure(
        &self,
    ) -> std::result::Result<(), MySqlQueryError> {
        if self.inner.get_auto_commit() {
            return Ok(());
        }
        *self.read_only_transaction.lock().unwrap() = false;
        self.run_engine_statement("ROLLBACK")
    }

    /// Records that the statement running now begins a transaction, at the
    /// level asked for the next one.
    pub(super) fn begin_transaction_isolation(&self) {
        let mut isolation = self.transaction_isolation.lock().unwrap();
        isolation.current = isolation.next;
        isolation.began_transaction = true;
        isolation.read_view_taken = false;
    }

    /// Takes the read snapshot of a transaction just begun, for `WITH
    /// CONSISTENT SNAPSHOT`.
    pub(super) fn begin_consistent_snapshot(
        &self,
    ) -> std::result::Result<MySqlTransactionOutcome, MySqlQueryError> {
        if self.transaction_isolation() != MySqlIsolationLevel::RepeatableRead {
            return Ok(MySqlTransactionOutcome {
                consistent_snapshot_ignored: true,
            });
        }
        if self.inner.mvcc_enabled() {
            self.note_consistent_read();
            return Ok(MySqlTransactionOutcome::default());
        }
        self.inner
            .begin_read_snapshot()
            .map_err(MySqlQueryError::Engine)?;
        Ok(MySqlTransactionOutcome::default())
    }
}
