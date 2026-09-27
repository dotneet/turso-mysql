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

fn moments_to_shift(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>) {
    run(
        adapter,
        "CREATE TABLE items (id INT NOT NULL PRIMARY KEY, created_at DATETIME, d DATE, f DATETIME(3), ts TIMESTAMP(3) NULL, label VARCHAR(8), n INT)",
    );
    run(
        adapter,
        concat!(
            "INSERT INTO items (id, created_at, d, f, ts, n) VALUES ",
            "(1, '2024-01-31 10:20:30', '2024-01-31', '2024-01-31 10:20:30.250', '2024-01-31 10:20:30.250', 2), ",
            "(2, '2023-12-31 00:00:00', '2024-02-29', '2023-12-31 00:00:00', NULL, NULL), ",
            "(3, NULL, NULL, '2024-03-31 12:00:00.5', '2024-03-31 12:00:00.5', -3)"
        ),
    );
}

/// `created_at + INTERVAL 1 DAY` is the operator spelling of `DATE_ADD`, and
/// `created_at - INTERVAL 1 HOUR` of `DATE_SUB`. Measured, each answers the
/// moment and the shape the call answers, and MySQL names the column after
/// the whole of it, `INTERVAL` and unit included. A moment keeps its fraction
/// of a second, and its column's decimals with it.
#[test]
fn an_interval_added_or_taken_away_is_the_shift_the_call_makes() {
    let (_directory, mut adapter) = adapter();
    moments_to_shift(&mut adapter);
    let sql = concat!(
        "SELECT created_at + INTERVAL 1 DAY, created_at - INTERVAL 1 HOUR, d + INTERVAL 1 DAY, ",
        "d + INTERVAL 1 HOUR, INTERVAL 1 DAY + d, d - INTERVAL 1 MONTH, f + INTERVAL 1 DAY, ",
        "f - INTERVAL 1 MONTH, ts + INTERVAL 1 HOUR, DATE_ADD(f, INTERVAL 1 WEEK) FROM items ORDER BY id"
    );
    let result = selected(&mut adapter, sql);
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        [
            "created_at + INTERVAL 1 DAY",
            "created_at - INTERVAL 1 HOUR",
            "d + INTERVAL 1 DAY",
            "d + INTERVAL 1 HOUR",
            "INTERVAL 1 DAY + d",
            "d - INTERVAL 1 MONTH",
            "f + INTERVAL 1 DAY",
            "f - INTERVAL 1 MONTH",
            "ts + INTERVAL 1 HOUR",
            "DATE_ADD(f, INTERVAL 1 WEEK)",
        ]
    );
    assert_eq!(
        rows(&mut adapter, sql),
        [
            [
                "2024-02-01 10:20:30",
                "2024-01-31 09:20:30",
                "2024-02-01",
                "2024-01-31 01:00:00",
                "2024-02-01",
                "2023-12-31",
                "2024-02-01 10:20:30.250",
                "2023-12-31 10:20:30.250",
                "2024-01-31 11:20:30.250",
                "2024-02-07 10:20:30.250",
            ]
            .map(|value| Some(value.to_owned()))
            .to_vec(),
            [
                Some("2024-01-01 00:00:00"),
                Some("2023-12-30 23:00:00"),
                Some("2024-03-01"),
                Some("2024-02-29 01:00:00"),
                Some("2024-03-01"),
                Some("2024-01-29"),
                Some("2024-01-01 00:00:00.000"),
                Some("2023-11-30 00:00:00.000"),
                None,
                Some("2024-01-07 00:00:00.000"),
            ]
            .map(|value| value.map(str::to_owned))
            .to_vec(),
            [
                None,
                None,
                None,
                None,
                None,
                None,
                Some("2024-04-01 12:00:00.500"),
                Some("2024-02-29 12:00:00.500"),
                Some("2024-03-31 13:00:00.500"),
                Some("2024-04-07 12:00:00.500"),
            ]
            .map(|value| value.map(str::to_owned))
            .to_vec(),
        ]
    );
    let moment = (MYSQL_TYPE_DATETIME, 19, 0, MYSQL_BINARY_FLAG);
    let day = (MYSQL_TYPE_DATE, 10, 0, MYSQL_BINARY_FLAG);
    let moment_to_the_millisecond = (MYSQL_TYPE_DATETIME, 23, 3, MYSQL_BINARY_FLAG);
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            moment,
            moment,
            day,
            moment,
            day,
            day,
            moment_to_the_millisecond,
            moment_to_the_millisecond,
            moment_to_the_millisecond,
            moment_to_the_millisecond,
        ]
    );

    // A reading of the clock shifted by an operator answers what the call
    // over it answers: a moment for `NOW()`, and a day for `CURDATE()`
    // shifted by whole days.
    assert_eq!(
        shapes(
            &mut adapter,
            "SELECT NOW() - INTERVAL 1 DAY, CURDATE() - INTERVAL 1 DAY, CURDATE() + INTERVAL 1 HOUR FROM items"
        ),
        [moment, day, moment]
    );

    // The binary protocol sends the kept fraction as microseconds.
    let (_, binary) = prepared_rows(
        &mut adapter,
        "SELECT f + INTERVAL 1 DAY FROM items WHERE id = ?",
        &one_number(3),
    );
    assert_eq!(
        binary,
        [[BinaryResultValue::DateTimeMicros {
            year: 2024,
            month: 4,
            day: 1,
            hour: 12,
            minute: 0,
            second: 0,
            microseconds: 500_000,
        }]]
    );

    // A count read from a row is multiplied for a week or a quarter, which has
    // not been taught; a word is a coercion; and two shifts in a row are a
    // shift of something other than a moment.
    for sql in [
        "SELECT created_at + INTERVAL n DAY FROM items",
        "SELECT label + INTERVAL 1 DAY FROM items",
        "SELECT created_at + INTERVAL 1 DAY + INTERVAL 1 HOUR FROM items",
        "SELECT created_at + INTERVAL ? DAY FROM items",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `WHERE created_at > NOW() - INTERVAL 1 DAY` is how a suite asks for the
/// rows of the last day, and `NOW() + INTERVAL 1 HOUR` how a row records when
/// it runs out. Each is read the way the `DATE_SUB` and `DATE_ADD` spellings
/// already are.
#[test]
fn a_shifted_clock_reading_is_compared_against_and_written() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE sessions (id INT NOT NULL PRIMARY KEY, created_at DATETIME NOT NULL, expires_at DATETIME, day DATE)",
    );
    run(
        &mut adapter,
        concat!(
            "INSERT INTO sessions (id, created_at, expires_at, day) VALUES ",
            "(1, NOW() - INTERVAL 2 HOUR, NOW() + INTERVAL 1 HOUR, CURDATE() - INTERVAL 1 DAY), ",
            "(2, NOW() - INTERVAL 3 DAY, NOW() - INTERVAL 2 DAY, CURDATE() - INTERVAL 7 DAY)"
        ),
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM sessions WHERE created_at > NOW() - INTERVAL 1 DAY ORDER BY id"
        ),
        ["1"]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM sessions WHERE expires_at < NOW() ORDER BY id"
        ),
        ["2"]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM sessions WHERE day >= CURDATE() - INTERVAL 2 DAY ORDER BY id"
        ),
        ["1"]
    );
    run(
        &mut adapter,
        "UPDATE sessions SET expires_at = NOW() + INTERVAL 30 MINUTE WHERE id = 1",
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM sessions WHERE expires_at BETWEEN NOW() AND NOW() + INTERVAL 31 MINUTE"
        ),
        ["1"]
    );
    run(
        &mut adapter,
        "DELETE FROM sessions WHERE created_at < NOW() - INTERVAL 2 DAY",
    );
    assert_eq!(column(&mut adapter, "SELECT id FROM sessions"), ["1"]);
}

/// A moment written out as a word is shifted as well. Measured, MySQL answers
/// a word rather than a moment — a STRING of 116 with no flags — reads the
/// word the way it reads any, keeps a day written alone a day, and writes a
/// fraction of a second out to six places.
#[test]
fn a_written_moment_is_shifted_into_a_word() {
    let (_directory, mut adapter) = adapter();
    moments_to_shift(&mut adapter);
    let sql = concat!(
        "SELECT '2026-01-01' + INTERVAL 1 HOUR, '2026-1-1' + INTERVAL 1 DAY, ",
        "'2026-01-01 10:00:00.5' + INTERVAL 1 DAY, '2026-01-31' - INTERVAL 1 MONTH, ",
        "DATE_ADD('2026-01-31', INTERVAL 1 MONTH) FROM items WHERE id = 1"
    );
    assert_eq!(
        rows(&mut adapter, sql),
        [[
            "2026-01-01 01:00:00",
            "2026-01-02",
            "2026-01-02 10:00:00.500000",
            "2025-12-31",
            "2026-02-28",
        ]
        .map(|value| Some(value.to_owned()))]
    );
    let result = selected(&mut adapter, sql);
    for column in &result.columns {
        assert_eq!(
            (
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
                column.character_set
            ),
            (
                MYSQL_TYPE_STRING,
                116,
                NOT_FIXED_DECIMALS,
                0,
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            ),
            "{}",
            column.name
        );
    }

    // A word that names no moment answers NULL in MySQL, and one written any
    // other way is read by rules this has not measured.
    for sql in [
        "SELECT '2026-02-30' + INTERVAL 1 DAY FROM items",
        "SELECT '20260101' + INTERVAL 1 DAY FROM items",
        "SELECT '2026/01/01' + INTERVAL 1 DAY FROM items",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

fn days_around_new_year(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>) {
    run(
        adapter,
        "CREATE TABLE wk (id INT NOT NULL PRIMARY KEY, d DATE, label VARCHAR(8))",
    );
    run(
        adapter,
        concat!(
            "INSERT INTO wk (id, d) VALUES (1, '2024-01-01'), (2, '2024-12-29'), ",
            "(3, '2024-12-30'), (4, '2024-12-31'), (5, '2025-01-01'), (6, '2026-01-01'), ",
            "(7, '2026-01-04'), (8, '2027-01-01'), (9, '2027-01-03'), (10, '2020-12-31'), ",
            "(11, '2021-01-03'), (12, '2023-01-01'), (13, NULL), (14, '2024-02-29')"
        ),
    );
}

/// `WEEK` numbers a week by one of MySQL's eight countings, and `DAYNAME` and
/// `MONTHNAME` name the day and the month, which is how a report groups rows
/// by week or labels them.
#[test]
fn week_dayname_and_monthname_read_the_calendar_the_way_mysql_does() {
    let (_directory, mut adapter) = adapter();
    days_around_new_year(&mut adapter);
    for (call, answers) in [
        ("WEEK(d)", "0 52 52 52 0 0 1 0 1 52 1 1 NULL 8"),
        ("WEEK(d, 0)", "0 52 52 52 0 0 1 0 1 52 1 1 NULL 8"),
        ("WEEK(d, 1)", "1 52 53 53 1 1 1 0 0 53 0 0 NULL 9"),
        ("WEEK(d, 2)", "53 52 52 52 52 52 1 52 1 52 1 1 NULL 8"),
        ("WEEK(d, 3)", "1 52 1 1 1 1 1 53 53 53 53 52 NULL 9"),
        ("WEEK(d, 4)", "1 53 53 53 1 0 1 0 1 53 1 1 NULL 9"),
        ("WEEK(d, 5)", "1 52 53 53 0 0 0 0 0 52 0 0 NULL 9"),
        ("WEEK(d, 6)", "1 1 1 1 1 53 1 52 1 53 1 1 NULL 9"),
        ("WEEK(d, 7)", "1 52 53 53 53 52 52 52 52 52 52 52 NULL 9"),
        (
            "DAYNAME(d)",
            "Monday Sunday Monday Tuesday Wednesday Thursday Sunday Friday Sunday Thursday Sunday Sunday NULL Thursday",
        ),
        (
            "MONTHNAME(d)",
            "January December December December January January January January January December January January NULL February",
        ),
    ] {
        let sql = format!("SELECT {call} FROM wk ORDER BY id");
        assert_eq!(column(&mut adapter, &sql).join(" "), answers, "{sql}");
    }
    assert_eq!(
        shapes(
            &mut adapter,
            "SELECT WEEK(d), WEEK(d, 3), DAYNAME(d), MONTHNAME(d), DAYNAME(NOW()), WEEK(NOW()) FROM wk"
        ),
        [
            (MYSQL_TYPE_LONGLONG, 3, 0, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONGLONG, 3, 0, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            (MYSQL_TYPE_VAR_STRING, 36, NOT_FIXED_DECIMALS, 0),
            (MYSQL_TYPE_VAR_STRING, 36, NOT_FIXED_DECIMALS, 0),
            (MYSQL_TYPE_VAR_STRING, 36, NOT_FIXED_DECIMALS, 0),
            (MYSQL_TYPE_LONGLONG, 3, 0, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
        ]
    );
    assert_eq!(
        selected(&mut adapter, "SELECT DAYNAME(d) FROM wk").columns[0].character_set,
        u16::from(DEFAULT_UTF8MB4_COLLATION)
    );

    // A name is compared without regard to case, and a week as a number.
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM wk WHERE DAYNAME(d) = 'sunday' ORDER BY id"
        )
        .join(" "),
        "2 7 9 11 12"
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM wk WHERE WEEK(d, 3) = 1 ORDER BY id"
        )
        .join(" "),
        "1 3 4 5 6 7"
    );
    let (_, binary) = prepared_rows(
        &mut adapter,
        "SELECT WEEK(d, 1), DAYNAME(d) FROM wk WHERE id = ?",
        &one_number(3),
    );
    assert_eq!(
        binary,
        [[
            BinaryResultValue::Integer(53),
            BinaryResultValue::Text("Monday".to_owned())
        ]]
    );

    // Measured, MySQL takes any mode and counts by its last three bits, and
    // coerces a word; a bound mode is read by the type the client sent.
    for sql in [
        "SELECT WEEK(d, 8) FROM wk",
        "SELECT WEEK(d, ?) FROM wk",
        "SELECT WEEK(d, id) FROM wk",
        "SELECT DAYNAME(label) FROM wk",
        "SELECT MONTHNAME('2024-02-30') FROM wk",
        "SELECT DAYNAME(CURTIME()) FROM wk",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `UTC_TIMESTAMP()`, `UTC_DATE()` and `UTC_TIME()` read the clock in UTC and
/// `SYSDATE()` reads it as it runs. Measured, each reports the shape `NOW()`,
/// `CURDATE()` or `CURTIME()` reports — NOT NULL — and this server's clock
/// reads UTC, so each answers what its relative answers. A session in another
/// zone is refused them, as it is `NOW()`.
#[test]
fn the_utc_readings_and_sysdate_read_the_clock_like_now() {
    let (_directory, mut adapter) = adapter();
    days_around_new_year(&mut adapter);
    assert_eq!(
        shapes(
            &mut adapter,
            "SELECT UTC_TIMESTAMP(), UTC_DATE(), UTC_TIME(), SYSDATE() FROM wk"
        ),
        [
            (
                MYSQL_TYPE_DATETIME,
                19,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_DATE,
                10,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_TIME,
                8,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_DATETIME,
                19,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
        ]
    );
    let [utc_day, today] = rows(
        &mut adapter,
        "SELECT UTC_DATE(), CURDATE() FROM wk WHERE id = 1",
    )
    .remove(0)
    .try_into()
    .unwrap();
    assert_eq!(utc_day, today);
    assert_eq!(
        column(
            &mut adapter,
            "SELECT id FROM wk WHERE d < UTC_DATE() + INTERVAL 100 YEAR ORDER BY id"
        )
        .join(" "),
        "1 2 3 4 5 6 7 8 9 10 11 12 14"
    );
    assert!(column(
        &mut adapter,
        "SELECT id FROM wk WHERE d < UTC_DATE() - INTERVAL 100 YEAR"
    )
    .is_empty());
    // 2024-01-01 lies at least 1000 days before the day this was written.
    let days: i64 = column(
        &mut adapter,
        "SELECT DATEDIFF(UTC_TIMESTAMP(), d) FROM wk WHERE id = 1",
    )[0]
    .parse()
    .unwrap();
    assert!(days >= 1000, "{days}");

    run(&mut adapter, "SET time_zone = '+09:00'");
    for sql in [
        "SELECT UTC_TIMESTAMP() FROM wk",
        "SELECT UTC_DATE() FROM wk",
        "SELECT SYSDATE() FROM wk",
        "SELECT NOW() FROM wk",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `FROM_UNIXTIME(n, 'fmt')` is how a report writes out a moment an
/// application stored as seconds from the epoch. Measured on MySQL 8.4.11 in a
/// UTC session, it writes the moment the way `DATE_FORMAT` does and reports
/// the width `DATE_FORMAT` reports for the same format, and a count before the
/// epoch or past `3001-01-18 23:59:59` answers no moment at all — with or
/// without a format.
#[test]
fn from_unixtime_writes_the_moment_out_by_a_format() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE epochs (id INT NOT NULL PRIMARY KEY, n BIGINT, label VARCHAR(8))",
    );
    run(
        &mut adapter,
        "INSERT INTO epochs (id, n) VALUES (1, 0), (2, 1700000000), (3, -1), (4, NULL), (5, 32536771199), (6, 32536771200)",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT FROM_UNIXTIME(n, '%Y-%m-%d %H:%i:%s'), FROM_UNIXTIME(n, '%W %M'), FROM_UNIXTIME(n) FROM epochs ORDER BY id"
        ),
        [
            [
                Some("1970-01-01 00:00:00"),
                Some("Thursday January"),
                Some("1970-01-01 00:00:00")
            ],
            [
                Some("2023-11-14 22:13:20"),
                Some("Tuesday November"),
                Some("2023-11-14 22:13:20")
            ],
            [None, None, None],
            [None, None, None],
            [
                Some("3001-01-18 23:59:59"),
                Some("Sunday January"),
                Some("3001-01-18 23:59:59")
            ],
            [None, None, None],
        ]
        .map(|row| row.map(|value| value.map(str::to_owned)).to_vec())
    );
    assert_eq!(
        shapes(
            &mut adapter,
            "SELECT FROM_UNIXTIME(n, '%Y-%m-%d'), FROM_UNIXTIME(n, '%W %M'), FROM_UNIXTIME(1700000000, '%Y') FROM epochs"
        ),
        [
            (MYSQL_TYPE_VAR_STRING, 40, NOT_FIXED_DECIMALS, 0),
            (MYSQL_TYPE_VAR_STRING, 516, NOT_FIXED_DECIMALS, 0),
            (MYSQL_TYPE_VAR_STRING, 16, NOT_FIXED_DECIMALS, 0),
        ]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT FROM_UNIXTIME(1700000000, '%Y') FROM epochs WHERE id = 1"
        ),
        ["2023"]
    );

    // A fraction is carried into the moment and a word read as the number it
    // begins with, by rules not measured here; a bound count is read by the
    // type the client sent.
    for sql in [
        "SELECT FROM_UNIXTIME(label, '%Y') FROM epochs",
        "SELECT FROM_UNIXTIME(1.5, '%Y') FROM epochs",
        "SELECT FROM_UNIXTIME(?, '%Y') FROM epochs",
        "SELECT FROM_UNIXTIME(n, ?) FROM epochs",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
