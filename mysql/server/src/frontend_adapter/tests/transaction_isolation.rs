//! What one session sees of what another commits, at each level.
//!
//! Every expectation here was measured on MySQL 8.4.11 with two sessions
//! taking turns over a two-row table.

use super::*;

struct TwoSessions {
    _directory: tempfile::TempDir,
    one: AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    two: AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
}

fn two_sessions() -> TwoSessions {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("ledger").unwrap();
    let second_factory =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer);
    let mut one = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([71; 32]),
        ))
        .unwrap();
    one.authorize_connection().unwrap();
    one.execute_init_db("ledger").unwrap();
    for sql in [
        "CREATE TABLE c (id INT NOT NULL PRIMARY KEY, n INT)",
        "INSERT INTO c (id, n) VALUES (1, 0), (2, 0)",
    ] {
        one.execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    let mut two = second_factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([72; 32]),
        ))
        .unwrap();
    two.authorize_connection().unwrap();
    two.execute_init_db("ledger").unwrap();
    TwoSessions {
        _directory: directory,
        one,
        two,
    }
}

fn run(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn n_of(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>, id: i64) -> String {
    let sql = format!("SELECT n FROM c WHERE id = {id}");
    let outcome = adapter.execute_query(&sql);
    let Ok(CommandExecutionResult::ResultSet(result)) = outcome else {
        panic!("{sql} must return a result set: {outcome:?}");
    };
    String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap()
}

fn level_of(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>) -> String {
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SELECT @@transaction_isolation")
    else {
        panic!("the level must read back");
    };
    String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap()
}

#[test]
fn repeatable_read_keeps_its_snapshot_and_gives_up_a_stale_write_with_1213() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    assert_eq!(level_of(&mut one), "REPEATABLE-READ");

    run(&mut one, "START TRANSACTION");
    assert_eq!(n_of(&mut one, 1), "0");
    run(&mut two, "UPDATE c SET n = 5 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "0");

    // MySQL writes here and goes on reading row 2 as 0, which a snapshot of
    // the whole database cannot do, so the transaction is given up instead.
    assert_eq!(
        one.execute_query("UPDATE c SET n = 7 WHERE id = 1"),
        Err(FrontendErrorKind::SerializationFailure)
    );
    // Measured on MySQL 8.4.11: after 1213 the session is in no transaction.
    assert_eq!(one.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    assert_eq!(n_of(&mut one, 2), "5");
    run(&mut one, "UPDATE c SET n = 7 WHERE id = 1");
    assert_eq!(n_of(&mut two, 1), "7");
}

/// Measured on MySQL 8.4.11: a transaction that read one table writes it
/// after another session committed a row into a second table, and goes on
/// reading the second table as its first read found it (no rows). Here the
/// write goes ahead too, because nothing the transaction read changed; it
/// then reads the row, as a transaction begun after that commit would.
#[test]
fn repeatable_read_writes_after_another_session_committed_to_a_table_it_did_not_read() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    run(&mut one, "CREATE TABLE d (id INT NOT NULL PRIMARY KEY)");

    run(&mut one, "START TRANSACTION");
    assert_eq!(n_of(&mut one, 1), "0");
    run(&mut two, "INSERT INTO d (id) VALUES (1)");
    run(&mut one, "UPDATE c SET n = 7 WHERE id = 1");
    let Ok(CommandExecutionResult::ResultSet(result)) = one.execute_query("SELECT COUNT(*) FROM d")
    else {
        panic!("the count must read back");
    };
    assert_eq!(result.rows[0][0].as_deref(), Some(&b"1"[..]));
    run(&mut one, "COMMIT");
    assert_eq!(n_of(&mut two, 1), "7");
}

#[test]
fn a_transaction_given_up_with_1213_takes_its_savepoints_with_it() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    run(&mut one, "START TRANSACTION");
    run(&mut one, "SAVEPOINT before_reading");
    assert_eq!(n_of(&mut one, 1), "0");
    run(&mut two, "UPDATE c SET n = 5 WHERE id = 2");
    assert_eq!(
        one.execute_query("UPDATE c SET n = 7 WHERE id = 1"),
        Err(FrontendErrorKind::SerializationFailure)
    );
    // The whole transaction is rolled back, the way MySQL rolls back one it
    // answers 1213 for, so nothing it marked is left to return to.
    assert_eq!(
        one.execute_query("ROLLBACK TO SAVEPOINT before_reading"),
        Err(FrontendErrorKind::NoSuchSavepoint)
    );
}

/// Measured on MySQL 8.4.11: a `SAVEPOINT` takes no read view, so a
/// transaction that begins with one sees what another session commits until
/// its first read, and holds what it read from then on.
#[test]
fn a_savepoint_before_the_first_read_takes_no_snapshot() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    run(&mut one, "START TRANSACTION");
    run(&mut one, "SAVEPOINT early");
    run(&mut two, "UPDATE c SET n = 5 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "5");
    run(&mut two, "UPDATE c SET n = 6 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "5");
    run(&mut one, "COMMIT");

    // The savepoint still undoes what the transaction wrote after it, over
    // the rows another session committed after it was taken.
    run(&mut one, "START TRANSACTION");
    run(&mut one, "SAVEPOINT early");
    for id in 100..300 {
        run(
            &mut two,
            &format!("INSERT INTO c (id, n) VALUES ({id}, {})", id * 1000),
        );
    }
    run(&mut one, "UPDATE c SET n = 8 WHERE id = 1");
    run(&mut one, "INSERT INTO c (id, n) VALUES (3, 3)");
    run(&mut one, "ROLLBACK TO SAVEPOINT early");
    run(&mut one, "INSERT INTO c (id, n) VALUES (4, 4)");
    run(&mut one, "COMMIT");
    let Ok(CommandExecutionResult::ResultSet(counted)) =
        two.execute_query("SELECT COUNT(*), SUM(n) FROM c")
    else {
        panic!("the count must return rows");
    };
    assert_eq!(
        counted.rows,
        [[
            Some(b"203".to_vec()),
            Some(
                ((100..300).map(|id| id * 1000).sum::<i64>() + 6 + 4)
                    .to_string()
                    .into_bytes()
            )
        ]]
    );
    assert_eq!(n_of(&mut two, 1), "0");
    let Ok(CommandExecutionResult::ResultSet(checked)) = two.execute_query("CHECK TABLE c") else {
        panic!("CHECK TABLE must return rows");
    };
    assert_eq!(checked.rows[0][3], Some(b"OK".to_vec()));
}

#[test]
fn read_committed_reads_each_statement_afresh_and_writes_after_another_commit() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    run(
        &mut one,
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
    );
    assert_eq!(level_of(&mut one), "READ-COMMITTED");

    run(&mut one, "START TRANSACTION");
    assert_eq!(n_of(&mut one, 2), "0");
    run(&mut two, "UPDATE c SET n = 9 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "9");

    run(&mut two, "UPDATE c SET n = 3 WHERE id = 1");
    run(&mut one, "UPDATE c SET n = n + 1 WHERE id = 1");
    assert_eq!(n_of(&mut one, 1), "4");
    assert_eq!(n_of(&mut two, 1), "3");
    run(&mut one, "COMMIT");
    assert_eq!(n_of(&mut two, 1), "4");
}

#[test]
fn read_committed_releases_its_snapshot_for_a_prepared_statement_too() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    run(
        &mut one,
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
    );
    let prepared = one
        .execute_stmt_prepare("SELECT n FROM c WHERE id = 2")
        .unwrap();
    run(&mut one, "START TRANSACTION");
    assert_eq!(n_of(&mut one, 2), "0");
    run(&mut two, "UPDATE c SET n = 9 WHERE id = 2");
    let PreparedStatementExecutionResult::ResultSet(rows) = one
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("the prepared SELECT must return rows");
    };
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(n_of(&mut one, 2), "9");
    run(&mut one, "COMMIT");
}

#[test]
fn a_level_for_the_next_transaction_alone_is_used_up_by_the_transaction_that_takes_it() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();

    run(&mut one, "SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
    // Measured on MySQL 8.4.11: the variable reads the session's level even
    // inside the transaction that uses the other one.
    assert_eq!(level_of(&mut one), "REPEATABLE-READ");
    // A statement that reads no table does not use the level up.
    run(&mut one, "SELECT 1");
    run(&mut one, "START TRANSACTION");
    assert_eq!(level_of(&mut one), "REPEATABLE-READ");
    assert_eq!(n_of(&mut one, 2), "0");
    run(&mut two, "UPDATE c SET n = 3 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "3");

    // Measured on MySQL 8.4.11: 1568 for the next-transaction form inside a
    // transaction, while the session form is taken and waits for the next.
    assert_eq!(
        one.execute_query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"),
        Err(FrontendErrorKind::TransactionCharacteristicsInProgress)
    );
    run(
        &mut one,
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
    );
    run(&mut one, "COMMIT");
    run(
        &mut one,
        "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );

    // Back at the session's level for the transaction after.
    run(&mut one, "START TRANSACTION");
    assert_eq!(n_of(&mut one, 2), "3");
    run(&mut two, "UPDATE c SET n = 4 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "3");
    run(&mut one, "COMMIT");

    // A statement reading a table is a transaction of its own and uses the
    // level up, so the next START TRANSACTION is back at the session's.
    run(&mut one, "SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
    assert_eq!(n_of(&mut one, 1), "0");
    run(&mut one, "START TRANSACTION");
    assert_eq!(n_of(&mut one, 2), "4");
    run(&mut two, "UPDATE c SET n = 5 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "4");
    run(&mut one, "COMMIT");
}

/// `START TRANSACTION WITH CONSISTENT SNAPSHOT` sent with no database
/// selected, as `mysqldump --single-transaction` sends it, is begun on the
/// database selected next and takes its read view at that selection: a row
/// another session commits afterwards is not seen, though nothing was read
/// yet. MySQL takes the view at the statement itself, which here has no
/// database to take it on; COMPAT.md records the difference.
#[test]
fn a_consistent_snapshot_begun_with_no_database_is_taken_at_the_selection() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, _factory) = catalog_factory(Arc::clone(&authorizer));
    catalog.create("ledger").unwrap();
    let session = |id: u8| {
        let mut session = AuthorizedDatabaseAdapterFactory::new(
            Arc::clone(&catalog),
            binary_context(),
            Arc::clone(&authorizer),
        )
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([id; 32]),
        ))
        .unwrap();
        session.authorize_connection().unwrap();
        session
    };
    let mut writer = session(73);
    writer.execute_init_db("ledger").unwrap();
    run(
        &mut writer,
        "CREATE TABLE c (id INT NOT NULL PRIMARY KEY, n INT)",
    );
    run(&mut writer, "INSERT INTO c (id, n) VALUES (1, 0), (2, 0)");

    let mut dump = session(74);
    run(&mut dump, "START TRANSACTION WITH CONSISTENT SNAPSHOT");
    dump.execute_init_db("ledger").unwrap();
    run(&mut writer, "UPDATE c SET n = 7 WHERE id = 2");
    assert_eq!(n_of(&mut dump, 2), "0");
    run(&mut dump, "COMMIT");
    assert_eq!(n_of(&mut dump, 2), "7");
}

#[test]
fn a_consistent_snapshot_is_taken_at_the_statement() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();

    run(&mut one, "START TRANSACTION WITH CONSISTENT SNAPSHOT");
    run(&mut two, "UPDATE c SET n = 6 WHERE id = 2");
    assert_eq!(n_of(&mut one, 2), "0");
    run(&mut one, "COMMIT");
    assert_eq!(n_of(&mut one, 2), "6");

    // Measured on MySQL 8.4.11: under READ COMMITTED the phrase is ignored
    // with warning 138 and the transaction begins all the same.
    run(
        &mut one,
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
    );
    let Ok(CommandExecutionResult::Ok(begun)) =
        one.execute_query("START TRANSACTION WITH CONSISTENT SNAPSHOT")
    else {
        panic!("the transaction must begin");
    };
    assert_eq!(begun.warnings, 1);
    assert_ne!(begun.status_flags & SERVER_STATUS_IN_TRANS, 0);
    let Ok(CommandExecutionResult::ResultSet(warnings)) = one.execute_query("SHOW WARNINGS") else {
        panic!("SHOW WARNINGS must return a result set");
    };
    assert_eq!(
        warnings.rows,
        vec![vec![
            Some(b"Warning".to_vec()),
            Some(b"138".to_vec()),
            Some(
                b"InnoDB: WITH CONSISTENT SNAPSHOT was ignored because this phrase can only be used with REPEATABLE READ isolation level."
                    .to_vec()
            ),
        ]]
    );
    run(&mut one, "COMMIT");
}

/// MySqlConnector begins `BeginTransaction(IsolationLevel.Serializable)` with
/// `set session transaction isolation level serializable;` and, without
/// waiting for its answer, `start transaction;`, so a refused level left the
/// client reading every later answer one behind.
///
/// Two transactions each read both rows and then write the one the other did
/// not: under `SERIALIZABLE` MySQL makes the second writer a deadlock victim
/// (1213), each read having locked what the other writes. Here the first
/// commits and the second, whose snapshot is then stale, is given up with
/// 1213 — one of them, and never both, is written, as in MySQL.
#[test]
fn serializable_refuses_one_of_two_transactions_that_each_write_what_the_other_read() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    for session in [&mut one, &mut two] {
        run(
            session,
            "set session transaction isolation level serializable;",
        );
        assert_eq!(level_of(session), "SERIALIZABLE");
        run(session, "start transaction;");
        assert_eq!(n_of(session, 1), "0");
        assert_eq!(n_of(session, 2), "0");
    }
    run(&mut one, "UPDATE c SET n = 1 WHERE id = 1");
    run(&mut one, "COMMIT");
    assert_eq!(
        two.execute_query("UPDATE c SET n = 1 WHERE id = 2"),
        Err(FrontendErrorKind::SerializationFailure)
    );
    assert_eq!(two.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    assert_eq!(
        (n_of(&mut two, 1), n_of(&mut two, 2)),
        ("1".to_owned(), "0".to_owned())
    );
}
