//! Readings of the clock to a fraction of a second — `NOW(6)`,
//! `CURRENT_TIMESTAMP(3)`, `CURTIME(3)` — and the `LOCALTIME` and
//! `LOCALTIMESTAMP` spellings of `NOW()`.
//!
//! Every shape here was measured on MySQL 8.4.11.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([152; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE posts (id INT NOT NULL PRIMARY KEY, created_at DATETIME NULL)",
        "INSERT INTO posts VALUES (1, '2020-01-01 00:00:00'), (2, '2999-01-01 00:00:00')",
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

/// Measured: each reports the places it was asked for and is that much wider,
/// one more for the point, and none of them can be null.
#[test]
fn a_clock_reading_to_a_fraction_reports_the_places_it_was_asked_for() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT NOW(6), NOW(3), NOW(0), CURRENT_TIMESTAMP(3), SYSDATE(1), UTC_TIMESTAMP(2), LOCALTIMESTAMP(6), LOCALTIME, LOCALTIMESTAMP, LOCALTIME(), CURTIME(6), CURRENT_TIME(3), UTC_TIME(3), CURRENT_TIME(0)";
    let text = result(&mut adapter, sql);
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns);
    let shapes: Vec<_> = text
        .columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
            )
        })
        .collect();
    let flags = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG;
    assert_eq!(
        shapes,
        [
            ("NOW(6)", MYSQL_TYPE_DATETIME, 26, 6, flags),
            ("NOW(3)", MYSQL_TYPE_DATETIME, 23, 3, flags),
            ("NOW(0)", MYSQL_TYPE_DATETIME, 19, 0, flags),
            ("CURRENT_TIMESTAMP(3)", MYSQL_TYPE_DATETIME, 23, 3, flags),
            ("SYSDATE(1)", MYSQL_TYPE_DATETIME, 21, 1, flags),
            ("UTC_TIMESTAMP(2)", MYSQL_TYPE_DATETIME, 22, 2, flags),
            ("LOCALTIMESTAMP(6)", MYSQL_TYPE_DATETIME, 26, 6, flags),
            ("LOCALTIME", MYSQL_TYPE_DATETIME, 19, 0, flags),
            ("LOCALTIMESTAMP", MYSQL_TYPE_DATETIME, 19, 0, flags),
            ("LOCALTIME()", MYSQL_TYPE_DATETIME, 19, 0, flags),
            ("CURTIME(6)", MYSQL_TYPE_TIME, 15, 6, flags),
            ("CURRENT_TIME(3)", MYSQL_TYPE_TIME, 12, 3, flags),
            ("UTC_TIME(3)", MYSQL_TYPE_TIME, 12, 3, flags),
            ("CURRENT_TIME(0)", MYSQL_TYPE_TIME, 8, 0, flags),
        ]
    );
    // Each value is written with exactly the places the column reports, a
    // moment as `YYYY-MM-DD HH:MM:SS.ffffff` and a time of day as
    // `HH:MM:SS.fff`.
    let [row] = text.rows.as_slice() else {
        panic!("one row");
    };
    for (value, column) in row.iter().zip(&text.columns) {
        let value = String::from_utf8(value.clone().unwrap()).unwrap();
        assert_eq!(
            value.len(),
            usize::try_from(column.column_length).unwrap(),
            "{}: {value}",
            column.name
        );
        let whole = if column.column_type == MYSQL_TYPE_DATETIME {
            19
        } else {
            8
        };
        if column.decimals > 0 {
            assert_eq!(&value[whole..=whole], ".", "{}: {value}", column.name);
            assert!(value[whole + 1..].bytes().all(|byte| byte.is_ascii_digit()));
        }
    }

    // The binary protocol sends the fraction as microseconds.
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert!(matches!(
        binary.rows[0][0],
        BinaryResultValue::DateTimeMicros { .. } | BinaryResultValue::DateTime { .. }
    ));
    assert!(matches!(
        binary.rows[0][10],
        BinaryResultValue::Time { .. } | BinaryResultValue::TimeMicros { .. }
    ));
}

#[test]
fn localtimestamp_is_the_moment_now_is() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT id FROM posts WHERE created_at < LOCALTIMESTAMP",
        "SELECT id FROM posts WHERE created_at < LOCALTIME",
        "SELECT id FROM posts WHERE created_at < NOW()",
    ] {
        assert_eq!(
            result(&mut adapter, sql).rows,
            [[Some(b"1".to_vec())]],
            "{sql}"
        );
    }
}

/// Measured: MySQL refuses a count past 6 with 1426.
#[test]
fn a_clock_reading_past_six_places_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT NOW(7)",
        "SELECT CURTIME(7)",
        "SELECT NOW(-1)",
        "SELECT NOW(id) FROM posts",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
