use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::common::{ExecRows, TempDatabase};
use turso_core::{
    Connection, DatabaseOpts, LimboError, LockingRead, RowLockMode, RowLockWaitPolicy,
};

const LONG_ENOUGH_TO_SEE_A_WAIT: Duration = Duration::from_millis(300);

fn database_with_row_locks() -> TempDatabase {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_mvcc_row_locks(true))
        .with_mvcc(true)
        .build();
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER, note TEXT)")
        .unwrap();
    conn.execute("CREATE UNIQUE INDEX accounts_note ON accounts (note)")
        .unwrap();
    conn.execute("INSERT INTO accounts VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c')")
        .unwrap();
    db
}

fn session(db: &TempDatabase) -> Arc<Connection> {
    let conn = db.connect_limbo();
    conn.set_busy_timeout(Duration::from_secs(10));
    conn
}

fn run_in_the_background(
    conn: Arc<Connection>,
    sql: &'static str,
) -> JoinHandle<(Arc<Connection>, turso_core::Result<()>)> {
    std::thread::spawn(move || {
        let result = conn.execute(sql);
        (conn, result)
    })
}

fn still_waits<T>(handle: &JoinHandle<T>) -> bool {
    std::thread::sleep(LONG_ENOUGH_TO_SEE_A_WAIT);
    !handle.is_finished()
}

fn balance(conn: &Arc<Connection>, id: i64) -> i64 {
    let rows: Vec<(i64,)> =
        conn.exec_rows(&format!("SELECT balance FROM accounts WHERE id = {id}"));
    rows[0].0
}

#[test]
fn an_update_waits_for_the_transaction_holding_the_row_then_writes_on_its_committed_row() {
    let db = database_with_row_locks();
    let holder = session(&db);
    let waiter = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    waiter.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(balance(&waiter, 1), 10);
    holder
        .execute("UPDATE accounts SET balance = balance + 1 WHERE id = 1")
        .unwrap();

    let waiting = run_in_the_background(
        waiter,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
    );
    assert!(still_waits(&waiting));
    holder.execute("COMMIT").unwrap();
    let (waiter, result) = waiting.join().unwrap();
    result.unwrap();

    assert_eq!(balance(&waiter, 1), 12);
    assert_eq!(balance(&waiter, 2), 20);
    waiter.execute("COMMIT").unwrap();
    assert_eq!(balance(&holder, 1), 12);
}

#[test]
fn an_update_waiting_for_a_holder_that_rolls_back_writes_on_the_row_it_left() {
    let db = database_with_row_locks();
    let holder = session(&db);
    let waiter = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    waiter.execute("BEGIN CONCURRENT").unwrap();
    holder
        .execute("UPDATE accounts SET balance = balance + 1 WHERE id = 1")
        .unwrap();

    let waiting = run_in_the_background(
        waiter,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
    );
    assert!(still_waits(&waiting));
    holder.execute("ROLLBACK").unwrap();
    let (waiter, result) = waiting.join().unwrap();
    result.unwrap();
    waiter.execute("COMMIT").unwrap();

    assert_eq!(balance(&holder, 1), 11);
}

#[test]
fn an_update_of_a_row_another_transaction_committed_since_the_snapshot_writes_on_the_newer_row() {
    let db = database_with_row_locks();
    let other = session(&db);
    let writer = session(&db);
    writer.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(balance(&writer, 1), 10);
    other
        .execute("UPDATE accounts SET balance = 50 WHERE id = 1")
        .unwrap();
    other.execute("DELETE FROM accounts WHERE id = 2").unwrap();

    writer
        .execute("UPDATE accounts SET balance = balance + 1 WHERE id = 1")
        .unwrap();
    writer.execute("DELETE FROM accounts WHERE id = 2").unwrap();
    assert_eq!(balance(&writer, 1), 51);
    assert_eq!(balance(&writer, 3), 30);
    writer.execute("COMMIT").unwrap();
    assert_eq!(balance(&other, 1), 51);
}

#[test]
fn a_wait_longer_than_the_busy_timeout_fails_only_the_statement() {
    let db = database_with_row_locks();
    let holder = session(&db);
    let waiter = session(&db);
    waiter.set_busy_timeout(Duration::from_millis(200));
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder
        .execute("UPDATE accounts SET balance = 0 WHERE id = 1")
        .unwrap();
    waiter.execute("BEGIN CONCURRENT").unwrap();
    waiter
        .execute("UPDATE accounts SET balance = 99 WHERE id = 2")
        .unwrap();

    let started = Instant::now();
    let result = waiter.execute("UPDATE accounts SET balance = 99 WHERE id = 1");
    assert!(matches!(result, Err(LimboError::Busy)), "{result:?}");
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(!waiter.get_auto_commit());
    waiter.execute("COMMIT").unwrap();
    holder.execute("COMMIT").unwrap();

    assert_eq!(balance(&holder, 1), 0);
    assert_eq!(balance(&holder, 2), 99);
}

#[test]
fn a_deadlock_gives_up_the_transaction_whose_wait_closes_it_when_both_wrote_as_much() {
    let db = database_with_row_locks();
    let first = session(&db);
    let second = session(&db);
    first.execute("BEGIN CONCURRENT").unwrap();
    second.execute("BEGIN CONCURRENT").unwrap();
    first
        .execute("UPDATE accounts SET balance = balance + 1 WHERE id = 1")
        .unwrap();
    second
        .execute("UPDATE accounts SET balance = balance + 10 WHERE id = 2")
        .unwrap();

    let waiting = run_in_the_background(
        first,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 2",
    );
    assert!(still_waits(&waiting));
    let closing = second.execute("UPDATE accounts SET balance = balance + 10 WHERE id = 1");
    assert!(
        matches!(closing, Err(LimboError::WriteWriteConflict)),
        "{closing:?}"
    );
    assert!(second.get_auto_commit());
    let (first, result) = waiting.join().unwrap();
    result.unwrap();
    first.execute("COMMIT").unwrap();

    assert_eq!(balance(&second, 1), 11);
    assert_eq!(balance(&second, 2), 21);
}

#[test]
fn a_deadlock_gives_up_the_transaction_that_wrote_fewer_rows() {
    let db = database_with_row_locks();
    let lighter = session(&db);
    let heavier = session(&db);
    lighter.execute("BEGIN CONCURRENT").unwrap();
    heavier.execute("BEGIN CONCURRENT").unwrap();
    lighter
        .execute("UPDATE accounts SET balance = balance + 1 WHERE id = 1")
        .unwrap();
    heavier
        .execute("UPDATE accounts SET balance = balance + 10 WHERE id = 2")
        .unwrap();
    heavier
        .execute("UPDATE accounts SET balance = balance + 10 WHERE id = 3")
        .unwrap();

    let waiting = run_in_the_background(
        lighter,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 2",
    );
    assert!(still_waits(&waiting));
    heavier
        .execute("UPDATE accounts SET balance = balance + 10 WHERE id = 1")
        .unwrap();
    let (lighter, result) = waiting.join().unwrap();
    assert!(
        matches!(result, Err(LimboError::WriteWriteConflict)),
        "{result:?}"
    );
    assert!(lighter.get_auto_commit());
    heavier.execute("COMMIT").unwrap();

    assert_eq!(balance(&lighter, 1), 20);
    assert_eq!(balance(&lighter, 2), 30);
    assert_eq!(balance(&lighter, 3), 40);
}

#[test]
fn an_insert_of_a_unique_key_another_transaction_inserted_waits_for_it() {
    for (holder_ends_with, waiter_succeeds) in [("COMMIT", false), ("ROLLBACK", true)] {
        let db = database_with_row_locks();
        let holder = session(&db);
        let waiter = session(&db);
        holder.execute("BEGIN CONCURRENT").unwrap();
        holder
            .execute("INSERT INTO accounts VALUES (4, 0, 'same')")
            .unwrap();
        waiter.execute("BEGIN CONCURRENT").unwrap();

        let waiting = run_in_the_background(waiter, "INSERT INTO accounts VALUES (5, 0, 'same')");
        assert!(still_waits(&waiting), "{holder_ends_with}");
        holder.execute(holder_ends_with).unwrap();
        let (waiter, result) = waiting.join().unwrap();
        if waiter_succeeds {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(LimboError::Constraint(_))),
                "{result:?}"
            );
        }
        assert!(!waiter.get_auto_commit());
        waiter.execute("COMMIT").unwrap();
        let ids: Vec<(i64,)> =
            holder.exec_rows("SELECT id FROM accounts WHERE note = 'same' ORDER BY id");
        assert_eq!(ids, [(if waiter_succeeds { 5 } else { 4 },)]);
    }
}

#[test]
fn an_insert_of_a_different_key_does_not_wait() {
    let db = database_with_row_locks();
    let holder = session(&db);
    let other = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder
        .execute("INSERT INTO accounts VALUES (4, 0, 'news')")
        .unwrap();
    other.execute("BEGIN CONCURRENT").unwrap();
    other
        .execute("INSERT INTO accounts VALUES (5, 0, 'rust')")
        .unwrap();
    other.execute("COMMIT").unwrap();
    holder.execute("COMMIT").unwrap();
    let count: Vec<(i64,)> = holder.exec_rows("SELECT COUNT(*) FROM accounts");
    assert_eq!(count, [(5,)]);
}

#[test]
fn a_locking_read_waits_for_a_changed_row_and_reads_what_was_committed() {
    let db = database_with_row_locks();
    let holder = session(&db);
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(balance(&reader, 1), 10);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder
        .execute("UPDATE accounts SET balance = 11 WHERE id = 1")
        .unwrap();

    let waiting = std::thread::spawn(move || {
        let read = locked_balances(&reader, RowLockMode::Exclusive, RowLockWaitPolicy::Wait);
        (reader, read)
    });
    assert!(still_waits(&waiting));
    holder.execute("COMMIT").unwrap();
    let (reader, read) = waiting.join().unwrap();
    assert_eq!(read.unwrap(), [(1, 11)]);
    assert_eq!(balance(&reader, 1), 10);

    let blocked = session(&db);
    blocked.set_busy_timeout(Duration::from_millis(100));
    assert!(matches!(
        blocked.execute("UPDATE accounts SET balance = 0 WHERE id = 1"),
        Err(LimboError::Busy)
    ));
    blocked
        .execute("UPDATE accounts SET balance = 0 WHERE id = 2")
        .unwrap();
    reader.execute("COMMIT").unwrap();
    blocked
        .execute("UPDATE accounts SET balance = 0 WHERE id = 1")
        .unwrap();
}

#[test]
fn shared_locks_let_each_other_in_and_keep_writers_out() {
    let db = database_with_row_locks();
    let first = session(&db);
    let second = session(&db);
    first.execute("BEGIN CONCURRENT").unwrap();
    second.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_balances(&first, RowLockMode::Shared, RowLockWaitPolicy::Wait).unwrap(),
        [(1, 10)]
    );
    assert_eq!(
        locked_balances(&second, RowLockMode::Shared, RowLockWaitPolicy::Wait).unwrap(),
        [(1, 10)]
    );
    second.set_busy_timeout(Duration::from_millis(100));
    assert!(matches!(
        second.execute("UPDATE accounts SET balance = 0 WHERE id = 1"),
        Err(LimboError::Busy)
    ));
    assert!(matches!(
        locked_balances(&second, RowLockMode::Exclusive, RowLockWaitPolicy::NoWait),
        Err(LimboError::RowLocked(_))
    ));
    first.execute("COMMIT").unwrap();
    second
        .execute("UPDATE accounts SET balance = 0 WHERE id = 1")
        .unwrap();
    second.execute("COMMIT").unwrap();
}

#[test]
fn skip_locked_leaves_out_the_rows_another_transaction_holds() {
    let db = database_with_row_locks();
    let holder = session(&db);
    let reader = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder
        .execute("UPDATE accounts SET balance = 0 WHERE id = 1")
        .unwrap();
    reader.execute("BEGIN CONCURRENT").unwrap();

    let mut statement = reader
        .prepare("SELECT id FROM accounts ORDER BY id")
        .unwrap();
    statement.lock_rows_it_reads(LockingRead {
        mode: RowLockMode::Exclusive,
        policy: RowLockWaitPolicy::SkipLocked,
        tables: vec!["accounts".to_string()],
    });
    let ids: Vec<i64> = statement
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[0].as_int().unwrap())
        .collect();
    assert_eq!(ids, [2, 3]);
    holder.execute("ROLLBACK").unwrap();
    reader.execute("COMMIT").unwrap();
}

#[test]
fn without_row_locks_a_write_to_a_changed_row_fails_at_once() {
    let db = TempDatabase::builder().with_mvcc(true).build();
    let setup = db.connect_limbo();
    setup
        .execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER)")
        .unwrap();
    setup
        .execute("INSERT INTO accounts VALUES (1, 10)")
        .unwrap();
    let holder = session(&db);
    let other = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder
        .execute("UPDATE accounts SET balance = 11 WHERE id = 1")
        .unwrap();
    other.execute("BEGIN CONCURRENT").unwrap();
    let started = Instant::now();
    let result = other.execute("UPDATE accounts SET balance = 12 WHERE id = 1");
    assert!(
        matches!(result, Err(LimboError::WriteWriteConflict)),
        "{result:?}"
    );
    assert!(started.elapsed() < LONG_ENOUGH_TO_SEE_A_WAIT);
}

fn locked_balances(
    conn: &Arc<Connection>,
    mode: RowLockMode,
    policy: RowLockWaitPolicy,
) -> turso_core::Result<Vec<(i64, i64)>> {
    let mut statement = conn.prepare("SELECT id, balance FROM accounts WHERE id = 1")?;
    statement.lock_rows_it_reads(LockingRead {
        mode,
        policy,
        tables: vec!["accounts".to_string()],
    });
    Ok(statement
        .run_collect_rows()?
        .into_iter()
        .map(|row| (row[0].as_int().unwrap(), row[1].as_int().unwrap()))
        .collect())
}
