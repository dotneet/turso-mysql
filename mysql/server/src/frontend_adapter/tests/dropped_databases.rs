//! `DROP DATABASE` seen from the session that drops and from the sessions
//! that still have the database selected.
//!
//! Every expected answer was measured on MySQL 8.4.11 with two sessions.

use super::*;
use std::time::{Duration, Instant};

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn catalog() -> (
    tempfile::TempDir,
    Arc<MySqlDatabaseCatalog>,
    Arc<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, _factory) = catalog_factory(Arc::clone(&authorizer));
    catalog.create("shop").unwrap();
    catalog.create("other").unwrap();
    (directory, catalog, authorizer)
}

fn session(
    catalog: &Arc<MySqlDatabaseCatalog>,
    authorizer: &Arc<RecordingAuthorizer>,
    database: &str,
) -> Adapter {
    let factory = AuthorizedDatabaseAdapterFactory::new(
        Arc::clone(catalog),
        binary_context(),
        Arc::clone(authorizer),
    );
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([171; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db(database).unwrap();
    adapter
}

fn run(adapter: &mut Adapter, sql: &str) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(ok)) => ok,
        Ok(other) => panic!("{sql}: expected OK, got {other:?}"),
        Err(error) => panic!("{sql}: {error:?} {:?}", adapter.take_error_message()),
    }
}

fn words(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                    .collect()
            })
            .collect(),
        other => panic!(
            "{sql} must return a result set, got {other:?} {:?}",
            adapter.take_error_message()
        ),
    }
}

fn one(value: &str) -> Vec<Vec<Option<String>>> {
    vec![vec![Some(value.to_owned())]]
}

fn refusal(adapter: &mut Adapter, sql: &str) -> (FrontendErrorKind, String) {
    let error = adapter
        .execute_query(sql)
        .expect_err("the statement must be refused");
    let message = adapter
        .take_error_message()
        .map(|message| String::from_utf8(message).unwrap())
        .unwrap_or_default();
    (error, message)
}

/// `DROP DATABASE IF EXISTS` of a database that is not there answers OK
/// counting one warning that `SHOW WARNINGS` does not list, commits the
/// session's transaction first, and counts nothing under `sql_notes = 0`.
#[test]
fn drop_database_if_exists_answers_a_missing_database_with_ok() {
    let (_directory, catalog, authorizer) = catalog();
    let mut adapter = session(&catalog, &authorizer, "shop");
    run(&mut adapter, "CREATE TABLE kept (id INT PRIMARY KEY)");
    run(&mut adapter, "DROP TABLE IF EXISTS nothing_here");
    run(&mut adapter, "BEGIN");
    run(&mut adapter, "INSERT INTO kept VALUES (1)");

    let answer = run(&mut adapter, "DROP DATABASE IF EXISTS nowhere");
    assert_eq!((answer.affected_rows, answer.warnings), (0, 1));
    assert!(words(&mut adapter, "SHOW WARNINGS").is_empty());
    assert_eq!(words(&mut adapter, "SHOW COUNT(*) WARNINGS"), one("0"));
    run(&mut adapter, "ROLLBACK");
    assert_eq!(words(&mut adapter, "SELECT COUNT(*) FROM kept"), one("1"));

    assert_eq!(
        run(&mut adapter, "drop schema if exists `nowhere`;").warnings,
        1
    );
    run(&mut adapter, "SET sql_notes = 0");
    assert_eq!(
        run(&mut adapter, "DROP DATABASE IF EXISTS nowhere").warnings,
        0
    );

    run(&mut adapter, "SET sql_notes = 1");
    assert_eq!(
        run(&mut adapter, "DROP DATABASE IF EXISTS other").warnings,
        0
    );
    assert!(!catalog.list().unwrap().contains(&"other".to_owned()));
    assert_eq!(
        refusal(&mut adapter, "DROP DATABASE other"),
        (
            FrontendErrorKind::NoDatabaseToDrop,
            "Can't drop database 'other'; database doesn't exist".to_owned()
        )
    );
}

/// A session whose database another session dropped keeps its name through a
/// plain `DROP DATABASE` of it, which answers 1008, and is left in none by
/// `DROP DATABASE IF EXISTS`, as though it had dropped it itself.
#[test]
fn drop_database_if_exists_leaves_a_session_whose_database_went_in_none() {
    let (_directory, catalog, authorizer) = catalog();
    let mut left_behind = session(&catalog, &authorizer, "shop");
    let mut dropper = session(&catalog, &authorizer, "other");
    run(&mut dropper, "DROP DATABASE shop");

    assert_eq!(
        refusal(&mut left_behind, "DROP DATABASE shop").0,
        FrontendErrorKind::NoDatabaseToDrop
    );
    assert_eq!(words(&mut left_behind, "SELECT DATABASE()"), one("shop"));
    assert_eq!(
        run(&mut left_behind, "DROP DATABASE IF EXISTS shop").warnings,
        1
    );
    assert_eq!(
        words(&mut left_behind, "SELECT DATABASE()"),
        vec![vec![None]]
    );
}

/// `lock_wait_timeout` starts at a year and reads back what the session set,
/// from one second to a year; MySQL clamps anything else with warning 1292,
/// which is refused here.
#[test]
fn lock_wait_timeout_reads_back_what_the_session_set() {
    let (_directory, catalog, authorizer) = catalog();
    let mut adapter = session(&catalog, &authorizer, "shop");
    assert_eq!(
        words(&mut adapter, "SELECT @@lock_wait_timeout"),
        one("31536000")
    );
    run(&mut adapter, "SET SESSION lock_wait_timeout = 7");
    assert_eq!(
        words(&mut adapter, "SELECT @@session.lock_wait_timeout"),
        one("7")
    );
    assert_eq!(
        words(&mut adapter, "SHOW VARIABLES LIKE 'lock_wait_timeout'"),
        vec![vec![
            Some("lock_wait_timeout".to_owned()),
            Some("7".to_owned())
        ]]
    );
    for refused in [
        "SET lock_wait_timeout = 0",
        "SET lock_wait_timeout = 31536001",
    ] {
        assert!(adapter.execute_query(refused).is_err(), "{refused}");
    }
    run(&mut adapter, "SET lock_wait_timeout = DEFAULT");
    assert_eq!(
        words(&mut adapter, "SELECT @@lock_wait_timeout"),
        one("31536000")
    );
}

/// A `DROP DATABASE` waits for a transaction that read one of the database's
/// tables only as long as the dropping session's `lock_wait_timeout`, then
/// answers 1205 with MySQL's message and leaves the database as it was.
#[test]
fn drop_database_gives_up_after_the_sessions_lock_wait_timeout() {
    let (_directory, catalog, authorizer) = catalog();
    let mut holder = session(&catalog, &authorizer, "shop");
    run(&mut holder, "CREATE TABLE kept (id INT PRIMARY KEY)");
    run(&mut holder, "BEGIN");
    assert!(words(&mut holder, "SELECT id FROM kept").is_empty());

    let mut dropper = session(&catalog, &authorizer, "other");
    run(&mut dropper, "SET lock_wait_timeout = 1");
    let started = Instant::now();
    assert_eq!(
        dropper.execute_query("DROP DATABASE shop"),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        crate::response::map_frontend_error(FrontendErrorKind::DatabaseBusy).message,
        b"Lock wait timeout exceeded; try restarting transaction"
    );

    assert!(words(&mut holder, "SELECT id FROM kept").is_empty());
    run(&mut holder, "COMMIT");
    run(&mut dropper, "DROP DATABASE shop");
}

/// While a `DROP DATABASE` waits, a statement another session starts on the
/// database, and selecting it, each wait that session's own
/// `lock_wait_timeout` and answer 1205, the selection leaving the session
/// where it was.
#[test]
fn a_statement_behind_a_waiting_drop_gives_up_after_its_own_lock_wait_timeout() {
    let (_directory, catalog, authorizer) = catalog();
    let mut holder = session(&catalog, &authorizer, "shop");
    run(&mut holder, "CREATE TABLE kept (id INT PRIMARY KEY)");
    run(&mut holder, "BEGIN");
    assert!(words(&mut holder, "SELECT id FROM kept").is_empty());

    let mut waiting = session(&catalog, &authorizer, "shop");
    run(&mut waiting, "SET lock_wait_timeout = 1");
    let mut selecting = session(&catalog, &authorizer, "other");
    run(&mut selecting, "SET lock_wait_timeout = 1");
    let dropper = {
        let mut dropper = session(&catalog, &authorizer, "other");
        run(&mut dropper, "SET lock_wait_timeout = 60");
        std::thread::spawn(move || dropper.execute_query("DROP DATABASE shop").map(|_| ()))
    };
    // Until the drop starts waiting, a statement on the database runs.
    let waited_from = Instant::now();
    let started = loop {
        let started = Instant::now();
        if waiting.execute_query("SELECT id FROM kept").is_err() {
            break started;
        }
        assert!(waited_from.elapsed() < Duration::from_secs(20));
    };
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(
        refusal(&mut waiting, "SELECT id FROM kept").0,
        FrontendErrorKind::DatabaseBusy
    );
    assert_eq!(
        selecting.execute_init_db("shop"),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    assert_eq!(words(&mut selecting, "SELECT DATABASE()"), one("other"));

    run(&mut holder, "COMMIT");
    assert_eq!(dropper.join().unwrap(), Ok(()));
    assert_eq!(
        refusal(&mut waiting, "SELECT id FROM kept").0,
        FrontendErrorKind::UnknownDatabase
    );
}

/// A statement changing a table's definition, and `LOCK TABLES`, wait for
/// another session's write as long as `lock_wait_timeout` says, not
/// `innodb_lock_wait_timeout`.
#[test]
fn table_definition_changes_wait_as_long_as_lock_wait_timeout() {
    let (_directory, catalog, authorizer) = catalog();
    let mut writer = session(&catalog, &authorizer, "shop");
    run(&mut writer, "CREATE TABLE kept (id INT PRIMARY KEY, v INT)");
    run(&mut writer, "BEGIN");
    run(&mut writer, "INSERT INTO kept VALUES (1, 1)");

    let mut changer = session(&catalog, &authorizer, "shop");
    run(&mut changer, "SET lock_wait_timeout = 1");
    run(&mut changer, "SET innodb_lock_wait_timeout = 60");
    for statement in [
        "ALTER TABLE kept ADD COLUMN w INT",
        "TRUNCATE TABLE kept",
        "DROP TABLE kept",
        "CREATE INDEX kept_v ON kept (v)",
        "LOCK TABLES kept WRITE",
    ] {
        let started = Instant::now();
        assert_eq!(
            changer.execute_query(statement).err(),
            Some(FrontendErrorKind::DatabaseBusy),
            "{statement}"
        );
        assert!(started.elapsed() < Duration::from_secs(30), "{statement}");
    }
    run(&mut writer, "COMMIT");
    run(&mut changer, "ALTER TABLE kept ADD COLUMN w INT");
}

/// A transaction holds a `DROP DATABASE` up once it has read the database —
/// a table, or the list of them — and not before: one that has only begun,
/// begun `WITH CONSISTENT SNAPSHOT`, taken a savepoint or run `SELECT 1` lets
/// the drop go at once, and its next read of a table answers 1049.
#[test]
fn only_a_transaction_that_read_the_database_holds_a_drop_up() {
    for (opening, holds) in [
        (&["BEGIN", "SELECT 1"][..], false),
        (&["START TRANSACTION WITH CONSISTENT SNAPSHOT"][..], false),
        (&["BEGIN", "SAVEPOINT s"][..], false),
        (&["SET autocommit = 0", "SELECT 1"][..], false),
        (&["BEGIN", "SELECT 1", "SHOW TABLES", "SELECT 2"][..], true),
        (&["BEGIN", "SELECT id FROM kept", "SAVEPOINT s"][..], true),
        (
            &[
                "BEGIN",
                "SELECT id FROM kept",
                "SAVEPOINT s",
                "ROLLBACK TO SAVEPOINT s",
            ][..],
            true,
        ),
        (
            &["BEGIN", "SELECT id FROM kept", "BEGIN", "SELECT 1"][..],
            false,
        ),
    ] {
        let (_directory, catalog, authorizer) = catalog();
        let mut holder = session(&catalog, &authorizer, "shop");
        run(&mut holder, "CREATE TABLE kept (id INT PRIMARY KEY)");
        for statement in opening {
            assert!(holder.execute_query(statement).is_ok(), "{statement}");
        }
        let mut dropper = session(&catalog, &authorizer, "other");
        run(&mut dropper, "SET lock_wait_timeout = 1");
        let dropped = dropper.execute_query("DROP DATABASE shop");
        if holds {
            assert_eq!(dropped, Err(FrontendErrorKind::DatabaseBusy), "{opening:?}");
            continue;
        }
        assert!(dropped.is_ok(), "{opening:?}: {dropped:?}");
        assert_eq!(
            refusal(&mut holder, "SELECT id FROM kept").0,
            FrontendErrorKind::UnknownDatabase,
            "{opening:?}"
        );
    }
}
