//! The settings a client sends when it opens a connection, and what they
//! change afterwards.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

const RAILS_OPENS_WITH: &str = "SET NAMES utf8mb4,  @@SESSION.sql_mode = CONCAT(CONCAT(@@sql_mode, ',STRICT_ALL_TABLES'), ',NO_AUTO_VALUE_ON_ZERO'),  @@SESSION.wait_timeout = 2147483";

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([81; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result,
        other => panic!("{sql}: {other:?}"),
    }
}

fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<Option<String>>> {
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

/// Under `NO_AUTO_VALUE_ON_ZERO`, which Rails always turns on, a written 0 is
/// stored as 0 and a second one is a duplicate, while NULL still takes the
/// next number. Measured on MySQL 8.4.11: `(0, 1)`, `(2)`, `(NULL, 3)` and
/// `(0, 4)` leave rows 0, 1 and 2 and refuse the last with 1062, and once the
/// mode is gone a written 0 takes the next number again.
#[test]
fn a_written_zero_is_stored_under_no_auto_value_on_zero() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, RAILS_OPENS_WITH);
    run(
        &mut adapter,
        "CREATE TABLE z (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)",
    );
    run(&mut adapter, "INSERT INTO z (id, v) VALUES (0, 1)");
    let second = run(&mut adapter, "INSERT INTO z (v) VALUES (2)");
    assert_eq!(second.last_insert_id, 1);
    run(&mut adapter, "INSERT INTO z (id, v) VALUES (NULL, 3)");
    assert_eq!(
        adapter.execute_query("INSERT INTO z (id, v) VALUES (0, 4)"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM z ORDER BY id"),
        [
            [Some("0".to_owned()), Some("1".to_owned())],
            [Some("1".to_owned()), Some("2".to_owned())],
            [Some("2".to_owned()), Some("3".to_owned())],
        ]
    );

    run(&mut adapter, "SET sql_mode = DEFAULT");
    let generated = run(&mut adapter, "INSERT INTO z (id, v) VALUES (0, 5)");
    assert_eq!(generated.last_insert_id, 3);
}

/// A prepared `INSERT` binding 0 reads it the same way the written one does.
#[test]
fn a_bound_zero_is_stored_under_no_auto_value_on_zero() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, RAILS_OPENS_WITH);
    run(
        &mut adapter,
        "CREATE TABLE z (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)",
    );
    let prepared = adapter
        .execute_stmt_prepare("INSERT INTO z (id, v) VALUES (?, 1)")
        .unwrap();
    // The null bitmap, the new-parameters flag, LONGLONG and its sign byte,
    // and the eight bytes of 0.
    let payload = [0, 1, MYSQL_TYPE_LONGLONG, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    adapter
        .execute_stmt_execute(prepared.statement_id, &payload)
        .unwrap();
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM z"),
        [[Some("0".to_owned())]]
    );
}

/// The idle time a session asks for is the one its connection keeps, and
/// `DEFAULT` gives the server's own back.
#[test]
fn the_idle_time_a_session_asks_for_reaches_its_connection() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(adapter.session_wait_timeout(), None);
    run(&mut adapter, RAILS_OPENS_WITH);
    assert_eq!(
        adapter.session_wait_timeout(),
        Some(Duration::from_secs(2_147_483))
    );
    run(&mut adapter, "SET @@SESSION.wait_timeout = DEFAULT");
    assert_eq!(adapter.session_wait_timeout(), None);
}
