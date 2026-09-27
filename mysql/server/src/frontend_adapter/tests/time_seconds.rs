//! `TIME_TO_SEC` and `SEC_TO_TIME`, which count the seconds in a time and
//! write a count back out as one.
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
            AccountId::from_bytes([160; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE tt (id INT PRIMARY KEY, t TIME, tn TIME NOT NULL, dt DATETIME, s INT, sn INT NOT NULL, t3 TIME(3), d DATE, ts TIMESTAMP NULL, v VARCHAR(10))",
        "INSERT INTO tt VALUES (1, '01:01:01', '-838:59:59', '2026-01-15 10:20:30', 3661, -1, '01:02:03.456', '2026-01-15', NULL, '01:00:00'), (2, NULL, '00:00:00', NULL, NULL, 0, NULL, NULL, NULL, NULL), (3, '100:00:00', '12:34:56', '2026-01-15 00:00:00', 3020400, 86400, '-00:00:01.5', '2026-01-15', NULL, NULL)",
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

/// Measured: a `TIME` counts whole, a fraction cut toward zero, a `DATETIME`
/// counts its time of day and a `DATE` counts none.
#[test]
fn time_to_sec_counts_the_seconds_in_a_time() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT TIME_TO_SEC(t), TIME_TO_SEC(tn), TIME_TO_SEC(dt), TIME_TO_SEC(t3), TIME_TO_SEC(d) FROM tt ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["3661", "-3020399", "37230", "3723", "0"],
            ["NULL", "0", "NULL", "NULL", "NULL"],
            ["360000", "45296", "0", "-1", "0"],
        ]
    );
    assert_eq!(
        shapes(&mut adapter, sql),
        [(MYSQL_TYPE_LONGLONG, 10, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG); 5]
    );
}

/// Measured: a count past the widest time is held to `838:59:59`.
#[test]
fn sec_to_time_writes_a_count_out_as_a_time() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT SEC_TO_TIME(s), SEC_TO_TIME(sn) FROM tt ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["01:01:01", "-00:00:01"],
            ["NULL", "00:00:00"],
            ["838:59:59", "24:00:00"],
        ]
    );
    assert_eq!(
        shapes(&mut adapter, sql),
        [(MYSQL_TYPE_TIME, 10, MYSQL_BINARY_FLAG); 2]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TIME_TO_SEC('01:01:01'), SEC_TO_TIME(3661), SEC_TO_TIME(-1)"
        ),
        [["3661", "01:01:01", "-00:00:01"]]
    );
    let prepared = adapter
        .execute_stmt_prepare("SELECT SEC_TO_TIME(s) FROM tt WHERE id = 3")
        .unwrap();
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert_eq!(
        binary.rows,
        [[BinaryResultValue::Time {
            negative: false,
            days: 34,
            hour: 22,
            minute: 59,
            second: 59
        }]]
    );
}

/// Measured: a written value MySQL holds or reads as nothing comes with a
/// warning, which is not raised here; a `TIMESTAMP` is read in the session's
/// zone; and a word or a number is read by rules of MySQL's own.
#[test]
fn a_value_time_to_sec_or_sec_to_time_would_warn_about_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT SEC_TO_TIME(3020400)",
        "SELECT TIME_TO_SEC('nope')",
        "SELECT TIME_TO_SEC(ts) FROM tt",
        "SELECT TIME_TO_SEC(v) FROM tt",
        "SELECT TIME_TO_SEC(s) FROM tt",
        "SELECT SEC_TO_TIME(t) FROM tt",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
