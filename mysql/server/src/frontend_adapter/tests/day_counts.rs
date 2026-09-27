//! `ADDDATE` and `SUBDATE`, the other spellings of `DATE_ADD` and
//! `DATE_SUB`, and `TO_DAYS` and `YEARWEEK`, which count days and weeks the
//! way MySQL's calendar counts them.
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
            AccountId::from_bytes([156; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE dd (id INT PRIMARY KEY, d DATE, dt DATETIME, dnn DATE NOT NULL, dt3 DATETIME(3), t TIME)",
        "INSERT INTO dd VALUES (1, '2026-01-15', '2026-01-15 10:20:30', '2024-02-29', '2026-01-15 10:20:30.123', '10:20:30'), (2, NULL, NULL, '0001-01-01', NULL, NULL)",
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
) -> Vec<(String, u8, u32, u16)> {
    let text = result(adapter, sql);
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns, "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
    text.columns
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                column.column_type,
                column.column_length,
                column.flags,
            )
        })
        .collect()
}

/// Measured: a bare count is a count of days, and each reports what its
/// `DATE_ADD` spelling reports — a day shifted by days stays a `DATE`, and
/// every answer is nullable even over a NOT NULL column.
#[test]
fn adddate_and_subdate_shift_the_way_date_add_does() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT ADDDATE(dt, 1), SUBDATE(dt, 1), ADDDATE(d, 1), SUBDATE(d, 31), ADDDATE(d, INTERVAL 1 MONTH), ADDDATE(dt, INTERVAL 1 HOUR), ADDDATE(dnn, 1), ADDDATE(d, -1) FROM dd ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            [
                "2026-01-16 10:20:30",
                "2026-01-14 10:20:30",
                "2026-01-16",
                "2025-12-15",
                "2026-02-15",
                "2026-01-15 11:20:30",
                "2024-03-01",
                "2026-01-14"
            ],
            [
                "NULL",
                "NULL",
                "NULL",
                "NULL",
                "NULL",
                "NULL",
                "0001-01-02",
                "NULL"
            ],
        ]
    );
    let moment = |name: &str| (name.to_owned(), MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG);
    let day = |name: &str| (name.to_owned(), MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG);
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            moment("ADDDATE(dt, 1)"),
            moment("SUBDATE(dt, 1)"),
            day("ADDDATE(d, 1)"),
            day("SUBDATE(d, 31)"),
            day("ADDDATE(d, INTERVAL 1 MONTH)"),
            moment("ADDDATE(dt, INTERVAL 1 HOUR)"),
            day("ADDDATE(dnn, 1)"),
            day("ADDDATE(d, -1)"),
        ]
    );
    assert!(adapter
        .execute_query("SELECT ADDDATE(d, id) FROM dd")
        .is_err());
}

#[test]
fn to_days_and_yearweek_count_by_mysqls_calendar() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT TO_DAYS(d), TO_DAYS(dt), TO_DAYS(dnn), TO_DAYS(dt3), YEARWEEK(d), YEARWEEK(dt), YEARWEEK(d, 1), YEARWEEK(dnn) FROM dd ORDER BY id";
    // Measured: the first day of the year one is day 366, and it falls in
    // the last week of the year zero.
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["739996", "739996", "739310", "739996", "202602", "202602", "202603", "202408"],
            ["NULL", "NULL", "366", "NULL", "NULL", "NULL", "NULL", "53"],
        ]
    );
    let lengths: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, flags)| {
            assert_eq!(
                (column_type, flags),
                (MYSQL_TYPE_LONGLONG, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG)
            );
            length
        })
        .collect();
    assert_eq!(lengths, [8, 8, 8, 8, 7, 7, 7, 7]);
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TO_DAYS('2026-01-15'), YEARWEEK('2026-01-15')"
        ),
        [["739996", "202602"]]
    );
    // A time of day is a span rather than a moment.
    assert!(adapter.execute_query("SELECT TO_DAYS(t) FROM dd").is_err());
}
