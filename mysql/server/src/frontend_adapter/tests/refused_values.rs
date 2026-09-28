//! A value a column refuses, inside a transaction: the statement fails and
//! the transaction goes on.
//!
//! Every expectation here was measured on MySQL 8.4.11 in strict mode.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([181; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    run(
        &mut adapter,
        "CREATE TABLE p (n INT NOT NULL, s VARCHAR(3), t TINYINT, d DATETIME, \
         e ENUM('a','b'), j JSON, KEY (s))",
    );
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

#[test]
fn a_refused_value_fails_its_statement_and_keeps_the_transaction_and_its_savepoint() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "START TRANSACTION");
    run(
        &mut adapter,
        "INSERT INTO p (n, s) VALUES (1, 'a'), (2, 'b')",
    );
    run(&mut adapter, "SAVEPOINT before_refused");

    // A statement that fails on a later row keeps none of its own rows,
    // and an UPDATE that fails on its second row leaves the first as it was.
    for (sql, refused) in [
        (
            "INSERT INTO p (n, s) VALUES (3, 'toolong')",
            FrontendErrorKind::DataTooLong,
        ),
        (
            "INSERT INTO p (n, t) VALUES (3, 1000)",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO p (n) VALUES ('abc')",
            FrontendErrorKind::IncorrectValue,
        ),
        (
            "INSERT INTO p (n, d) VALUES (3, '2024-13-45 99:99:99')",
            FrontendErrorKind::IncorrectTemporalValue,
        ),
        (
            "INSERT INTO p (n, e) VALUES (3, 'zzz')",
            FrontendErrorKind::NotAMember,
        ),
        (
            "INSERT INTO p (n, j) VALUES (3, '{bad')",
            FrontendErrorKind::InvalidJsonText,
        ),
        (
            "INSERT INTO p (n, s) VALUES (3, 'ok'), (4, 'toolong'), (5, 'ok')",
            FrontendErrorKind::DataTooLong,
        ),
        ("UPDATE p SET t = n * 100", FrontendErrorKind::OutOfRange),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(refused), "{sql}");
        assert_ne!(
            adapter.status_flags() & SERVER_STATUS_IN_TRANS,
            0,
            "{sql} ended the transaction"
        );
    }
    assert_eq!(
        rows(&mut adapter, "SELECT n, s, t FROM p ORDER BY n"),
        vec![
            vec![Some("1".to_owned()), Some("a".to_owned()), None],
            vec![Some("2".to_owned()), Some("b".to_owned()), None],
        ]
    );

    run(&mut adapter, "INSERT INTO p (n) VALUES (6)");
    run(&mut adapter, "ROLLBACK TO SAVEPOINT before_refused");
    run(&mut adapter, "COMMIT");
    assert_eq!(
        rows(&mut adapter, "SELECT n FROM p ORDER BY n"),
        vec![vec![Some("1".to_owned())], vec![Some("2".to_owned())]]
    );
}
