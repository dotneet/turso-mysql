//! JSON documents built out of a row — `JSON_OBJECT('id', id, 'name', name)`,
//! the `JSON_ARRAYAGG` around it that nests every row into one answer, and a
//! value put into a document by `JSON_SET` — and how each column's value is
//! written into the document.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([153; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100))",
        "INSERT INTO users (name) VALUES ('ann'), ('bob')",
        "CREATE TABLE posts (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, user_id BIGINT UNSIGNED, title VARCHAR(200) NOT NULL)",
        "INSERT INTO posts (user_id, title) VALUES (1, 'first'), (1, 'second'), (2, 'third')",
        "CREATE TABLE kinds (id BIGINT UNSIGNED PRIMARY KEY, d0 DECIMAL(10,0), d2 DECIMAL(10,2), u INT UNSIGNED, dt DATETIME, d3 DATETIME(3), dd DATE, f DOUBLE, b TINYINT(1), y YEAR, e ENUM('a','b'), js JSON, ts TIMESTAMP NULL, t TIME, fl FLOAT)",
        "INSERT INTO kinds VALUES (18446744073709551615, 5, 1.50, 7, '2026-01-02 03:04:05', '2026-01-02 03:04:05.123', '2026-01-02', 1.5, 1, 2026, 'b', '{\"a\": 1}', '2026-01-02 03:04:05', '03:04:05', 1.5), (1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn result(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

/// The first column of every row, with NULL written as `NULL`.
fn column(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    result(adapter, sql)
        .rows
        .into_iter()
        .map(|row| match &row[0] {
            Some(value) => String::from_utf8(value.clone()).unwrap(),
            None => "NULL".to_owned(),
        })
        .collect()
}

fn assert_reports_json(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) {
    let text = result(adapter, sql);
    let column = text.columns.last().unwrap();
    assert_eq!(
        (
            column.column_type,
            column.column_length,
            column.decimals,
            column.flags
        ),
        (
            MYSQL_TYPE_JSON,
            u32::MAX - 3,
            NOT_FIXED_DECIMALS,
            MYSQL_BINARY_FLAG
        ),
        "{sql}"
    );
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns, "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
}

#[test]
fn a_document_built_from_an_unsigned_key_writes_the_key_as_a_number() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT JSON_OBJECT('id', id, 'name', name) FROM users ORDER BY id";
    assert_eq!(
        column(&mut adapter, sql),
        [r#"{"id": 1, "name": "ann"}"#, r#"{"id": 2, "name": "bob"}"#]
    );
    assert_reports_json(&mut adapter, sql);
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_OBJECT('id', id, 'd0', d0, 'u', u) FROM kinds ORDER BY id"
        ),
        [
            r#"{"u": null, "d0": null, "id": 1}"#,
            r#"{"u": 7, "d0": 5, "id": 18446744073709551615}"#
        ]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_ARRAY(id, d0) FROM kinds ORDER BY id"
        ),
        ["[1, null]", "[18446744073709551615, 5]"]
    );
    // A number written with a point keeps the places it was written with,
    // which a double writes back while none past the first is a trailing zero.
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_ARRAY(1.5, 0.1, 1.0, .5, 123456789.123, -0.5, 100.0)"
        ),
        ["[1.5, 0.1, 1.0, 0.5, 123456789.123, -0.5, 100.0]"]
    );
}

/// Measured: a moment is written with six places of a second whatever its
/// column keeps, and a `JSON` column as the document it holds.
#[test]
fn a_moment_or_a_document_is_written_into_a_document_the_way_mysql_writes_it() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_OBJECT('dt', dt, 'd3', d3, 'dd', dd, 'f', f, 'b', b, 'y', y, 'e', e, 'js', js) FROM kinds ORDER BY id"
        ),
        [
            r#"{"b": null, "e": null, "f": null, "y": null, "d3": null, "dd": null, "dt": null, "js": null}"#,
            r#"{"b": 1, "e": "b", "f": 1.5, "y": 2026, "d3": "2026-01-02 03:04:05.123000", "dd": "2026-01-02", "dt": "2026-01-02 03:04:05.000000", "js": {"a": 1}}"#
        ]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_SET(js, '$.b', d3), JSON_SET(js, '$.b', js), JSON_INSERT(js, '$.c', id) FROM kinds WHERE id = 18446744073709551615"
        ),
        [r#"{"a": 1, "b": "2026-01-02 03:04:05.123000"}"#]
    );
    let row = &result(
        &mut adapter,
        "SELECT JSON_SET(js, '$.b', d3), JSON_SET(js, '$.b', js), JSON_INSERT(js, '$.c', id) FROM kinds WHERE id = 18446744073709551615",
    )
    .rows[0];
    let texts: Vec<_> = row
        .iter()
        .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            r#"{"a": 1, "b": "2026-01-02 03:04:05.123000"}"#,
            r#"{"a": 1, "b": {"a": 1}}"#,
            r#"{"a": 1, "c": 18446744073709551615}"#
        ]
    );
}

/// Measured: a `DECIMAL` with places and a number written with a point keep
/// every place in the document — `1.50`, `10.00` — where the engine writes a
/// double's shortest digits, and a `TIME` and a `TIMESTAMP` are
/// written by rules of their own; a `FLOAT` has not been measured.
#[test]
fn a_value_written_into_a_document_by_a_rule_not_followed_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT JSON_OBJECT('d2', d2) FROM kinds",
        "SELECT JSON_ARRAY(d2) FROM kinds",
        "SELECT JSON_OBJECT('n', 1.50)",
        "SELECT JSON_ARRAY(10.00)",
        "SELECT JSON_ARRAY(1234567890.1234567)",
        "SELECT JSON_OBJECT('t', t) FROM kinds",
        "SELECT JSON_OBJECT('ts', ts) FROM kinds",
        "SELECT JSON_OBJECT('fl', fl) FROM kinds",
        "SELECT JSON_SET(js, '$.b', d2) FROM kinds",
        "SELECT JSON_SET(js, '$.b', t) FROM kinds",
        "SELECT JSON_SET(js, '$.b', 1.50) FROM kinds",
        "SELECT JSON_INSERT(js, '$.b', ts) FROM kinds",
        "SELECT JSON_ARRAYAGG(JSON_OBJECT('t', t)) FROM kinds",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

#[test]
fn json_arrayagg_nests_the_document_built_from_each_row() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT JSON_ARRAYAGG(JSON_OBJECT('id', id, 'name', name)) FROM users";
    assert_eq!(
        column(&mut adapter, sql),
        [r#"[{"id": 1, "name": "ann"}, {"id": 2, "name": "bob"}]"#]
    );
    assert_reports_json(&mut adapter, sql);
    // Grouped, each group's rows go into its own array.
    let sql = "SELECT user_id, JSON_ARRAYAGG(JSON_OBJECT('id', id, 'title', title)) FROM posts GROUP BY user_id ORDER BY user_id";
    let rows: Vec<_> = result(&mut adapter, sql)
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
        .collect();
    assert_eq!(
        rows,
        [
            r#"[{"id": 1, "title": "first"}, {"id": 2, "title": "second"}]"#,
            r#"[{"id": 3, "title": "third"}]"#
        ]
    );
    assert_reports_json(&mut adapter, sql);
    // Over no rows at all, NULL rather than an empty array.
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_ARRAYAGG(JSON_ARRAY(id)) FROM users WHERE id > 100"
        ),
        ["NULL"]
    );
    // It aggregates the statement, so a bare column beside it is 1140.
    assert!(adapter
        .execute_query("SELECT id, JSON_ARRAYAGG(JSON_OBJECT('id', id)) FROM users")
        .is_err());
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert_eq!(
        binary.rows[1][1],
        BinaryResultValue::Blob(br#"[{"id": 3, "title": "third"}]"#.to_vec())
    );
}
