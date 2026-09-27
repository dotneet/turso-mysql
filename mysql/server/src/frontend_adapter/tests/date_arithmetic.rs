//! The date arithmetic report queries and framework scopes write: how long ago
//! a row was made, the rows of the last day, which week a row falls in.
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
            AccountId::from_bytes([131; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn selected(
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
) -> Vec<Vec<Option<String>>> {
    selected(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

/// One column of rows, with NULL written as `NULL`.
fn column(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    rows(adapter, sql)
        .into_iter()
        .map(|mut row| {
            assert_eq!(row.len(), 1, "{sql}");
            row.remove(0).unwrap_or_else(|| "NULL".to_owned())
        })
        .collect()
}

/// The type, length, decimals and flags of each result column.
fn shapes(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<(u8, u32, u8, u16)> {
    selected(adapter, sql)
        .columns
        .iter()
        .map(|column| {
            (
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
            )
        })
        .collect()
}

/// The null bitmap, the new-parameters flag and one LONGLONG parameter.
fn one_number(number: i64) -> Vec<u8> {
    let mut payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
    payload.extend_from_slice(&number.to_le_bytes());
    payload
}

fn prepared_rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
    payload: &[u8],
) -> (Vec<ColumnDefinitionConfig>, Vec<Vec<BinaryResultValue>>) {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(statement.statement_id, payload)
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"))
    else {
        panic!("{sql} must return a result set");
    };
    adapter.execute_stmt_close(statement.statement_id);
    assert_eq!(result.columns, statement.columns, "{sql}");
    (result.columns, result.rows)
}

fn pairs_of_moments(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>) {
    run(
        adapter,
        "CREATE TABLE pairs (id INT NOT NULL PRIMARY KEY, a DATETIME(6), b DATETIME(6), label VARCHAR(8))",
    );
    run(
        adapter,
        concat!(
            "INSERT INTO pairs (id, a, b) VALUES ",
            "(1, '2024-01-31 00:00:00', '2024-02-29 00:00:00'), ",
            "(2, '2024-01-31 00:00:00', '2024-03-01 00:00:00'), ",
            "(3, '2024-01-15 10:00:00', '2024-02-15 09:59:59'), ",
            "(4, '2024-01-15 10:00:00', '2024-02-15 10:00:00'), ",
            "(5, '2024-01-15 10:00:00.5', '2024-02-15 10:00:00.4'), ",
            "(6, '2024-03-31 00:00:00', '2024-02-29 00:00:00'), ",
            "(7, '2024-02-29 00:00:00', '2025-02-28 00:00:00'), ",
            "(8, '2024-02-29 00:00:00', '2028-02-29 00:00:00'), ",
            "(9, '2025-12-31 23:59:59', '2024-01-01 00:00:00'), ",
            "(10, '2024-02-15 10:00:00', '2024-01-15 10:00:00.000001'), ",
            "(11, '2024-01-01 00:00:00', '2024-04-01 00:00:00'), ",
            "(12, '2024-01-01 00:00:00', '2024-03-31 23:59:59'), ",
            "(13, '2024-02-29 12:00:00', '2024-01-31 12:00:00'), ",
            "(14, '2024-01-31 00:00:00.000001', '2024-01-31 00:00:00'), ",
            "(15, '2024-01-01 00:00:00.9', '2024-01-01 00:00:01.1'), ",
            "(16, '2024-01-01 00:00:01.1', '2024-01-01 00:00:00.9'), ",
            "(17, NULL, '2024-01-01')"
        ),
    );
}

/// `TIMESTAMPDIFF` counts a month by the calendar — it is whole once the later
/// moment reaches the same day of the month and the same time of day, so
/// January 31st to February 29th is no month at all — and counts every unit to
/// the microsecond before dropping what is left over, towards zero either way.
/// `DATEDIFF` counts the days alone.
#[test]
fn timestampdiff_counts_whole_units_by_the_calendar_and_to_the_microsecond() {
    let (_directory, mut adapter) = adapter();
    pairs_of_moments(&mut adapter);
    for (unit, answers) in [
        (
            "MONTH",
            "0 1 0 1 0 -1 11 48 -23 0 3 2 0 0 0 0 NULL",
        ),
        ("QUARTER", "0 0 0 0 0 0 3 16 -7 0 1 0 0 0 0 0 NULL"),
        ("YEAR", "0 0 0 0 0 0 0 4 -1 0 0 0 0 0 0 0 NULL"),
        (
            "SECOND",
            "2505600 2592000 2678399 2678400 2678399 -2678400 31536000 126230400 -63158399 -2678399 7862400 7862399 -2505600 0 0 0 NULL",
        ),
        (
            "MICROSECOND",
            "2505600000000 2592000000000 2678399000000 2678400000000 2678399900000 -2678400000000 31536000000000 126230400000000 -63158399000000 -2678399999999 7862400000000 7862399000000 -2505600000000 -1 200000 -200000 NULL",
        ),
        (
            "DAY",
            "29 30 30 31 30 -31 365 1461 -730 -30 91 90 -29 0 0 0 NULL",
        ),
        ("WEEK", "4 4 4 4 4 -4 52 208 -104 -4 13 12 -4 0 0 0 NULL"),
    ] {
        let sql = format!("SELECT TIMESTAMPDIFF({unit}, a, b) FROM pairs ORDER BY id");
        assert_eq!(
            column(&mut adapter, &sql).join(" "),
            answers,
            "{sql}"
        );
        assert_eq!(
            shapes(&mut adapter, &sql),
            [(MYSQL_TYPE_LONGLONG, 21, 0, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG)],
            "{sql}"
        );
    }
    let sql = "SELECT DATEDIFF(b, a) FROM pairs ORDER BY id";
    assert_eq!(
        column(&mut adapter, sql).join(" "),
        "29 30 31 31 31 -31 365 1461 -730 -31 91 90 -29 0 0 0 NULL"
    );
    assert_eq!(
        shapes(&mut adapter, sql),
        [(
            MYSQL_TYPE_LONGLONG,
            9,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        )]
    );

    // Each is read as a whole number where a row is chosen or ordered.
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM pairs WHERE TIMESTAMPDIFF(MONTH, a, b) >= 1 ORDER BY id"
        )
        .join(" "),
        "2 4 7 8 11 12"
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM pairs WHERE DATEDIFF(b, a) < 0 ORDER BY id"
        )
        .join(" "),
        "6 9 10 13"
    );

    // The binary protocol sends the same count as an eight-byte integer.
    let (columns, rows) = prepared_rows(
        &mut adapter,
        "SELECT TIMESTAMPDIFF(MONTH, a, b), DATEDIFF(b, a) FROM pairs WHERE id = ?",
        &one_number(7),
    );
    assert_eq!(columns[0].column_length, 21);
    assert_eq!(columns[1].column_length, 9);
    assert_eq!(
        rows,
        [[
            BinaryResultValue::Integer(11),
            BinaryResultValue::Integer(365)
        ]]
    );
}

/// `DATEDIFF(NOW(), created_at)` is how a report asks how old a row is. A
/// moment written out as a word is read the way MySQL reads one, and a reading
/// of the clock stands where a column does. Measured, each answers the shape
/// the same count over two columns answers.
#[test]
fn the_counts_take_a_reading_of_the_clock_or_a_written_moment() {
    let (_directory, mut adapter) = adapter();
    pairs_of_moments(&mut adapter);
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT DATEDIFF('2024-1-5', '2024-01-01'), TIMESTAMPDIFF(HOUR, '2024-01-01', '2024-01-02 03:00'), DATEDIFF(a, '2024-01-01') FROM pairs WHERE id = 1"
        ),
        [[Some("4".to_owned()), Some("27".to_owned()), Some("30".to_owned())]]
    );

    // Today lies at least 970 days after 2024-01-31, the day this was
    // measured, and a day holds no more than one date.
    let [days, day_count, days_from_today] = rows(
        &mut adapter,
        "SELECT DATEDIFF(NOW(), a), TIMESTAMPDIFF(DAY, a, NOW()), DATEDIFF(CURDATE(), a) FROM pairs WHERE id = 1",
    )
    .remove(0)
    .try_into()
    .unwrap();
    let days: i64 = days.unwrap().parse().unwrap();
    assert!(days >= 970, "{days}");
    assert_eq!(days_from_today.unwrap().parse::<i64>().unwrap(), days);
    assert!(
        (days - 1..=days).contains(&day_count.unwrap().parse::<i64>().unwrap()),
        "{days}"
    );
    assert_eq!(
        shapes(
            &mut adapter,
            "SELECT DATEDIFF(NOW(), a), TIMESTAMPDIFF(DAY, a, NOW()) FROM pairs"
        ),
        [
            (
                MYSQL_TYPE_LONGLONG,
                9,
                0,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                MYSQL_TYPE_LONGLONG,
                21,
                0,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM pairs WHERE TIMESTAMPDIFF(DAY, a, NOW()) > 900 ORDER BY id"
        )
        .join(" "),
        "1 2 3 4 5 6 7 8 10 11 12 13 14 15 16"
    );

    // A word that names no moment answers NULL in MySQL, a moment bound to a
    // `?` is read by rules of its own, a time of day is a span rather than a
    // moment, and a column of words is a coercion; none has been matched.
    for sql in [
        "SELECT DATEDIFF('2024-02-30', a) FROM pairs",
        "SELECT TIMESTAMPDIFF(DAY, a, ?) FROM pairs",
        "SELECT DATEDIFF(CURTIME(), a) FROM pairs",
        "SELECT DATEDIFF(label, a) FROM pairs",
        "SELECT TIMESTAMPDIFF(DAY, id, a) FROM pairs",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
