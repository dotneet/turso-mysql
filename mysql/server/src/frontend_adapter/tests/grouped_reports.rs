//! Grouped reports — a count or a total for each day, each month or each
//! user — and what MySQL's `ONLY_FULL_GROUP_BY` lets one name.
//!
//! Every expectation here was measured on MySQL 8.4.11 over the same rows.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([151; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100), status VARCHAR(20), age INT NULL, balance DECIMAL(10,2), created_at DATETIME NULL)",
        "CREATE TABLE posts (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, user_id BIGINT UNSIGNED, title VARCHAR(200), views INT NOT NULL DEFAULT 0, published TINYINT(1), created_at DATETIME NULL)",
        "INSERT INTO users (name, status, age, balance, created_at) VALUES ('ann', 'active', 30, 10.50, '2026-01-05 10:00:00'), ('bob', 'inactive', NULL, 0.00, '2026-02-01 09:30:00'), ('cid', 'active', 41, 99.99, NULL), ('dee', NULL, 25, NULL, '2026-02-14 23:59:59')",
        "INSERT INTO posts (user_id, title, views, published, created_at) VALUES (1, 'a', 10, 1, '2026-01-05 10:00:00'), (1, 'b', 3, 0, '2026-01-05 18:00:00'), (2, 'c', 7, 1, '2026-02-01 09:30:00'), (3, 'd', 0, NULL, NULL), (NULL, 'e', 5, 1, '2025-12-31 23:59:59')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// A result column's type, length, decimals and flags.
type Shape = (u8, u32, u8, u16);

/// Every row, each value as the text protocol writes it.
type Rows = Vec<Vec<Option<String>>>;

/// Runs a statement in both protocols, holds the two to the same columns, and
/// answers the text protocol's rows with the columns' shapes.
fn report(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> (Vec<Shape>, Rows) {
    let Ok(CommandExecutionResult::ResultSet(text)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns, "{sql}");
    let binary = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(binary.columns, text.columns, "{sql}");
    assert_eq!(binary.rows.len(), text.rows.len(), "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
    let shapes = text
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
        .collect();
    let rows = text
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| value.clone().map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect();
    (shapes, rows)
}

fn rows(written: &[&[Option<&str>]]) -> Rows {
    written
        .iter()
        .map(|row| row.iter().map(|value| value.map(str::to_owned)).collect())
        .collect()
}

/// MySQL groups by an expression in a temporary table and reports each answer
/// that table stores as the table's column: a count without the binary flag,
/// a `YEAR` as a `LONG`, and a grouping key with the group flag, which a
/// client reads as the numeric one.
#[test]
fn grouping_by_a_day_or_a_month_answers_the_shapes_mysql_reads_back() {
    let (_directory, mut adapter) = adapter();
    let count = (
        MYSQL_TYPE_LONGLONG,
        21,
        0,
        MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
    );

    let (shapes, answered) = report(
        &mut adapter,
        "SELECT DATE(created_at) AS d, COUNT(*) FROM posts GROUP BY DATE(created_at) ORDER BY d",
    );
    assert_eq!(
        shapes,
        [
            (MYSQL_TYPE_DATE, 10, 0, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            count
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[None, Some("1")],
            &[Some("2025-12-31"), Some("1")],
            &[Some("2026-01-05"), Some("2")],
            &[Some("2026-02-01"), Some("1")],
        ])
    );

    let (shapes, answered) = report(
        &mut adapter,
        "SELECT YEAR(created_at), MONTH(created_at), COUNT(*) FROM posts GROUP BY YEAR(created_at), MONTH(created_at) ORDER BY 1, 2",
    );
    assert_eq!(
        shapes,
        [
            (MYSQL_TYPE_LONG, 4, 0, MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONG, 3, 0, MYSQL_NUM_FLAG),
            count
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[None, None, Some("1")],
            &[Some("2025"), Some("12"), Some("1")],
            &[Some("2026"), Some("1"), Some("2")],
            &[Some("2026"), Some("2"), Some("1")],
        ])
    );

    // A total, a largest and a smallest are stored and lose the binary flag
    // and the 31 decimals words carry; an average is worked out afterwards
    // and keeps its own shape.
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT DATE_FORMAT(created_at, '%Y-%m'), SUM(views), MAX(views), AVG(views), MIN(title) FROM posts GROUP BY DATE_FORMAT(created_at, '%Y-%m') ORDER BY 1",
    );
    assert_eq!(
        shapes,
        [
            (MYSQL_TYPE_VAR_STRING, 28, 0, MYSQL_NUM_FLAG),
            (MYSQL_TYPE_NEWDECIMAL, 33, 0, MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONG, 11, 0, MYSQL_NUM_FLAG),
            (
                MYSQL_TYPE_NEWDECIMAL,
                16,
                4,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (MYSQL_TYPE_VAR_STRING, 800, 0, 0),
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[None, Some("0"), Some("0"), Some("0.0000"), Some("d")],
            &[
                Some("2025-12"),
                Some("5"),
                Some("5"),
                Some("5.0000"),
                Some("e")
            ],
            &[
                Some("2026-01"),
                Some("13"),
                Some("10"),
                Some("6.5000"),
                Some("a")
            ],
            &[
                Some("2026-02"),
                Some("7"),
                Some("7"),
                Some("7.0000"),
                Some("c")
            ],
        ])
    );

    // The function name in another case is the same key, and a HAVING may
    // name the projection's alias for it.
    let (_, answered) = report(
        &mut adapter,
        "SELECT date(created_at) AS d, COUNT(*) FROM posts GROUP BY DATE(created_at) HAVING d > '2026-01-01' ORDER BY d",
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("2026-01-05"), Some("2")],
            &[Some("2026-02-01"), Some("1")],
        ])
    );
}

/// A whole column beside an expression key, ordered by both. `user_id` is a
/// BIGINT UNSIGNED, which the engine keeps in a type of its own; ordering a
/// grouped statement by it used to fail.
#[test]
fn grouping_by_a_user_and_a_day_orders_the_way_mysql_does() {
    let (_directory, mut adapter) = adapter();
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT user_id, DATE(created_at), COUNT(*) FROM posts GROUP BY user_id, DATE(created_at) ORDER BY 1, 2",
    );
    assert_eq!(
        shapes[1..],
        [
            (MYSQL_TYPE_DATE, 10, 0, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            (
                MYSQL_TYPE_LONGLONG,
                21,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[None, Some("2025-12-31"), Some("1")],
            &[Some("1"), Some("2026-01-05"), Some("2")],
            &[Some("2"), Some("2026-02-01"), Some("1")],
            &[Some("3"), None, Some("1")],
        ])
    );

    let (_, answered) = report(
        &mut adapter,
        "SELECT user_id, COUNT(*) FROM posts GROUP BY user_id ORDER BY COUNT(*) DESC, user_id",
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("1"), Some("2")],
            &[None, Some("1")],
            &[Some("2"), Some("1")],
            &[Some("3"), Some("1")],
        ])
    );
}

/// What `ONLY_FULL_GROUP_BY` turns away, which MySQL answers 1055 or 1054
/// for, and a key the engine would group differently from MySQL.
#[test]
fn a_grouped_statement_refuses_what_only_full_group_by_refuses() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // 1055: a column in the projection that is not a whole grouping key.
        "SELECT UPPER(title), COUNT(*) FROM posts GROUP BY user_id",
        "SELECT created_at, COUNT(*) FROM posts GROUP BY DATE(created_at)",
        "SELECT YEAR(created_at), COUNT(*) FROM posts GROUP BY DATE(created_at)",
        // A key inside a larger expression is not the key.
        "SELECT UPPER(DATE(created_at)), COUNT(*) FROM posts GROUP BY DATE(created_at)",
        // 1055 for an ORDER BY.
        "SELECT user_id, COUNT(*) FROM posts GROUP BY user_id ORDER BY title",
        "SELECT DATE(created_at), COUNT(*) FROM posts GROUP BY DATE(created_at) ORDER BY created_at",
        // 1054 for a HAVING, even over a primary key and even inside the key.
        "SELECT id FROM posts GROUP BY id HAVING views > 1",
        "SELECT DATE(created_at), COUNT(*) FROM posts GROUP BY DATE(created_at) HAVING DATE(created_at) > '2026-01-01'",
        // MySQL groups words under the column's collation, where the engine
        // would group the call's answer by its bytes.
        "SELECT UPPER(title), COUNT(*) FROM posts GROUP BY UPPER(title)",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}

/// `ROUND(AVG(n), 2)` is how a report prints an average. MySQL answers a
/// decimal worked out from the aggregate's own shape, and rounds the exact
/// average, half away from zero.
#[test]
fn a_rounded_total_or_average_answers_the_decimal_mysql_answers() {
    let (_directory, mut adapter) = adapter();
    let decimal = |length, decimals| {
        (
            MYSQL_TYPE_NEWDECIMAL,
            length,
            decimals,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        )
    };
    // Rounding to at least the average's four places keeps its shape;
    // rounding to fewer keeps the whole part and adds a digit for the carry.
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT ROUND(AVG(views), 2), ROUND(AVG(views)), ROUND(AVG(views), 6), ROUND(AVG(views), 4), ROUND(AVG(views), 3), ROUND(SUM(views)), ROUND(SUM(views), 1) FROM posts",
    );
    assert_eq!(
        shapes,
        [
            decimal(15, 2),
            decimal(12, 0),
            decimal(16, 4),
            decimal(16, 4),
            decimal(16, 3),
            decimal(34, 0),
            decimal(33, 0),
        ]
    );
    assert_eq!(
        answered,
        rows(&[&[
            Some("5.00"),
            Some("5"),
            Some("5.0000"),
            Some("5.0000"),
            Some("5.000"),
            Some("25"),
            Some("25"),
        ]])
    );

    let (shapes, answered) = report(
        &mut adapter,
        "SELECT ROUND(AVG(balance), 2), ROUND(AVG(balance)), ROUND(AVG(balance), 8), ROUND(SUM(balance), 1), ROUND(SUM(balance), 3) FROM users",
    );
    assert_eq!(
        shapes,
        [
            decimal(13, 2),
            decimal(10, 0),
            decimal(16, 6),
            decimal(34, 1),
            decimal(34, 2),
        ]
    );
    assert_eq!(
        answered,
        rows(&[&[
            Some("36.83"),
            Some("37"),
            Some("36.830000"),
            Some("110.5"),
            Some("110.49"),
        ]])
    );

    // Over no rows it is NULL, and in a grouped report it keeps its shape.
    let (_, answered) = report(
        &mut adapter,
        "SELECT ROUND(AVG(views), 2) AS a FROM posts WHERE id > 100",
    );
    assert_eq!(answered, rows(&[&[None]]));
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT DATE(created_at), ROUND(AVG(views), 2) FROM posts GROUP BY DATE(created_at) ORDER BY 1",
    );
    assert_eq!(shapes[1], decimal(15, 2));
    assert_eq!(
        answered,
        rows(&[
            &[None, Some("0.00")],
            &[Some("2025-12-31"), Some("5.00")],
            &[Some("2026-01-05"), Some("6.50")],
            &[Some("2026-02-01"), Some("7.00")],
        ])
    );

    adapter
        .execute_query("CREATE TABLE halves (g INT, n INT, d DECIMAL(6,3))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO halves VALUES (1, -2, -0.005), (1, -3, -0.010), (2, 1, 0.001), (2, 1, 0.002), (2, 2, 0.002)")
        .unwrap();
    let (_, answered) = report(
        &mut adapter,
        "SELECT g, ROUND(AVG(n)), ROUND(AVG(n), 3), ROUND(AVG(d), 2), ROUND(SUM(d), 2), ROUND(AVG(d), 5) FROM halves GROUP BY g ORDER BY g",
    );
    assert_eq!(
        answered,
        rows(&[
            &[
                Some("1"),
                Some("-3"),
                Some("-2.500"),
                Some("-0.01"),
                Some("-0.02"),
                Some("-0.00750"),
            ],
            &[
                Some("2"),
                Some("1"),
                Some("1.333"),
                Some("0.00"),
                Some("0.01"),
                Some("0.00167"),
            ],
        ])
    );

    for sql in [
        // Left of the point, which the engine's decimal rounding stops short
        // of, and over a largest value, which answers a shape of its own.
        "SELECT ROUND(AVG(views), -1) FROM posts",
        "SELECT ROUND(MAX(views), 2) FROM posts",
        // 1140: a column beside the aggregate.
        "SELECT id, ROUND(AVG(views), 2) FROM posts",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}

/// `WHERE views > (SELECT AVG(views) FROM posts)` asks for the rows above
/// average. MySQL compares the column against the average as a decimal
/// rounded to four places.
#[test]
fn a_column_compared_against_the_average_finds_the_rows_mysql_finds() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query("CREATE TABLE thirds (n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO thirds VALUES (1), (1), (2)")
        .unwrap();
    for (sql, expected) in [
        (
            "SELECT id FROM posts WHERE views > (SELECT AVG(views) FROM posts) ORDER BY id",
            &[Some("1"), Some("3")][..],
        ),
        (
            "SELECT id FROM posts WHERE views >= (SELECT AVG(views) FROM posts) ORDER BY id",
            &[Some("1"), Some("3"), Some("5")],
        ),
        (
            "SELECT id FROM posts WHERE views = (SELECT AVG(views) FROM posts) ORDER BY id",
            &[Some("5")],
        ),
        (
            "SELECT id FROM posts WHERE views <> (SELECT AVG(views) FROM posts) ORDER BY id",
            &[Some("1"), Some("2"), Some("3"), Some("4")],
        ),
        (
            "SELECT id FROM posts WHERE (SELECT AVG(views) FROM posts) < views ORDER BY id",
            &[Some("1"), Some("3")],
        ),
        (
            "SELECT id FROM posts WHERE views <= (SELECT AVG(views) FROM posts) ORDER BY id",
            &[Some("2"), Some("4"), Some("5")],
        ),
        // An average over no rows is NULL, which no row compares true against.
        (
            "SELECT id FROM posts WHERE views <= (SELECT AVG(views) FROM posts WHERE id > 100) ORDER BY id",
            &[],
        ),
        (
            "SELECT n FROM thirds WHERE n > (SELECT AVG(n) FROM thirds)",
            &[Some("2")],
        ),
    ] {
        let (_, answered) = report(&mut adapter, sql);
        assert_eq!(
            answered,
            expected
                .iter()
                .map(|value| vec![value.map(str::to_owned)])
                .collect::<Rows>(),
            "{sql}"
        );
    }
    let (_, answered) = report(
        &mut adapter,
        "SELECT * FROM posts WHERE views > (SELECT AVG(views) FROM posts) ORDER BY id",
    );
    assert_eq!(answered.len(), 2);

    for sql in [
        // Words compared against a number, which MySQL coerces.
        "SELECT id FROM posts WHERE title > (SELECT AVG(views) FROM posts)",
        // A total, which has not been measured against a column.
        "SELECT id FROM posts WHERE views > (SELECT SUM(views) FROM posts)",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}

/// A largest or smallest of an unsigned column is unsigned too, which a
/// client decoding the binary protocol reads the value by; a total and an
/// average of one widen its digits and answer a signed decimal.
#[test]
fn aggregates_over_unsigned_columns_answer_the_shapes_mysql_answers() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE un (id INT PRIMARY KEY, a INT UNSIGNED, b BIGINT UNSIGNED, t TINYINT UNSIGNED, d DECIMAL(6,2) UNSIGNED)",
        "INSERT INTO un VALUES (1, 4000000000, 18446744073709551615, 250, 1.50), (2, 3, 5, 1, 2.25)",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let numeric = |column_type, length, decimals, unsigned: bool| {
        (
            column_type,
            length,
            decimals,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG | if unsigned { MYSQL_UNSIGNED_FLAG } else { 0 },
        )
    };
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT MAX(a), MIN(a), MAX(b), MIN(b), MAX(t), SUM(a), SUM(b), AVG(a), AVG(b), SUM(t), AVG(t), MAX(d), SUM(d), AVG(d) FROM un",
    );
    assert_eq!(
        shapes,
        [
            numeric(MYSQL_TYPE_LONG, 10, 0, true),
            numeric(MYSQL_TYPE_LONG, 10, 0, true),
            numeric(MYSQL_TYPE_LONGLONG, 20, 0, true),
            numeric(MYSQL_TYPE_LONGLONG, 20, 0, true),
            numeric(MYSQL_TYPE_TINY, 3, 0, true),
            numeric(MYSQL_TYPE_NEWDECIMAL, 33, 0, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 43, 0, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 16, 4, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 26, 4, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 26, 0, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 9, 4, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 7, 2, true),
            numeric(MYSQL_TYPE_NEWDECIMAL, 30, 2, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 12, 6, false),
        ]
    );
    assert_eq!(
        answered,
        rows(&[&[
            Some("4000000000"),
            Some("3"),
            Some("18446744073709551615"),
            Some("5"),
            Some("250"),
            Some("4000000003"),
            Some("18446744073709551620"),
            Some("2000000001.5000"),
            Some("9223372036854775810.0000"),
            Some("251"),
            Some("125.5000"),
            Some("2.25"),
            Some("3.75"),
            Some("1.875000"),
        ]])
    );
    // Rounded, over the users' BIGINT UNSIGNED ids 1 to 4.
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT ROUND(AVG(id), 2), ROUND(SUM(id), 2), ROUND(SUM(id)), ROUND(AVG(id)) FROM users",
    );
    assert_eq!(
        shapes,
        [
            numeric(MYSQL_TYPE_NEWDECIMAL, 25, 2, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 43, 0, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 44, 0, false),
            numeric(MYSQL_TYPE_NEWDECIMAL, 22, 0, false),
        ]
    );
    assert_eq!(
        answered,
        rows(&[&[Some("2.50"), Some("10"), Some("10"), Some("3")]])
    );
}
