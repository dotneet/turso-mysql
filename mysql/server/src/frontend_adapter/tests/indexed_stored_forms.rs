//! A value MySQL stores in a form of its own — a `DATETIME(6)` with its six
//! fraction digits, a `CHAR` without its trailing spaces, a `SET` in member
//! order — goes into a key over its column in that same form, so the key
//! finds the row and a `DELETE` finds the key's entry.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([122; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let answer = adapter.execute_query(sql);
    let Ok(CommandExecutionResult::ResultSet(result)) = answer else {
        panic!("{sql} must return a result set: {answer:?}");
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

fn ids(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    rows(adapter, sql)
        .into_iter()
        .map(|mut row| row.remove(0).unwrap())
        .collect()
}

/// `CHECK TABLE`'s message for the table, `OK` when every row is in every key.
fn checked(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("CHECK TABLE {table}"))
        .remove(0)
        .pop()
        .unwrap()
        .unwrap()
}

fn prepared_insert(adapter: &mut Adapter, sql: &str, values: &[&str]) {
    let result = prepared(adapter, sql, values);
    assert!(
        matches!(result, PreparedStatementExecutionResult::Ok(_)),
        "{sql}"
    );
}

/// The rows a prepared `SELECT` answers when each value is bound as a word.
fn prepared_rows(adapter: &mut Adapter, sql: &str, values: &[&str]) -> Vec<Vec<BinaryResultValue>> {
    let PreparedStatementExecutionResult::ResultSet(result) = prepared(adapter, sql, values) else {
        panic!("{sql} must return a result set");
    };
    result.rows
}

fn prepared(adapter: &mut Adapter, sql: &str, values: &[&str]) -> PreparedStatementExecutionResult {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for _ in values {
        payload.extend_from_slice(&[MYSQL_TYPE_VAR_STRING, 0]);
    }
    for value in values {
        payload.push(u8::try_from(value.len()).unwrap());
        payload.extend_from_slice(value.as_bytes());
    }
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = adapter
        .execute_stmt_execute(statement.statement_id, &payload)
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"));
    adapter.execute_stmt_close(statement.statement_id);
    result
}

/// Hibernate declares a `LocalDateTime` as `DATETIME(6)` and writes it
/// without a fraction when it has none; Spring's
/// `delete p1_0 from posts p1_0` then failed on a key over the column.
#[test]
fn a_moment_written_without_its_fraction_is_found_through_its_keys() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE b1 (id INT NOT NULL PRIMARY KEY, u INT, d DATETIME(6) NULL, KEY k (d), KEY ud (u, d))",
    );
    run(
        &mut adapter,
        "INSERT INTO b1 (id, u, d) VALUES (1, 7, '2024-01-02 03:04:05')",
    );
    prepared_insert(
        &mut adapter,
        "INSERT INTO b1 (id, u, d) VALUES (2, 7, ?)",
        &["2024-01-02 03:04:06.5"],
    );
    assert_eq!(checked(&mut adapter, "b1"), "OK");
    // A written moment is compared in the column's own form, and a bound
    // one is put into it; both reach the row through its key.
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM b1 WHERE d = '2024-01-02 03:04:05.000000'"
        ),
        ["1"]
    );
    assert_eq!(
        prepared_rows(
            &mut adapter,
            "SELECT id FROM b1 WHERE d = ?",
            &["2024-01-02 03:04:05"]
        ),
        [[BinaryResultValue::Integer(1)]]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM b1 WHERE u = 7 AND d = '2024-01-02 03:04:06.500000'"
        ),
        ["2"]
    );

    run(
        &mut adapter,
        "UPDATE b1 SET d = '2024-01-02 03:04:07' WHERE id = 1",
    );
    run(
        &mut adapter,
        "INSERT INTO b1 (id, u, d) VALUES (2, 7, NULL) ON DUPLICATE KEY UPDATE d = '2024-01-02 03:04:08'",
    );
    assert_eq!(checked(&mut adapter, "b1"), "OK");
    assert_eq!(
        rows(&mut adapter, "SELECT id, d FROM b1 ORDER BY d"),
        [
            [
                Some("1".to_owned()),
                Some("2024-01-02 03:04:07.000000".to_owned())
            ],
            [
                Some("2".to_owned()),
                Some("2024-01-02 03:04:08.000000".to_owned())
            ],
        ]
    );

    run(&mut adapter, "DELETE FROM b1");
    assert!(rows(&mut adapter, "SELECT id FROM b1").is_empty());
}

/// The same moment written with and without its fraction is one value to a
/// unique key, as it is on MySQL (1062).
#[test]
fn a_unique_key_meets_a_moment_however_its_fraction_was_written() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE u1 (id INT NOT NULL PRIMARY KEY, d DATETIME(6), UNIQUE KEY d (d))",
    );
    run(
        &mut adapter,
        "INSERT INTO u1 (id, d) VALUES (1, '2024-01-02 03:04:05')",
    );
    assert!(adapter
        .execute_query("INSERT INTO u1 (id, d) VALUES (2, '2024-01-02 03:04:05.000000')")
        .is_err());
    assert_eq!(ids(&mut adapter, "SELECT id FROM u1"), ["1"]);
}

/// Every kind of column whose written value is stored rewritten, written by
/// an `INSERT`, an `UPDATE` and an upsert, in a table whose key is its rowid,
/// one that counts its ids and one whose key is an index of its own.
#[test]
fn every_value_stored_in_a_form_of_its_own_goes_into_its_key_that_way() {
    for key in [
        "INT NOT NULL PRIMARY KEY",
        "INT NOT NULL AUTO_INCREMENT PRIMARY KEY",
        "BIGINT UNSIGNED NOT NULL PRIMARY KEY",
    ] {
        for (column, written) in [
            ("TIMESTAMP(6)", "'2024-01-02 03:04:05'"),
            ("TIME(3)", "'03:04:05'"),
            ("DATETIME", "'2024-01-02 03:04:05.4'"),
            ("DATE", "'2024-1-2'"),
            ("YEAR", "24"),
            ("CHAR(4)", "'ab  '"),
            ("SET('a','b')", "'b,a'"),
            ("ENUM('a','b')", "'0'"),
            ("FLOAT", "1.1"),
        ] {
            every_value_written_into(key, column, written);
        }
    }
}

fn every_value_written_into(key: &str, column: &str, written: &str) {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        &format!("CREATE TABLE b1 (id {key}, d {column} NULL, KEY k (d))"),
    );
    run(
        &mut adapter,
        &format!("INSERT INTO b1 (id, d) VALUES (1, {written}), (2, NULL), (3, NULL)"),
    );
    run(
        &mut adapter,
        &format!("UPDATE b1 SET d = {written} WHERE id = 2"),
    );
    run(
        &mut adapter,
        &format!("INSERT INTO b1 (id, d) VALUES (3, NULL) ON DUPLICATE KEY UPDATE d = {written}"),
    );
    assert_eq!(checked(&mut adapter, "b1"), "OK", "{key} {column}");
    run(&mut adapter, "DELETE FROM b1");
    assert!(
        rows(&mut adapter, "SELECT id FROM b1").is_empty(),
        "{key} {column}"
    );
}
