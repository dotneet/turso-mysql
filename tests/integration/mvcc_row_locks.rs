use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::common::{ExecRows, TempDatabase};
use turso_core::{
    Connection, DatabaseOpts, LimboError, LockingRead, RowLockLevel, RowLockMode, RowLockWaitPolicy,
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

#[test]
fn a_range_locking_read_keeps_inserts_out_of_the_gaps_it_read() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id > 15 AND id < 35",
            RowLockMode::Exclusive
        ),
        [20, 30]
    );

    assert!(waits(&db, "INSERT INTO t VALUES (11, 0, 11, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (25, 0, 25, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (36, 0, 36, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (5, 0, 5, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (45, 0, 45, 0)"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 30"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 10"));
    reader.execute("COMMIT").unwrap();
    assert!(!waits(&db, "INSERT INTO t VALUES (25, 0, 25, 0)"));
}

#[test]
fn a_locking_read_of_one_unique_key_locks_only_its_row() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id = 20",
            RowLockMode::Exclusive
        ),
        [20]
    );
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE u = 30",
            RowLockMode::Exclusive
        ),
        [30]
    );
    assert!(!waits(&db, "INSERT INTO t VALUES (19, 0, 19, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (21, 0, 21, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (22, 0, 31, 0)"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 20"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 30"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_locking_read_of_a_missing_key_locks_the_gap_it_would_be_in() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert!(locked_ids(
        &reader,
        "SELECT id FROM t WHERE id = 25",
        RowLockMode::Exclusive
    )
    .is_empty());
    assert!(locked_ids(
        &reader,
        "SELECT id FROM t WHERE k = 30",
        RowLockMode::Shared
    )
    .is_empty());
    assert!(locked_ids(
        &reader,
        "SELECT id FROM t WHERE id = 50",
        RowLockMode::Shared
    )
    .is_empty());

    assert!(waits(&db, "INSERT INTO t VALUES (21, 0, 21, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (29, 0, 29, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (5, 35, 5, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (60, 0, 60, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (31, 0, 31, 0)"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 30"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn two_transactions_that_locked_one_gap_and_insert_into_it_deadlock() {
    let db = database_with_gaps_to_lock();
    let first = session(&db);
    let second = session(&db);
    first.execute("BEGIN CONCURRENT").unwrap();
    second.execute("BEGIN CONCURRENT").unwrap();
    assert!(locked_ids(
        &first,
        "SELECT id FROM t WHERE id = 25",
        RowLockMode::Exclusive
    )
    .is_empty());
    assert!(locked_ids(
        &second,
        "SELECT id FROM t WHERE id = 26",
        RowLockMode::Exclusive
    )
    .is_empty());

    let waiting = run_in_the_background(first, "INSERT INTO t VALUES (25, 0, 25, 0)");
    assert!(still_waits(&waiting));
    let closing = second.execute("INSERT INTO t VALUES (26, 0, 26, 0)");
    assert!(
        matches!(closing, Err(LimboError::WriteWriteConflict)),
        "{closing:?}"
    );
    let (first, result) = waiting.join().unwrap();
    result.unwrap();
    first.execute("COMMIT").unwrap();
    let ids: Vec<(i64,)> = first.exec_rows("SELECT id FROM t WHERE id BETWEEN 21 AND 29");
    assert_eq!(ids, [(25,)]);
}

#[test]
fn a_full_scan_locks_the_gap_after_the_last_row() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(&reader, "SELECT id FROM t WHERE v = 2", RowLockMode::Shared),
        [20]
    );
    assert!(waits(&db, "INSERT INTO t VALUES (100, 0, 100, 0)"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_secondary_index_scan_locks_its_gaps_and_the_rows_it_matched() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE k = 20 ORDER BY id",
            RowLockMode::Exclusive
        ),
        [20, 30]
    );
    assert!(waits(&db, "INSERT INTO t VALUES (25, 20, 25, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (26, 15, 26, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (27, 35, 27, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (28, 45, 28, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (29, 5, 29, 0)"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 30"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_primary_key_index_scan_locks_gaps_the_way_a_rowid_scan_does() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM p WHERE id > 15 AND id < 35",
            RowLockMode::Exclusive
        ),
        [20, 30]
    );
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM p WHERE id = 70",
            RowLockMode::Exclusive
        ),
        [70]
    );
    assert!(waits(&db, "INSERT INTO p VALUES (11, 0)"));
    assert!(waits(&db, "INSERT INTO p VALUES (36, 0)"));
    assert!(!waits(&db, "INSERT INTO p VALUES (45, 0)"));
    assert!(!waits(&db, "INSERT INTO p VALUES (71, 0)"));
    assert!(!waits(&db, "UPDATE p SET v = 9 WHERE id = 40"));
    assert!(waits(&db, "UPDATE p SET v = 9 WHERE id = 70"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_locking_read_waits_for_a_row_another_transaction_inserted_into_its_range() {
    let db = database_with_gaps_to_lock();
    let inserter = session(&db);
    inserter.execute("BEGIN CONCURRENT").unwrap();
    inserter
        .execute("INSERT INTO t VALUES (25, 25, 25, 25)")
        .unwrap();
    for level in [RowLockLevel::RepeatableRead, RowLockLevel::ReadCommitted] {
        assert!(locking_read_waits(
            &db,
            level,
            "SELECT id FROM t WHERE id > 15 AND id < 35"
        ));
        assert!(locking_read_waits(
            &db,
            level,
            "SELECT id FROM t WHERE id = 25"
        ));
        assert!(!locking_read_waits(
            &db,
            level,
            "SELECT id FROM t WHERE id > 26"
        ));
    }
    assert!(!locking_read_waits(
        &db,
        RowLockLevel::RepeatableRead,
        "SELECT id FROM t WHERE id < 25"
    ));
    assert!(!waits(&db, "INSERT INTO t VALUES (22, 0, 22, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (99, 0, 25, 0)"));
    inserter.execute("ROLLBACK").unwrap();
}

#[test]
fn read_committed_locks_no_gaps_and_lets_go_of_rows_that_did_not_match() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.set_row_lock_level(RowLockLevel::ReadCommitted);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id > 15 AND id < 35",
            RowLockMode::Exclusive
        ),
        [20, 30]
    );
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE v = 4",
            RowLockMode::Exclusive
        ),
        [40]
    );
    assert!(!waits(&db, "INSERT INTO t VALUES (25, 0, 25, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (100, 0, 100, 0)"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 10"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 20"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_read_committed_update_skips_a_held_row_whose_committed_version_does_not_match() {
    let db = database_with_gaps_to_lock();
    let holder = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder.execute("UPDATE t SET v = 33 WHERE id = 30").unwrap();

    let updater = session(&db);
    updater.set_row_lock_level(RowLockLevel::ReadCommitted);
    updater.set_busy_timeout(Duration::from_millis(200));
    updater.execute("BEGIN CONCURRENT").unwrap();
    updater.execute("UPDATE t SET v = 9 WHERE v = 2").unwrap();
    updater
        .execute("UPDATE t SET v = 9 WHERE id > 15 AND id < 35 AND v = 1")
        .unwrap();
    assert!(matches!(
        updater.execute("UPDATE t SET v = 9 WHERE v = 3"),
        Err(LimboError::Busy)
    ));
    assert!(matches!(
        updater.execute("DELETE FROM t WHERE v = 1"),
        Err(LimboError::Busy)
    ));
    updater.execute("COMMIT").unwrap();
    holder.execute("COMMIT").unwrap();
    let values: Vec<(i64, i64)> = holder.exec_rows("SELECT id, v FROM t ORDER BY id");
    assert_eq!(values, [(10, 1), (20, 9), (30, 33), (40, 4)]);
}

#[test]
fn a_read_up_to_a_key_it_found_does_not_lock_the_gap_past_it() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id <= 20",
            RowLockMode::Exclusive
        ),
        [10, 20]
    );
    assert!(!waits(&db, "INSERT INTO t VALUES (25, 0, 25, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (15, 0, 15, 0)"));
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM p WHERE id <= 25",
            RowLockMode::Exclusive
        ),
        [10, 20]
    );
    assert!(waits(&db, "INSERT INTO p VALUES (26, 0)"));
    assert!(!waits(&db, "UPDATE p SET v = 9 WHERE id = 30"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_descending_read_locks_the_gap_above_where_it_started_and_below_where_it_stopped() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id >= 20 AND id < 40 ORDER BY id DESC",
            RowLockMode::Exclusive
        ),
        [30, 20]
    );
    assert!(waits(&db, "INSERT INTO t VALUES (35, 0, 35, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (15, 0, 15, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (5, 0, 5, 0)"));
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 10"));
    assert!(!waits(&db, "INSERT INTO t VALUES (45, 0, 45, 0)"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn an_insert_that_finds_a_duplicate_key_locks_the_row_it_found_even_under_read_committed() {
    let db = database_with_gaps_to_lock();
    let inserter = session(&db);
    inserter.set_row_lock_level(RowLockLevel::ReadCommitted);
    inserter.execute("BEGIN CONCURRENT").unwrap();
    for duplicate in [
        "INSERT INTO t VALUES (10, 0, 99, 0)",
        "INSERT INTO t VALUES (99, 0, 30, 0)",
        "INSERT INTO p VALUES (20, 0)",
    ] {
        let result = inserter.execute(duplicate);
        assert!(
            matches!(result, Err(LimboError::Constraint(_))),
            "{duplicate}: {result:?}"
        );
    }
    assert!(!inserter.get_auto_commit());

    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 10"));
    assert!(!waits(&db, "INSERT INTO t VALUES (5, 0, 5, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (98, 0, 25, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (97, 0, 35, 0)"));
    assert!(waits(&db, "DELETE FROM t WHERE id = 30"));
    assert!(waits(&db, "UPDATE p SET v = 9 WHERE id = 20"));
    assert!(!waits(&db, "INSERT INTO p VALUES (15, 0)"));
    assert!(!waits(&db, "INSERT INTO p VALUES (25, 0)"));
    inserter.execute("COMMIT").unwrap();
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 10"));
}

#[test]
fn a_read_from_a_primary_key_it_found_locks_no_gap_below_it() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id >= 20 AND id < 30",
            RowLockMode::Exclusive
        ),
        [20]
    );
    assert!(!waits(&db, "INSERT INTO t VALUES (15, 0, 15, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (25, 0, 25, 0)"));
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id >= 15 AND id < 20",
            RowLockMode::Exclusive
        ),
        Vec::<i64>::new()
    );
    assert!(waits(&db, "INSERT INTO t VALUES (15, 0, 15, 0)"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_read_committed_update_through_a_secondary_index_waits_for_a_held_row() {
    let db = database_with_gaps_to_lock();
    let holder = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder.execute("UPDATE t SET v = 33 WHERE id = 30").unwrap();

    let updater = session(&db);
    updater.set_row_lock_level(RowLockLevel::ReadCommitted);
    updater.set_busy_timeout(Duration::from_millis(200));
    updater.execute("BEGIN CONCURRENT").unwrap();
    assert!(matches!(
        updater.execute("UPDATE t SET v = 9 WHERE k = 20 AND v = 2"),
        Err(LimboError::Busy)
    ));
    updater.execute("ROLLBACK").unwrap();
    holder.execute("ROLLBACK").unwrap();
}

#[test]
fn a_descending_read_stopped_by_its_limit_locks_the_gap_below_its_last_row() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE id <= 40 ORDER BY id DESC LIMIT 2",
            RowLockMode::Exclusive
        ),
        [40, 30]
    );
    assert!(waits(&db, "INSERT INTO t VALUES (45, 0, 45, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (35, 0, 35, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (25, 0, 25, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (15, 0, 15, 0)"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 20"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn skip_locked_and_nowait_lock_only_the_gap_below_where_a_primary_key_range_stops() {
    let db = database_with_gaps_to_lock();
    let holder = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder.execute("UPDATE t SET v = 9 WHERE id = 40").unwrap();
    for policy in [RowLockWaitPolicy::SkipLocked, RowLockWaitPolicy::NoWait] {
        let reader = session(&db);
        reader.execute("BEGIN CONCURRENT").unwrap();
        assert_eq!(
            locked_ids_with(
                &reader,
                "SELECT id FROM t WHERE id < 35",
                RowLockMode::Exclusive,
                policy
            )
            .unwrap(),
            [10, 20, 30],
            "{policy:?}"
        );
        assert!(waits(&db, "INSERT INTO t VALUES (35, 0, 35, 0)"));
        assert!(!waits(&db, "INSERT INTO t VALUES (45, 0, 45, 0)"));
        reader.execute("ROLLBACK").unwrap();
    }
    holder.execute("ROLLBACK").unwrap();

    for policy in [RowLockWaitPolicy::SkipLocked, RowLockWaitPolicy::NoWait] {
        let reader = session(&db);
        reader.execute("BEGIN CONCURRENT").unwrap();
        assert_eq!(
            locked_ids_with(
                &reader,
                "SELECT id FROM t WHERE id < 35",
                RowLockMode::Exclusive,
                policy
            )
            .unwrap(),
            [10, 20, 30]
        );
        assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
        assert!(waits(&db, "INSERT INTO t VALUES (36, 0, 36, 0)"));
        reader.execute("ROLLBACK").unwrap();

        let reader = session(&db);
        reader.execute("BEGIN CONCURRENT").unwrap();
        assert_eq!(
            locked_ids_with(
                &reader,
                "SELECT id FROM t WHERE k = 10",
                RowLockMode::Exclusive,
                policy
            )
            .unwrap(),
            [10]
        );
        assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 20"));
        assert!(waits(&db, "INSERT INTO t VALUES (37, 15, 37, 0)"));
        reader.execute("ROLLBACK").unwrap();
    }
}

#[test]
fn skip_locked_reads_past_a_held_row_inside_a_primary_key_range() {
    let db = database_with_gaps_to_lock();
    let holder = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    holder.execute("UPDATE t SET v = 9 WHERE id = 20").unwrap();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids_with(
            &reader,
            "SELECT id FROM t WHERE id < 35",
            RowLockMode::Exclusive,
            RowLockWaitPolicy::SkipLocked
        )
        .unwrap(),
        [10, 30]
    );
    assert!(matches!(
        locked_ids_with(
            &reader,
            "SELECT id FROM t WHERE id < 35",
            RowLockMode::Exclusive,
            RowLockWaitPolicy::NoWait
        ),
        Err(LimboError::RowLocked(_))
    ));
    reader.execute("ROLLBACK").unwrap();
    holder.execute("ROLLBACK").unwrap();
}

#[test]
fn a_secondary_index_range_locks_the_row_where_it_stops_with_its_table_row() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE k < 15",
            RowLockMode::Exclusive
        ),
        [10]
    );
    assert!(waits(&db, "UPDATE t SET v = 9 WHERE id = 20"));
    assert!(waits(&db, "INSERT INTO t VALUES (15, 15, 15, 0)"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 30"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_read_of_an_in_list_on_a_secondary_index_locks_only_the_gap_after_each_value() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(
        locked_ids(
            &reader,
            "SELECT id FROM t WHERE k IN (10, 40)",
            RowLockMode::Exclusive
        ),
        [10, 40]
    );
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 20"));
    assert!(waits(&db, "INSERT INTO t VALUES (15, 15, 15, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (45, 45, 45, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (35, 35, 35, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (25, 20, 25, 0)"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn read_committed_lets_go_of_a_row_that_did_not_match_when_the_statement_ends() {
    let db = database_with_gaps_to_lock();
    let writer = session(&db);
    writer.set_row_lock_level(RowLockLevel::ReadCommitted);
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer
        .execute("UPDATE t SET v = 9 WHERE id = 20 AND v = 99")
        .unwrap();
    writer
        .execute("DELETE FROM t WHERE id = 30 AND v = 99")
        .unwrap();
    writer
        .execute("UPDATE t SET v = 9 WHERE id IN (10, 40) AND v = 99")
        .unwrap();
    assert!(locked_ids(
        &writer,
        "SELECT id FROM t WHERE id IN (10, 40) AND v = 99",
        RowLockMode::Exclusive
    )
    .is_empty());
    assert!(!waits(&db, "UPDATE t SET v = 8 WHERE id = 20"));
    assert!(!waits(&db, "UPDATE t SET v = 8 WHERE id = 30"));
    assert!(!waits(&db, "UPDATE t SET v = 8 WHERE id = 10"));
    assert!(!waits(&db, "UPDATE t SET v = 8 WHERE id = 40"));
    writer.execute("COMMIT").unwrap();
}

#[test]
fn a_read_committed_locking_read_of_one_key_keeps_the_row_it_found_though_it_did_not_match() {
    let db = database_with_gaps_to_lock();
    let reader = session(&db);
    reader.set_row_lock_level(RowLockLevel::ReadCommitted);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert!(locked_ids(
        &reader,
        "SELECT id FROM t WHERE id = 20 AND v = 99",
        RowLockMode::Exclusive
    )
    .is_empty());
    assert!(locked_ids(
        &reader,
        "SELECT id FROM t WHERE u = 30 AND v = 99",
        RowLockMode::Exclusive
    )
    .is_empty());
    assert!(waits(&db, "UPDATE t SET v = 8 WHERE id = 20"));
    assert!(waits(&db, "UPDATE t SET v = 8 WHERE id = 30"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_missed_key_locks_the_gap_up_to_another_transactions_uncommitted_row() {
    let db = database_with_gaps_to_lock();
    let inserter = session(&db);
    inserter.execute("BEGIN CONCURRENT").unwrap();
    inserter
        .execute("INSERT INTO t VALUES (25, 25, 25, 25)")
        .unwrap();
    for sql in [
        "SELECT id FROM t WHERE id = 22",
        "SELECT id FROM t WHERE k = 22",
        "SELECT id FROM t WHERE u = 22",
    ] {
        let reader = session(&db);
        reader.execute("BEGIN CONCURRENT").unwrap();
        assert!(locked_ids(&reader, sql, RowLockMode::Exclusive).is_empty());
        assert!(waits(&db, "INSERT INTO t VALUES (21, 21, 21, 0)"), "{sql}");
        assert!(!waits(&db, "INSERT INTO t VALUES (26, 26, 26, 0)"), "{sql}");
        reader.execute("ROLLBACK").unwrap();
    }
    for sql in [
        "SELECT id FROM t WHERE id = 28",
        "SELECT id FROM t WHERE k = 28",
        "SELECT id FROM t WHERE u = 28",
    ] {
        let reader = session(&db);
        reader.execute("BEGIN CONCURRENT").unwrap();
        assert!(locked_ids(&reader, sql, RowLockMode::Exclusive).is_empty());
        assert!(waits(&db, "INSERT INTO t VALUES (26, 26, 26, 0)"), "{sql}");
        assert!(!waits(&db, "INSERT INTO t VALUES (21, 21, 21, 0)"), "{sql}");
        reader.execute("ROLLBACK").unwrap();
    }
    inserter.execute("ROLLBACK").unwrap();
}

#[test]
fn a_failed_statement_lets_go_of_its_inserted_rows_and_keeps_the_gaps_they_were_in() {
    let db = database_with_gaps_to_lock();
    let writer = session(&db);
    writer.execute("BEGIN CONCURRENT").unwrap();
    assert!(matches!(
        writer.execute("INSERT INTO t VALUES (25, 25, 25, 0), (10, 10, 99, 0)"),
        Err(LimboError::Constraint(_))
    ));
    assert!(!writer.get_auto_commit());
    assert!(!locking_read_waits(
        &db,
        RowLockLevel::RepeatableRead,
        "SELECT id FROM t WHERE id > 20 AND id < 30"
    ));
    assert!(waits(&db, "INSERT INTO t VALUES (27, 0, 27, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (5, 30, 5, 0)"));
    assert!(waits(&db, "INSERT INTO t VALUES (6, 0, 22, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (15, 15, 15, 0)"));
    writer.execute("COMMIT").unwrap();
    assert!(!waits(&db, "INSERT INTO t VALUES (27, 0, 27, 0)"));
}

#[test]
fn an_insert_that_gives_up_waiting_keeps_the_gaps_its_undone_rows_were_in() {
    let db = database_with_gaps_to_lock();
    let holder = session(&db);
    holder.execute("BEGIN CONCURRENT").unwrap();
    assert!(locked_ids(
        &holder,
        "SELECT id FROM t WHERE id = 35",
        RowLockMode::Exclusive
    )
    .is_empty());
    let writer = session(&db);
    writer.set_busy_timeout(Duration::from_millis(200));
    writer.execute("BEGIN CONCURRENT").unwrap();
    assert!(matches!(
        writer.execute("INSERT INTO t VALUES (25, 25, 25, 0), (35, 35, 35, 0)"),
        Err(LimboError::Busy)
    ));
    assert!(!writer.get_auto_commit());
    holder.execute("COMMIT").unwrap();
    assert!(!locking_read_waits(
        &db,
        RowLockLevel::RepeatableRead,
        "SELECT id FROM t WHERE id > 20 AND id < 30"
    ));
    assert!(waits(&db, "INSERT INTO t VALUES (27, 0, 27, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (36, 0, 36, 0)"));
    writer.execute("COMMIT").unwrap();
}

#[test]
fn a_failed_statement_under_read_committed_keeps_nothing_of_its_inserted_rows() {
    let db = database_with_gaps_to_lock();
    let writer = session(&db);
    writer.set_row_lock_level(RowLockLevel::ReadCommitted);
    writer.execute("BEGIN CONCURRENT").unwrap();
    assert!(matches!(
        writer.execute("INSERT INTO t VALUES (25, 25, 25, 0), (10, 10, 99, 0)"),
        Err(LimboError::Constraint(_))
    ));
    assert!(!locking_read_waits(
        &db,
        RowLockLevel::RepeatableRead,
        "SELECT id FROM t WHERE id > 20 AND id < 30"
    ));
    assert!(!waits(&db, "INSERT INTO t VALUES (27, 0, 27, 0)"));
    writer.execute("COMMIT").unwrap();
}

#[test]
fn a_rollback_to_a_savepoint_keeps_nothing_of_the_rows_it_undid() {
    let db = database_with_gaps_to_lock();
    let writer = session(&db);
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer.execute("SAVEPOINT s").unwrap();
    writer
        .execute("INSERT INTO t VALUES (25, 25, 25, 0)")
        .unwrap();
    writer.execute("ROLLBACK TO SAVEPOINT s").unwrap();
    assert!(!locking_read_waits(
        &db,
        RowLockLevel::RepeatableRead,
        "SELECT id FROM t WHERE id > 20 AND id < 30"
    ));
    assert!(!waits(&db, "INSERT INTO t VALUES (27, 0, 27, 0)"));
    writer.execute("COMMIT").unwrap();
}

fn database_with_gaps_to_lock() -> TempDatabase {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_mvcc_row_locks(true))
        .with_mvcc(true)
        .build();
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, u INT, v INT)")
        .unwrap();
    conn.execute("CREATE INDEX t_k ON t (k)").unwrap();
    conn.execute("CREATE UNIQUE INDEX t_u ON t (u)").unwrap();
    conn.execute(
        "INSERT INTO t VALUES (10, 10, 10, 1), (20, 20, 20, 2), (30, 20, 30, 3), (40, 40, 40, 4)",
    )
    .unwrap();
    conn.execute("CREATE TABLE p (id INT NOT NULL PRIMARY KEY, v INT)")
        .unwrap();
    conn.execute("INSERT INTO p VALUES (10, 1), (20, 2), (30, 3), (40, 4), (70, 7)")
        .unwrap();
    db
}

fn locked_ids(conn: &Arc<Connection>, sql: &str, mode: RowLockMode) -> Vec<i64> {
    locked_ids_with(conn, sql, mode, RowLockWaitPolicy::Wait).unwrap()
}

fn locked_ids_with(
    conn: &Arc<Connection>,
    sql: &str,
    mode: RowLockMode,
    policy: RowLockWaitPolicy,
) -> turso_core::Result<Vec<i64>> {
    let mut statement = conn.prepare(sql)?;
    let table = if sql.contains(" FROM p") { "p" } else { "t" };
    statement.lock_rows_it_reads(LockingRead {
        mode,
        policy,
        tables: vec![table.to_string()],
    });
    Ok(statement
        .run_collect_rows()?
        .into_iter()
        .map(|row| row[0].as_int().unwrap())
        .collect())
}

fn waits(db: &TempDatabase, sql: &str) -> bool {
    let probe = db.connect_limbo();
    probe.set_busy_timeout(Duration::from_millis(200));
    probe.execute("BEGIN CONCURRENT").unwrap();
    let result = probe.execute(sql);
    probe.execute("ROLLBACK").unwrap();
    match result {
        Ok(()) => false,
        Err(LimboError::Busy) => true,
        Err(err) => panic!("{sql}: {err:?}"),
    }
}

fn locking_read_waits(db: &TempDatabase, level: RowLockLevel, sql: &str) -> bool {
    let probe = db.connect_limbo();
    probe.set_busy_timeout(Duration::from_millis(200));
    probe.set_row_lock_level(level);
    probe.execute("BEGIN CONCURRENT").unwrap();
    let mut statement = probe.prepare(sql).unwrap();
    statement.lock_rows_it_reads(LockingRead {
        mode: RowLockMode::Exclusive,
        policy: RowLockWaitPolicy::Wait,
        tables: vec!["t".to_string()],
    });
    let result = statement.run_collect_rows();
    drop(statement);
    probe.execute("ROLLBACK").unwrap();
    match result {
        Ok(_) => false,
        Err(LimboError::Busy) => true,
        Err(err) => panic!("{sql}: {err:?}"),
    }
}

#[test]
fn a_locking_read_through_an_in_subquery_locks_the_rows_the_subquery_read() {
    let db = database_with_gaps_to_lock();
    for sql in [
        "SELECT id FROM t WHERE v IN (SELECT v FROM p)",
        "SELECT id FROM t WHERE v NOT IN (SELECT id FROM p)",
        "SELECT id FROM t WHERE k IN (SELECT id FROM p)",
        "SELECT id FROM t WHERE id IN (SELECT id FROM p)",
    ] {
        let reader = session(&db);
        reader.execute("BEGIN CONCURRENT").unwrap();
        let mut statement = reader.prepare(sql).unwrap();
        statement.lock_rows_it_reads(LockingRead {
            mode: RowLockMode::Shared,
            policy: RowLockWaitPolicy::Wait,
            tables: vec!["t".to_string(), "p".to_string()],
        });
        statement.run_collect_rows().unwrap();
        drop(statement);
        assert!(waits(&db, "UPDATE p SET v = 9 WHERE id = 30"), "{sql}");
        assert!(waits(&db, "DELETE FROM p WHERE id = 70"), "{sql}");
        assert!(waits(&db, "INSERT INTO p VALUES (35, 0)"), "{sql}");
        reader.execute("COMMIT").unwrap();
    }
}

#[test]
fn a_secondary_index_of_a_table_without_a_rowid_key_orders_equal_values_by_rowid_not_by_key() {
    let db = database_with_gaps_to_lock();
    let setup = db.connect_limbo();
    setup
        .execute("CREATE TABLE q (id INT NOT NULL PRIMARY KEY, k INT, v INT)")
        .unwrap();
    setup.execute("CREATE INDEX q_k ON q (k)").unwrap();
    setup.execute("INSERT INTO q VALUES (30, 5, 0)").unwrap();
    setup.execute("INSERT INTO q VALUES (10, 5, 0)").unwrap();
    setup.execute("INSERT INTO q VALUES (1, 1, 0)").unwrap();

    let reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    let mut statement = reader.prepare("SELECT id FROM q WHERE k < 5").unwrap();
    statement.lock_rows_it_reads(LockingRead {
        mode: RowLockMode::Exclusive,
        policy: RowLockWaitPolicy::Wait,
        tables: vec!["q".to_string()],
    });
    let ids: Vec<i64> = statement
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[0].as_int().unwrap())
        .collect();
    drop(statement);
    assert_eq!(ids, [1]);

    assert!(waits(&db, "UPDATE q SET v = 9 WHERE id = 30"));
    assert!(!waits(&db, "UPDATE q SET v = 9 WHERE id = 10"));
    reader.execute("COMMIT").unwrap();
}

#[test]
fn a_gap_that_ends_at_another_transactions_uncommitted_row_reaches_the_next_row_when_that_row_is_rolled_back(
) {
    let db = database_with_gaps_to_lock();
    for (ends_with, widened) in [("ROLLBACK", true), ("COMMIT", false)] {
        let inserter = session(&db);
        inserter.execute("BEGIN CONCURRENT").unwrap();
        inserter
            .execute("INSERT INTO t VALUES (25, 25, 25, 25)")
            .unwrap();
        let reader = session(&db);
        reader.execute("BEGIN CONCURRENT").unwrap();
        assert!(locked_ids(
            &reader,
            "SELECT id FROM t WHERE id = 22",
            RowLockMode::Exclusive
        )
        .is_empty());
        assert!(locked_ids(
            &reader,
            "SELECT id FROM t WHERE id = 33",
            RowLockMode::Exclusive
        )
        .is_empty());
        assert!(!waits(&db, "INSERT INTO t VALUES (27, 0, 27, 0)"));
        inserter.execute(ends_with).unwrap();
        assert_eq!(
            waits(&db, "INSERT INTO t VALUES (27, 0, 27, 0)"),
            widened,
            "{ends_with}"
        );
        assert!(waits(&db, "INSERT INTO t VALUES (21, 0, 21, 0)"));
        assert!(waits(&db, "INSERT INTO t VALUES (35, 0, 35, 0)"));
        assert!(!waits(&db, "INSERT INTO t VALUES (45, 0, 45, 0)"));
        reader.execute("ROLLBACK").unwrap();
        if ends_with == "COMMIT" {
            db.connect_limbo()
                .execute("DELETE FROM t WHERE id = 25")
                .unwrap();
        }
    }
}

#[test]
fn a_child_insert_locks_its_parent_row_in_share_mode_under_both_levels() {
    for level in [RowLockLevel::RepeatableRead, RowLockLevel::ReadCommitted] {
        let db = database_with_foreign_keys("");
        let writer = foreign_key_session(&db, level);
        writer.execute("BEGIN CONCURRENT").unwrap();
        writer.execute("INSERT INTO c VALUES (5, 20, 0)").unwrap();

        assert!(waits(&db, "DELETE FROM p WHERE id = 20"), "{level:?}");
        assert!(waits(&db, "UPDATE p SET v = 9 WHERE id = 20"), "{level:?}");
        assert!(waits(&db, "UPDATE p SET v = 9 WHERE v = 2"), "{level:?}");
        assert!(!waits(&db, "UPDATE p SET v = 9 WHERE id = 30"), "{level:?}");
        assert!(!waits(&db, "INSERT INTO p VALUES (25, 0)"), "{level:?}");
        let other = foreign_key_session(&db, level);
        other.execute("BEGIN CONCURRENT").unwrap();
        other.execute("INSERT INTO c VALUES (6, 20, 0)").unwrap();
        other.execute("ROLLBACK").unwrap();
        writer.execute("COMMIT").unwrap();
        assert!(!waits(&db, "UPDATE p SET v = 9 WHERE id = 20"), "{level:?}");
    }
}

#[test]
fn a_child_insert_of_a_missing_parent_locks_the_gap_it_would_be_in_only_under_repeatable_read() {
    for (level, keeps_the_gap) in [
        (RowLockLevel::RepeatableRead, true),
        (RowLockLevel::ReadCommitted, false),
    ] {
        let db = database_with_foreign_keys("");
        let writer = foreign_key_session(&db, level);
        writer.execute("BEGIN CONCURRENT").unwrap();
        let refused = writer.execute("INSERT INTO c VALUES (5, 25, 0)");
        assert!(
            matches!(refused, Err(LimboError::ForeignKeyConstraint(_))),
            "{refused:?}"
        );
        assert_eq!(
            waits(&db, "INSERT INTO p VALUES (25, 0)"),
            keeps_the_gap,
            "{level:?}"
        );
        assert!(!waits(&db, "INSERT INTO p VALUES (35, 0)"), "{level:?}");
        assert!(!waits(&db, "UPDATE p SET v = 9 WHERE id = 30"), "{level:?}");
        writer.execute("ROLLBACK").unwrap();
    }
}

#[test]
fn a_child_insert_waits_for_the_transaction_that_inserted_or_deleted_its_parent() {
    for level in [RowLockLevel::RepeatableRead, RowLockLevel::ReadCommitted] {
        for ends_with in ["COMMIT", "ROLLBACK"] {
            let db = database_with_foreign_keys("");
            let parent_writer = foreign_key_session(&db, level);
            parent_writer.execute("BEGIN CONCURRENT").unwrap();
            parent_writer
                .execute("INSERT INTO p VALUES (25, 0)")
                .unwrap();
            parent_writer
                .execute("DELETE FROM p WHERE id = 10")
                .unwrap();

            let inserting_a_child_of_the_new_parent = run_in_the_background(
                foreign_key_session(&db, level),
                "INSERT INTO c VALUES (5, 25, 0)",
            );
            let inserting_a_child_of_the_deleted_parent = run_in_the_background(
                foreign_key_session(&db, level),
                "INSERT INTO c VALUES (6, 10, 0)",
            );
            assert!(still_waits(&inserting_a_child_of_the_new_parent));
            assert!(still_waits(&inserting_a_child_of_the_deleted_parent));
            parent_writer.execute(ends_with).unwrap();
            let (_, new_parent) = inserting_a_child_of_the_new_parent.join().unwrap();
            let (_, deleted_parent) = inserting_a_child_of_the_deleted_parent.join().unwrap();
            let committed = ends_with == "COMMIT";
            assert_eq!(new_parent.is_ok(), committed, "{level:?} {ends_with}");
            assert_eq!(deleted_parent.is_ok(), !committed, "{level:?} {ends_with}");
        }
    }
}

#[test]
fn a_parent_delete_locks_the_child_index_entry_it_found_and_not_the_child_row() {
    for level in [RowLockLevel::RepeatableRead, RowLockLevel::ReadCommitted] {
        let db = database_with_foreign_keys("");
        let writer = foreign_key_session(&db, level);
        writer.execute("BEGIN CONCURRENT").unwrap();
        let refused = writer.execute("DELETE FROM p WHERE id = 20");
        assert!(
            matches!(refused, Err(LimboError::ForeignKeyConstraint(_))),
            "{refused:?}"
        );

        assert!(waits(&db, "DELETE FROM c WHERE id = 1"), "{level:?}");
        assert!(
            waits(&db, "UPDATE c SET pid = 30 WHERE id = 1"),
            "{level:?}"
        );
        assert!(!waits(&db, "UPDATE c SET v = 9 WHERE id = 1"), "{level:?}");
        assert!(!waits(&db, "DELETE FROM c WHERE id = 3"), "{level:?}");
        writer.execute("ROLLBACK").unwrap();
    }
}

#[test]
fn a_parent_delete_without_children_locks_the_gap_in_the_child_index_only_under_repeatable_read() {
    for (level, keeps_the_gap) in [
        (RowLockLevel::RepeatableRead, true),
        (RowLockLevel::ReadCommitted, false),
    ] {
        let db = database_with_foreign_keys("");
        let writer = foreign_key_session(&db, level);
        writer.execute("BEGIN CONCURRENT").unwrap();
        writer.execute("DELETE FROM p WHERE id = 10").unwrap();

        assert_eq!(
            waits(&db, "INSERT INTO c VALUES (7, 15, 0)"),
            keeps_the_gap,
            "{level:?}"
        );
        assert!(!waits(&db, "INSERT INTO c VALUES (8, 25, 0)"), "{level:?}");
        assert!(!waits(&db, "UPDATE c SET v = 9 WHERE id = 1"), "{level:?}");
        writer.execute("ROLLBACK").unwrap();
    }
}

#[test]
fn a_cascade_locks_the_child_rows_it_changes_and_only_the_gap_where_its_scan_stops() {
    for action in ["ON DELETE CASCADE", "ON DELETE SET NULL"] {
        let db = database_with_foreign_keys(action);
        let writer = foreign_key_session(&db, RowLockLevel::RepeatableRead);
        writer.execute("BEGIN CONCURRENT").unwrap();
        writer.execute("DELETE FROM p WHERE id = 20").unwrap();

        assert!(waits(&db, "UPDATE c SET v = 9 WHERE id = 1"), "{action}");
        assert!(waits(&db, "UPDATE c SET v = 9 WHERE id = 2"), "{action}");
        assert!(!waits(&db, "UPDATE c SET v = 9 WHERE id = 3"), "{action}");
        assert!(!waits(&db, "INSERT INTO c VALUES (7, 15, 0)"), "{action}");
        assert!(waits(&db, "INSERT INTO c VALUES (8, 25, 0)"), "{action}");
        writer.execute("ROLLBACK").unwrap();
    }
}

#[test]
fn two_child_inserts_that_both_update_their_parent_deadlock() {
    let db = database_with_foreign_keys("");
    let first = foreign_key_session(&db, RowLockLevel::RepeatableRead);
    let second = foreign_key_session(&db, RowLockLevel::RepeatableRead);
    first.execute("BEGIN CONCURRENT").unwrap();
    second.execute("BEGIN CONCURRENT").unwrap();
    first.execute("INSERT INTO c VALUES (5, 20, 0)").unwrap();
    second.execute("INSERT INTO c VALUES (6, 20, 0)").unwrap();

    let waiting = run_in_the_background(first, "UPDATE p SET v = 1 WHERE id = 20");
    assert!(still_waits(&waiting));
    let closing = second.execute("UPDATE p SET v = 2 WHERE id = 20");
    assert!(
        matches!(closing, Err(LimboError::WriteWriteConflict)),
        "{closing:?}"
    );
    let (first, result) = waiting.join().unwrap();
    result.unwrap();
    first.execute("COMMIT").unwrap();
    let values: Vec<(i64,)> = first.exec_rows("SELECT v FROM p WHERE id = 20");
    assert_eq!(values, [(1,)]);
}

#[test]
fn a_self_referencing_delete_checks_the_children_through_their_index() {
    let db = database_with_row_locks();
    let setup = db.connect_limbo();
    setup
        .execute(
            "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, parent INT REFERENCES t (id), v INT)",
        )
        .unwrap();
    setup
        .execute("CREATE INDEX t_parent ON t (parent)")
        .unwrap();
    setup
        .execute(
            "INSERT INTO t VALUES (10, NULL, 0), (20, 10, 0), (30, 10, 0), (40, 30, 0), (50, 50, 0)",
        )
        .unwrap();
    let writer = foreign_key_session(&db, RowLockLevel::RepeatableRead);
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer.execute("DELETE FROM t WHERE id = 20").unwrap();
    let refused = writer.execute("DELETE FROM t WHERE id = 30");
    assert!(
        matches!(refused, Err(LimboError::ForeignKeyConstraint(_))),
        "{refused:?}"
    );

    assert!(waits(&db, "UPDATE t SET parent = 10 WHERE id = 40"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 40"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 10"));
    assert!(!waits(&db, "UPDATE t SET v = 9 WHERE id = 50"));
    assert!(waits(&db, "INSERT INTO t VALUES (60, 25, 0)"));
    assert!(!waits(&db, "INSERT INTO t VALUES (70, 5, 0)"));
    writer.execute("ROLLBACK").unwrap();
}

#[test]
fn a_child_insert_locks_its_parent_row_by_every_column_of_a_composite_key() {
    let db = database_with_row_locks();
    let setup = db.connect_limbo();
    setup
        .execute("CREATE TABLE pc (a INT NOT NULL, b INT NOT NULL, v INT, PRIMARY KEY (a, b))")
        .unwrap();
    setup
        .execute(
            "CREATE TABLE cc (id INT NOT NULL PRIMARY KEY, a INT, b INT, FOREIGN KEY (a, b) REFERENCES pc (a, b))",
        )
        .unwrap();
    setup.execute("CREATE INDEX cc_ab ON cc (a, b)").unwrap();
    setup
        .execute("INSERT INTO pc VALUES (1, 1, 0), (1, 2, 0), (2, 1, 0)")
        .unwrap();
    let writer = foreign_key_session(&db, RowLockLevel::RepeatableRead);
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer.execute("INSERT INTO cc VALUES (5, 1, 2)").unwrap();
    let refused = writer.execute("INSERT INTO cc VALUES (6, 1, 3)");
    assert!(
        matches!(refused, Err(LimboError::ForeignKeyConstraint(_))),
        "{refused:?}"
    );

    assert!(waits(&db, "DELETE FROM pc WHERE a = 1 AND b = 2"));
    assert!(!waits(&db, "DELETE FROM pc WHERE a = 1 AND b = 1"));
    assert!(waits(&db, "INSERT INTO pc VALUES (1, 3, 0)"));
    assert!(!waits(&db, "INSERT INTO pc VALUES (2, 5, 0)"));
    writer.execute("ROLLBACK").unwrap();
}

#[test]
fn a_child_update_that_keeps_its_index_entry_locks_no_parent_when_checked_row_by_row() {
    let db = database_with_foreign_keys("");
    let writer = foreign_key_session(&db, RowLockLevel::RepeatableRead);
    writer.set_foreign_keys_checked_row_by_row(true);
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer
        .execute("UPDATE c SET pid = 20, v = 1 WHERE id = 1")
        .unwrap();
    assert!(!waits(&db, "UPDATE p SET v = 9 WHERE id = 20"));
    writer.execute("UPDATE c SET id = 7 WHERE id = 3").unwrap();
    assert!(waits(&db, "UPDATE p SET v = 9 WHERE id = 30"));
    writer.execute("ROLLBACK").unwrap();
}

#[test]
fn a_parent_delete_checked_row_by_row_stops_at_the_first_parent_a_child_names() {
    let db = database_with_foreign_keys("");
    let writer = foreign_key_session(&db, RowLockLevel::RepeatableRead);
    writer.set_foreign_keys_checked_row_by_row(true);
    writer.execute("BEGIN CONCURRENT").unwrap();
    let refused = writer.execute("DELETE FROM p");
    assert!(
        matches!(&refused, Err(LimboError::ForeignKeyConstraint(message)) if message == turso_core::FOREIGN_KEY_PARENT_ROW_REFERENCED),
        "{refused:?}"
    );
    assert!(waits(&db, "UPDATE p SET v = 9 WHERE id = 10"));
    assert!(waits(&db, "UPDATE p SET v = 9 WHERE id = 20"));
    assert!(!waits(&db, "UPDATE p SET v = 9 WHERE id = 30"));
    writer.execute("ROLLBACK").unwrap();
}

#[test]
fn a_parent_key_update_locks_nothing_around_the_new_key() {
    let db = database_with_foreign_keys("");
    let writer = foreign_key_session(&db, RowLockLevel::RepeatableRead);
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer
        .execute("UPDATE p SET id = 25 WHERE id = 10")
        .unwrap();
    assert!(waits(&db, "INSERT INTO c VALUES (7, 15, 0)"));
    assert!(!waits(&db, "INSERT INTO c VALUES (8, 25, 0)"));
    writer.execute("ROLLBACK").unwrap();
}

fn database_with_foreign_keys(action: &str) -> TempDatabase {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_mvcc_row_locks(true))
        .with_mvcc(true)
        .build();
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE p (id INT NOT NULL PRIMARY KEY, v INT)")
        .unwrap();
    conn.execute(format!(
        "CREATE TABLE c (id INT NOT NULL PRIMARY KEY, pid INT REFERENCES p (id) {action}, v INT)"
    ))
    .unwrap();
    conn.execute("CREATE INDEX c_pid ON c (pid)").unwrap();
    conn.execute("INSERT INTO p VALUES (10, 1), (20, 2), (30, 3)")
        .unwrap();
    conn.execute("INSERT INTO c VALUES (1, 20, 0), (2, 20, 0), (3, 30, 0)")
        .unwrap();
    db
}

fn foreign_key_session(db: &TempDatabase, level: RowLockLevel) -> Arc<Connection> {
    let conn = session(db);
    conn.set_row_lock_level(level);
    conn.execute("PRAGMA foreign_keys = ON").unwrap();
    conn
}
