//! `LAG` and `LEAD` reaching more than one row away, and answering a default
//! where there is no such row.
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
            AccountId::from_bytes([155; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE w (id INT PRIMARY KEY, n INT, nn INT NOT NULL, b BIGINT, s VARCHAR(10), sn VARCHAR(10) NOT NULL, d DATE, dc DECIMAL(10,2))",
        "INSERT INTO w VALUES (1, 10, 1, 100, 'a', 'x', '2026-01-01', 1.50), (2, NULL, 2, 200, NULL, 'y', NULL, NULL), (3, 30, 3, 300, 'c', 'z', '2026-01-03', 3.25)",
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

fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<String>> {
    result(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    Some(value) => String::from_utf8(value).unwrap(),
                    None => "NULL".to_owned(),
                })
                .collect()
        })
        .collect()
}

fn shapes(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<(u8, u32, u16)> {
    let text = result(adapter, sql);
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns, "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
    text.columns
        .iter()
        .map(|column| (column.column_type, column.column_length, column.flags))
        .collect()
}

#[test]
fn an_offset_reaches_that_many_rows_and_a_default_fills_where_there_is_none() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT LAG(n, 1) OVER (ORDER BY id), LAG(n, 2) OVER (ORDER BY id), LAG(n, 1, 0) OVER (ORDER BY id), LEAD(n, 1, -1) OVER (ORDER BY id), LAG(n, 0) OVER (ORDER BY id), LAG(n, 1, NULL) OVER (ORDER BY id) FROM w ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["NULL", "NULL", "0", "NULL", "10", "NULL"],
            ["10", "NULL", "10", "30", "NULL", "10"],
            ["NULL", "10", "NULL", "-1", "30", "NULL"],
        ]
    );
    // An offset leaves the shape the plain call reports.
    assert_eq!(
        shapes(&mut adapter, sql),
        [(MYSQL_TYPE_LONGLONG, 11, MYSQL_NUM_FLAG); 6]
    );
}

/// Measured: a default widens the answer to its own width, and over a NOT
/// NULL column the answer cannot be null either.
#[test]
fn a_default_widens_the_answer_and_keeps_a_not_null_column_not_null() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT LAG(nn, 1, 0) OVER (ORDER BY id), LAG(nn) OVER (ORDER BY id), LEAD(b, 1, 0) OVER (ORDER BY id), LAG(n, 1, 99999999999) OVER (ORDER BY id), LAG(s, 1, 'none') OVER (ORDER BY id), LAG(sn, 1, 'none') OVER (ORDER BY id), LAG(s, 1, 'a much longer default') OVER (ORDER BY id), LEAD(sn, 2) OVER (ORDER BY id) FROM w ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            [
                "0",
                "NULL",
                "200",
                "99999999999",
                "none",
                "none",
                "a much longer default",
                "z"
            ],
            ["1", "1", "300", "10", "a", "x", "a", "NULL"],
            ["2", "2", "0", "NULL", "NULL", "y", "NULL", "NULL"],
        ]
    );
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            (
                MYSQL_TYPE_LONGLONG,
                11,
                MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG
            ),
            (MYSQL_TYPE_LONGLONG, 11, MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONGLONG, 20, MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONGLONG, 12, MYSQL_NUM_FLAG),
            (MYSQL_TYPE_VAR_STRING, 40, 0),
            (MYSQL_TYPE_VAR_STRING, 40, MYSQL_NOT_NULL_FLAG),
            (MYSQL_TYPE_VAR_STRING, 84, 0),
            (MYSQL_TYPE_VAR_STRING, 40, 0),
        ]
    );
}

/// Measured: a default of another kind than the column's changes the type —
/// `LAG(n, 1, 1.5)` answers a NEWDECIMAL and `LAG(d, 1, '2000-01-01')` a
/// VAR_STRING — and a negative offset is a syntax error, 1064.
#[test]
fn a_default_of_another_kind_or_an_offset_read_from_the_row_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT LAG(n, 1, 1.5) OVER (ORDER BY id) FROM w",
        "SELECT LAG(d, 1, '2000-01-01') OVER (ORDER BY id) FROM w",
        "SELECT LAG(dc, 1, 0) OVER (ORDER BY id) FROM w",
        "SELECT LAG(n, 1, 'x') OVER (ORDER BY id) FROM w",
        "SELECT LAG(s, 1, 0) OVER (ORDER BY id) FROM w",
        "SELECT LAG(n, -1) OVER (ORDER BY id) FROM w",
        "SELECT LAG(n, id) OVER (ORDER BY id) FROM w",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
