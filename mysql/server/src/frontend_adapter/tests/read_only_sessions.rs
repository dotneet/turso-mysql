//! `SET SESSION TRANSACTION READ ONLY`, which Connector/J sends for every
//! read-only Spring transaction, and the promise it makes.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn session() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("shop").unwrap();
    drop(catalog);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([0x52; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("shop").unwrap();
    for sql in [
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)",
        "INSERT INTO t (id, v) VALUES (1, 1)",
    ] {
        run(&mut adapter, sql);
    }
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn refused(adapter: &mut Adapter, sql: &str) {
    assert_eq!(
        adapter.execute_query(sql).err(),
        Some(FrontendErrorKind::ReadOnlyTransaction),
        "{sql}"
    );
}

fn value(adapter: &mut Adapter, sql: &str) -> String {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap()
}

#[test]
fn a_read_only_session_refuses_every_write_and_keeps_reading() {
    let (_directory, mut adapter) = session();
    run(&mut adapter, "SET SESSION TRANSACTION READ ONLY");
    assert_eq!(value(&mut adapter, "SELECT @@transaction_read_only"), "1");
    assert_eq!(
        value(&mut adapter, "SELECT @@session.transaction_read_only"),
        "1"
    );
    assert_eq!(
        value(&mut adapter, "SELECT @@global.transaction_read_only"),
        "0"
    );
    for sql in [
        "INSERT INTO t (id, v) VALUES (2, 2)",
        "UPDATE t SET v = 9 WHERE id = 1",
        "DELETE FROM t WHERE id = 1",
        "CREATE TABLE u (id INT NOT NULL PRIMARY KEY)",
        "DROP TABLE t",
        "CREATE TEMPORARY TABLE tt (id INT)",
        "SELECT id FROM t WHERE id = 1 FOR UPDATE",
        "CREATE DATABASE other",
    ] {
        refused(&mut adapter, sql);
    }
    assert_eq!(value(&mut adapter, "SELECT v FROM t WHERE id = 1"), "1");
    run(&mut adapter, "SET @x = 1");

    run(&mut adapter, "SET SESSION TRANSACTION READ WRITE");
    assert_eq!(value(&mut adapter, "SELECT @@transaction_read_only"), "0");
    run(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
}

#[test]
fn a_transaction_takes_the_session_mode_unless_it_names_its_own() {
    let (_directory, mut adapter) = session();
    run(&mut adapter, "SET SESSION TRANSACTION READ ONLY");

    run(&mut adapter, "BEGIN");
    refused(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
    // The session's mode changes from the next transaction on.
    run(&mut adapter, "SET SESSION TRANSACTION READ WRITE");
    refused(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
    run(&mut adapter, "ROLLBACK");
    run(&mut adapter, "SET SESSION TRANSACTION READ ONLY");

    run(&mut adapter, "START TRANSACTION READ WRITE");
    run(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
    // @@transaction_read_only reads the session's mode, not the transaction's.
    assert_eq!(value(&mut adapter, "SELECT @@transaction_read_only"), "1");
    // DDL ends the transaction first and answers to the session's mode.
    refused(&mut adapter, "CREATE TABLE u (id INT NOT NULL PRIMARY KEY)");
    run(&mut adapter, "ROLLBACK");
    assert_eq!(value(&mut adapter, "SELECT COUNT(*) FROM t"), "1");
}

#[test]
fn ddl_in_a_read_only_transaction_of_a_read_write_session_runs() {
    let (_directory, mut adapter) = session();
    run(&mut adapter, "START TRANSACTION READ ONLY");
    refused(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
    refused(&mut adapter, "CREATE TEMPORARY TABLE tt (id INT)");
    run(&mut adapter, "CREATE TABLE u (id INT NOT NULL PRIMARY KEY)");
    run(&mut adapter, "INSERT INTO u (id) VALUES (1)");
}

/// With autocommit off, the transaction the first statement reading a table
/// begins keeps the session's mode after the session changes it.
#[test]
fn an_implicit_transaction_keeps_the_mode_it_began_with() {
    let (_directory, mut adapter) = session();
    run(&mut adapter, "SET SESSION TRANSACTION READ ONLY");
    run(&mut adapter, "SET autocommit = 0");
    assert_eq!(value(&mut adapter, "SELECT v FROM t WHERE id = 1"), "1");
    run(&mut adapter, "SET SESSION TRANSACTION READ WRITE");
    refused(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
    run(&mut adapter, "ROLLBACK");
    run(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
    run(&mut adapter, "COMMIT");
}

#[test]
fn a_prepared_write_is_refused_when_it_runs_read_only() {
    let (_directory, mut adapter) = session();
    let insert = adapter
        .execute_stmt_prepare("INSERT INTO t (id, v) VALUES (5, 5)")
        .unwrap();
    let select = adapter
        .execute_stmt_prepare("SELECT v FROM t WHERE id = 1")
        .unwrap();
    run(&mut adapter, "SET SESSION TRANSACTION READ ONLY");
    assert_eq!(
        adapter.execute_stmt_execute(insert.statement_id, &[]).err(),
        Some(FrontendErrorKind::ReadOnlyTransaction)
    );
    assert!(adapter
        .execute_stmt_execute(select.statement_id, &[])
        .is_ok());
    run(&mut adapter, "SET SESSION TRANSACTION READ WRITE");
    assert!(adapter
        .execute_stmt_execute(insert.statement_id, &[])
        .is_ok());
}

#[test]
fn every_session_spelling_is_taken_and_the_next_transaction_alone_is_refused() {
    let (_directory, mut adapter) = session();
    for (sql, read_back) in [
        ("SET transaction_read_only = ON", "1"),
        ("SET SESSION transaction_read_only = OFF", "0"),
        ("SET @@session.transaction_read_only = 1", "1"),
        ("SET SESSION transaction_read_only = DEFAULT", "0"),
        ("SET LOCAL TRANSACTION READ ONLY", "1"),
        (
            "SET SESSION TRANSACTION READ WRITE, ISOLATION LEVEL READ COMMITTED",
            "0",
        ),
    ] {
        run(&mut adapter, sql);
        assert_eq!(
            value(&mut adapter, "SELECT @@transaction_read_only"),
            read_back,
            "{sql}"
        );
    }
    assert_eq!(
        value(&mut adapter, "SELECT @@transaction_isolation"),
        "READ-COMMITTED"
    );
    // MySQL keeps these for the next transaction until a transaction ends,
    // failed statements outside one included, which this server does not.
    for sql in [
        "SET TRANSACTION READ ONLY",
        "SET @@transaction_read_only = 1",
    ] {
        assert_eq!(
            adapter.execute_query(sql).err(),
            Some(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    assert!(adapter
        .execute_query("SET SESSION transaction_read_only = 2")
        .is_err());
    assert_eq!(value(&mut adapter, "SELECT @@transaction_read_only"), "0");
}

#[test]
fn resetting_the_connection_makes_the_session_read_write_again() {
    let (_directory, mut adapter) = session();
    run(&mut adapter, "SET SESSION TRANSACTION READ ONLY");
    adapter.execute_reset_connection().unwrap();
    adapter.execute_init_db("shop").unwrap();
    assert_eq!(value(&mut adapter, "SELECT @@transaction_read_only"), "0");
    run(&mut adapter, "INSERT INTO t (id, v) VALUES (2, 2)");
}
