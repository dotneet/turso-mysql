//! `%`, `DIV`, a negated column, and `ROUND`, `FLOOR` and `CEIL` over each
//! kind of number: the values each answers and the columns it reports.
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
            AccountId::from_bytes([143; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE nm (id INT NOT NULL PRIMARY KEY, i INT, b BIGINT, t TINYINT, sm SMALLINT, fl TINYINT(1), r DOUBLE, d DECIMAL(10,2), d3 DECIMAL(10,3))",
        "INSERT INTO nm VALUES (1, -7, -9223372036854775808, -128, 7, 1, -2.5, -7.25, -1.005), (2, 7, 9223372036854775807, 127, -7, 0, 2.5, 7.25, 2.675), (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), (4, 15, 25, 5, -5, 1, 0.15, 0.15, 0.155), (5, 2147483647, 1, 1, 1, 1, 1.005, 0.5, 1234567.125), (6, -25, 1, 1, 1, 1, 2.675, -0.5, -0.155), (7, 0, 0, 0, 0, 0, 1e20, 0, 0.5), (8, 0, 0, 0, 0, 0, -0.4, 0, 0)",
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
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
}

/// Each row of the result, its NULLs written as `NULL`.
fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    result(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| {
                    value.map_or("NULL".to_owned(), |value| String::from_utf8(value).unwrap())
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[test]
fn numeric_operators_report_the_column_mysql_reports() {
    let (_directory, mut adapter) = adapter();
    let numeric = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    for (sql, column_type, length, decimals, flags) in [
        // As wide as the column, and 4 over a TINYINT(1) that reports 1.
        ("SELECT -i FROM nm", MYSQL_TYPE_LONGLONG, 11, 0, numeric),
        ("SELECT -t FROM nm", MYSQL_TYPE_LONGLONG, 4, 0, numeric),
        ("SELECT -sm FROM nm", MYSQL_TYPE_LONGLONG, 6, 0, numeric),
        ("SELECT -fl FROM nm", MYSQL_TYPE_LONGLONG, 4, 0, numeric),
        (
            "SELECT -id FROM nm",
            MYSQL_TYPE_LONGLONG,
            11,
            0,
            numeric | MYSQL_NOT_NULL_FLAG,
        ),
        // `%` and `DIV` can divide by zero, so they are nullable whatever
        // the column is.
        ("SELECT id % 2 FROM nm", MYSQL_TYPE_LONGLONG, 11, 0, numeric),
        ("SELECT t % 2 FROM nm", MYSQL_TYPE_LONGLONG, 4, 0, numeric),
        ("SELECT b % 10 FROM nm", MYSQL_TYPE_LONGLONG, 20, 0, numeric),
        ("SELECT fl % 2 FROM nm", MYSQL_TYPE_LONGLONG, 4, 0, numeric),
        (
            "SELECT MOD(fl, 2) FROM nm",
            MYSQL_TYPE_LONGLONG,
            4,
            0,
            numeric,
        ),
        (
            "SELECT id DIV 2 FROM nm",
            MYSQL_TYPE_LONGLONG,
            11,
            0,
            numeric,
        ),
        (
            "SELECT b DIV 2 FROM nm",
            MYSQL_TYPE_LONGLONG,
            20,
            0,
            numeric,
        ),
        (
            "SELECT fl DIV 2 FROM nm",
            MYSQL_TYPE_LONGLONG,
            4,
            0,
            numeric,
        ),
        // ROUND answers the kind of number its column holds.
        (
            "SELECT ROUND(i, -1) FROM nm",
            MYSQL_TYPE_LONGLONG,
            21,
            0,
            numeric,
        ),
        (
            "SELECT ROUND(id, 1) FROM nm",
            MYSQL_TYPE_LONGLONG,
            21,
            0,
            numeric | MYSQL_NOT_NULL_FLAG,
        ),
        (
            "SELECT ROUND(r) FROM nm",
            MYSQL_TYPE_DOUBLE,
            23,
            NOT_FIXED_DECIMALS,
            numeric,
        ),
        (
            "SELECT ROUND(r, 1) FROM nm",
            MYSQL_TYPE_DOUBLE,
            23,
            NOT_FIXED_DECIMALS,
            numeric,
        ),
        (
            "SELECT FLOOR(r) FROM nm",
            MYSQL_TYPE_DOUBLE,
            23,
            NOT_FIXED_DECIMALS,
            numeric,
        ),
        (
            "SELECT CEIL(r) FROM nm",
            MYSQL_TYPE_DOUBLE,
            23,
            NOT_FIXED_DECIMALS,
            numeric,
        ),
        (
            "SELECT FLOOR(i) FROM nm",
            MYSQL_TYPE_LONGLONG,
            21,
            0,
            numeric,
        ),
        // A DECIMAL keeps the places it is rounded to, held to its own scale,
        // and one more whole digit for the carry when a place is cut away.
        (
            "SELECT ROUND(d) FROM nm",
            MYSQL_TYPE_NEWDECIMAL,
            10,
            0,
            numeric,
        ),
        (
            "SELECT ROUND(d, 1) FROM nm",
            MYSQL_TYPE_NEWDECIMAL,
            12,
            1,
            numeric,
        ),
        (
            "SELECT ROUND(d3) FROM nm",
            MYSQL_TYPE_NEWDECIMAL,
            9,
            0,
            numeric,
        ),
        (
            "SELECT ROUND(d3, 1) FROM nm",
            MYSQL_TYPE_NEWDECIMAL,
            11,
            1,
            numeric,
        ),
        (
            "SELECT ROUND(d3, 2) FROM nm",
            MYSQL_TYPE_NEWDECIMAL,
            12,
            2,
            numeric,
        ),
        (
            "SELECT ROUND(d3, 5) FROM nm",
            MYSQL_TYPE_NEWDECIMAL,
            12,
            3,
            numeric,
        ),
    ] {
        let text = result(&mut adapter, sql);
        let column = &text.columns[0];
        assert_eq!(
            (
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags
            ),
            (column_type, length, decimals, flags),
            "{sql}"
        );
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(prepared.columns, text.columns, "{sql}");
        let Ok(PreparedStatementExecutionResult::ResultSet(_)) =
            adapter.execute_stmt_execute(prepared.statement_id, &[])
        else {
            panic!("{sql} must answer rows when prepared");
        };
        adapter.execute_stmt_close(prepared.statement_id);
    }
    assert_eq!(
        result(&mut adapter, "SELECT -i FROM nm").columns[0].name,
        "-i"
    );
}

#[test]
fn whole_division_keeps_the_dividends_sign_and_cuts_toward_zero() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT -i, -t, i % 2, i % -3, b % 10, i DIV 2, i DIV -2, t DIV 2, b DIV 2 FROM nm WHERE id <= 4 ORDER BY id"
        ),
        [
            "7 128 -1 -1 -8 -3 3 -64 -4611686018427387904",
            "-7 -127 1 1 7 3 -3 63 4611686018427387903",
            "NULL NULL NULL NULL NULL NULL NULL NULL NULL",
            "-15 -5 1 0 5 7 -7 2 12",
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id FROM nm WHERE id < 5 ORDER BY -i, id"
        ),
        ["3", "4", "2", "1"]
    );
}

#[test]
fn round_rounds_each_kind_of_number_the_way_mysql_does() {
    let (_directory, mut adapter) = adapter();
    // A whole number rounds half away from zero left of the point and is left
    // alone right of it.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT ROUND(i, -1), ROUND(i, -20), ROUND(i, 30), ROUND(t, -2), ROUND(b, 2) FROM nm WHERE id IN (1, 2, 4, 5, 6) ORDER BY id"
        ),
        [
            "-10 0 -7 -100 -9223372036854775808",
            "10 0 7 100 9223372036854775807",
            "20 0 15 0 25",
            "2147483650 0 2147483647 0 1",
            "-30 0 -25 0 1",
        ]
    );
    // A DOUBLE is scaled, rounded half to even, and scaled back: 0.15 times
    // ten is 1.5 exactly, 1.005 times a hundred falls short of 100.5, and
    // 2.675 times a hundred reaches 267.5.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT ROUND(r), ROUND(r, 1), ROUND(r, 2), FLOOR(r), CEIL(r), ROUND(r, 400), ROUND(r, -400) FROM nm WHERE id <> 3 ORDER BY id"
        ),
        [
            "-2 -2.5 -2.5 -3 -2 -2.5 0",
            "2 2.5 2.5 2 3 2.5 0",
            "0 0.2 0.15 0 1 0.15 0",
            "1 1 1 1 2 1.005 0",
            "3 2.7 2.68 2 3 2.675 0",
            "1e20 1e20 1e20 1e20 1e20 1e20 0",
            // A negative zero is written with its sign.
            "-0 -0.4 -0.4 -1 -0 -0.4 0",
        ]
    );
    // A DECIMAL rounds half away from zero, and keeps its places.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT ROUND(d), ROUND(d, 1), ROUND(d, 5), ROUND(d3), ROUND(d3, 1), ROUND(d3, 2) FROM nm WHERE id <> 3 ORDER BY id"
        ),
        [
            "-7 -7.3 -7.25 -1 -1.0 -1.01",
            "7 7.3 7.25 3 2.7 2.68",
            "0 0.2 0.15 0 0.2 0.16",
            "1 0.5 0.50 1234567 1234567.1 1234567.13",
            "-1 -0.5 -0.50 0 -0.2 -0.16",
            "0 0.0 0.00 1 0.5 0.50",
            "0 0.0 0.00 0 0.0 0.00",
        ]
    );
}

/// `FORMAT` over a DECIMAL rounds the exact decimal, half away from zero.
#[test]
fn format_groups_a_decimal_rounded_as_the_decimal_it_is() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query("CREATE TABLE fm (id INT PRIMARY KEY, d DECIMAL(10,3))")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO fm VALUES (1, 1234567.125), (2, -0.005), (3, -1234.5), (4, NULL), (5, -0.004), (6, 9999999.995)",
        )
        .unwrap();
    let sql = "SELECT FORMAT(d, 2), FORMAT(d, 0), FORMAT(d, 5), FORMAT(d, -1) FROM fm ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            "1,234,567.13 1,234,567 1,234,567.12500 1,234,567",
            "-0.01 0 -0.00500 0",
            "-1,234.50 -1,235 -1,234.50000 -1,235",
            "NULL NULL NULL NULL",
            "0.00 0 -0.00400 0",
            "10,000,000.00 10,000,000 9,999,999.99500 10,000,000",
        ]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT FORMAT(d, 40) FROM fm WHERE id = 1"),
        ["1,234,567.125000000000000000000000000000"]
    );
    // The width is the column's own 12, a comma for every three digits, and
    // thirty-two, four bytes to the character.
    let column = &result(&mut adapter, sql).columns[0];
    assert_eq!(
        (column.column_type, column.column_length),
        (MYSQL_TYPE_VAR_STRING, 192)
    );
}

#[test]
fn numeric_operators_refuse_what_mysql_answers_by_another_rule() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // The smallest BIGINT has no negative, and MySQL answers 1690 for it
        // and for a BIGINT rounded past the largest.
        "SELECT -b FROM nm",
        "SELECT ROUND(b, -1) FROM nm",
        "SELECT i DIV -1 FROM nm",
        // MySQL answers NULL with a warning for a zero divisor.
        "SELECT i % 0 FROM nm",
        "SELECT i DIV 0 FROM nm",
        // Over a real or a decimal each answers a real or a decimal.
        "SELECT r % 2 FROM nm",
        "SELECT MOD(r, 2) FROM nm",
        "SELECT -r FROM nm",
        "SELECT d DIV 2 FROM nm",
        "SELECT -d FROM nm",
        // A DECIMAL rounded left of the point, or to a whole number down or up.
        "SELECT ROUND(d, -1) FROM nm",
        "SELECT FLOOR(d) FROM nm",
        // Places read from the row.
        "SELECT ROUND(r, i) FROM nm",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
}
