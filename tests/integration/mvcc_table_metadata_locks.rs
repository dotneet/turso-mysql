use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::common::{ExecRows, TempDatabase};
use turso_core::{Connection, DatabaseOpts, LimboError, MetadataLockMode};

const LONG_ENOUGH_TO_SEE_A_WAIT: Duration = Duration::from_millis(300);

fn database_with_table_locks() -> TempDatabase {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_mvcc_row_locks(true))
        .with_mvcc(true)
        .build();
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    conn.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 10), (2, 20)")
        .unwrap();
    conn.execute("INSERT INTO u VALUES (1, 10), (2, 20)")
        .unwrap();
    db
}

fn session(db: &TempDatabase) -> Arc<Connection> {
    let conn = db.connect_limbo();
    conn.set_busy_timeout(Duration::from_secs(10));
    conn.set_metadata_lock_wait(Duration::from_secs(10));
    conn
}

fn in_the_background<T: Send + 'static>(
    conn: Arc<Connection>,
    run: impl FnOnce(&Arc<Connection>) -> turso_core::Result<T> + Send + 'static,
) -> JoinHandle<(Arc<Connection>, turso_core::Result<T>)> {
    std::thread::spawn(move || {
        let result = run(&conn);
        (conn, result)
    })
}

fn still_waits<T>(handle: &JoinHandle<T>) -> bool {
    std::thread::sleep(LONG_ENOUGH_TO_SEE_A_WAIT);
    !handle.is_finished()
}

fn count(conn: &Arc<Connection>, table: &str) -> turso_core::Result<i64> {
    let mut statement = conn.prepare(format!("SELECT COUNT(*) FROM {table}"))?;
    let rows = statement.run_collect_rows()?;
    Ok(rows[0][0].as_int().expect("a count is a whole number"))
}

#[test]
fn a_definition_lock_waits_for_a_reader_and_new_readers_wait_behind_it() {
    let db = database_with_table_locks();
    let reader = session(&db);
    let definer = session(&db);
    let late_reader = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(count(&reader, "t").unwrap(), 2);

    let defining = in_the_background(definer, |conn| {
        conn.lock_tables_metadata(&[("t", MetadataLockMode::Exclusive)])
    });
    assert!(still_waits(&defining));
    let late_reading = in_the_background(late_reader, |conn| count(conn, "t"));
    assert!(still_waits(&late_reading));
    assert_eq!(count(&reader, "u").unwrap(), 2);
    assert_eq!(count(&reader, "t").unwrap(), 2);

    reader.execute("COMMIT").unwrap();
    let (definer, defined) = defining.join().unwrap();
    defined.unwrap();
    assert!(still_waits(&late_reading));
    definer.release_metadata_locks_outside_a_transaction();
    let (_, late_count) = late_reading.join().unwrap();
    assert_eq!(late_count.unwrap(), 2);
}

#[test]
fn a_table_lock_wait_gives_up_with_busy_after_the_metadata_lock_wait() {
    let db = database_with_table_locks();
    let writer = session(&db);
    let definer = session(&db);
    definer.set_metadata_lock_wait(Duration::from_millis(400));
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();

    let started = Instant::now();
    let refused = definer.lock_tables_metadata(&[("t", MetadataLockMode::SharedReadOnly)]);
    assert!(matches!(refused, Err(LimboError::Busy)), "{refused:?}");
    assert!(started.elapsed() >= Duration::from_millis(400));
    definer.release_metadata_locks_outside_a_transaction();

    definer
        .lock_tables_metadata(&[("u", MetadataLockMode::SharedNoReadWrite)])
        .unwrap();
    writer.execute("COMMIT").unwrap();
    definer.release_metadata_locks_outside_a_transaction();
}

#[test]
fn a_table_locked_for_writing_keeps_other_sessions_off_it_alone() {
    let db = database_with_table_locks();
    let locker = session(&db);
    let other = session(&db);
    other.set_metadata_lock_wait(Duration::from_millis(300));
    locker.execute("BEGIN CONCURRENT").unwrap();
    locker
        .lock_tables_metadata(&[("t", MetadataLockMode::SharedNoReadWrite)])
        .unwrap();
    locker.execute("UPDATE t SET v = 99 WHERE id = 1").unwrap();

    other.execute("BEGIN CONCURRENT").unwrap();
    other.execute("UPDATE u SET v = 7 WHERE id = 1").unwrap();
    other.execute("COMMIT").unwrap();
    assert!(matches!(count(&other, "t"), Err(LimboError::Busy)));
    assert!(matches!(
        other.execute("INSERT INTO t VALUES (3, 30)"),
        Err(LimboError::Busy)
    ));

    locker.execute("COMMIT").unwrap();
    assert_eq!(count(&other, "t").unwrap(), 2);
    let rows: Vec<(i64,)> = other.exec_rows("SELECT v FROM t WHERE id = 1");
    assert_eq!(rows, [(99,)]);
}

#[test]
fn a_reader_asking_to_write_behind_a_waiting_definition_lock_is_rolled_back_as_a_deadlock() {
    let db = database_with_table_locks();
    let reader = session(&db);
    let definer = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    reader.execute("INSERT INTO u VALUES (3, 30)").unwrap();
    assert_eq!(count(&reader, "t").unwrap(), 2);

    let defining = in_the_background(definer, |conn| {
        conn.lock_tables_metadata(&[("t", MetadataLockMode::Exclusive)])
    });
    assert!(still_waits(&defining));
    assert!(matches!(
        reader.execute("INSERT INTO t VALUES (3, 30)"),
        Err(LimboError::WriteWriteConflict)
    ));
    assert!(reader.get_auto_commit());

    let (definer, defined) = defining.join().unwrap();
    defined.unwrap();
    definer.release_metadata_locks_outside_a_transaction();
    assert_eq!(count(&reader, "u").unwrap(), 2);
}

#[test]
fn a_writer_of_one_table_commits_after_another_session_changed_a_second_table() {
    let db = database_with_table_locks();
    let writer = session(&db);
    let definer = session(&db);
    writer.execute("BEGIN CONCURRENT").unwrap();
    writer.execute("UPDATE u SET v = 11 WHERE id = 1").unwrap();

    definer
        .execute("ALTER TABLE t ADD COLUMN w INTEGER")
        .unwrap();
    definer
        .execute("CREATE TABLE d (id INTEGER PRIMARY KEY)")
        .unwrap();

    writer.execute("UPDATE u SET v = 12 WHERE id = 2").unwrap();
    writer.execute("COMMIT").unwrap();
    let rows: Vec<(i64,)> = definer.exec_rows("SELECT v FROM u ORDER BY id");
    assert_eq!(rows, [(11,), (12,)]);
}

#[test]
fn a_transaction_that_reads_a_table_changed_after_its_snapshot_is_told_the_definition_changed() {
    let db = database_with_table_locks();
    let reader = session(&db);
    let definer = session(&db);
    reader.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(count(&reader, "u").unwrap(), 2);

    definer
        .execute("ALTER TABLE t ADD COLUMN w INTEGER")
        .unwrap();

    assert!(matches!(
        count(&reader, "t"),
        Err(LimboError::TableDefinitionChanged(table)) if table == "t"
    ));
    assert!(!reader.get_auto_commit());
    reader.execute("UPDATE u SET v = 5 WHERE id = 1").unwrap();
    reader.execute("COMMIT").unwrap();
    assert_eq!(count(&reader, "t").unwrap(), 2);
}

#[test]
fn a_writer_waiting_for_a_table_another_session_redefines_runs_on_the_new_definition() {
    let db = database_with_table_locks();
    let definer = session(&db);
    let writer = session(&db);
    definer
        .lock_tables_metadata(&[("t", MetadataLockMode::Exclusive)])
        .unwrap();
    writer.execute("BEGIN CONCURRENT").unwrap();
    let writing = in_the_background(writer, |conn| {
        conn.execute("INSERT INTO t (id, v) VALUES (3, 30)")
    });
    assert!(still_waits(&writing));
    definer
        .execute("ALTER TABLE t ADD COLUMN w INTEGER DEFAULT 7")
        .unwrap();
    definer.release_metadata_locks_outside_a_transaction();

    let (writer, written) = writing.join().unwrap();
    written.unwrap();
    writer.execute("COMMIT").unwrap();
    let rows: Vec<(i64, i64)> = writer.exec_rows("SELECT id, w FROM t ORDER BY id");
    assert_eq!(rows, [(1, 7), (2, 7), (3, 7)]);
}

#[test]
fn a_cycle_through_a_row_lock_and_a_waiting_definition_lock_rolls_back_the_lightest_transaction() {
    let db = database_with_table_locks();
    let row_holder = session(&db);
    let row_waiter = session(&db);
    let definer = session(&db);
    row_holder.execute("BEGIN CONCURRENT").unwrap();
    row_holder
        .execute("UPDATE t SET v = 11 WHERE id = 1")
        .unwrap();
    row_waiter.execute("BEGIN CONCURRENT").unwrap();
    assert_eq!(count(&row_waiter, "u").unwrap(), 2);
    let row_waiting = in_the_background(row_waiter, |conn| {
        conn.execute("UPDATE t SET v = 12 WHERE id = 1")
    });
    assert!(still_waits(&row_waiting));
    let defining = in_the_background(definer, |conn| {
        conn.lock_tables_metadata(&[("u", MetadataLockMode::Exclusive)])
    });
    assert!(still_waits(&defining));

    let reading = in_the_background(row_holder, |conn| count(conn, "u"));
    let (row_waiter, waited) = row_waiting.join().unwrap();
    assert!(
        matches!(waited, Err(LimboError::WriteWriteConflict)),
        "{waited:?}"
    );
    assert!(row_waiter.get_auto_commit());
    let (definer, defined) = defining.join().unwrap();
    defined.unwrap();
    assert!(still_waits(&reading));
    definer.release_metadata_locks_outside_a_transaction();
    let (row_holder, read) = reading.join().unwrap();
    assert_eq!(read.unwrap(), 2);
    row_holder.execute("COMMIT").unwrap();
}
