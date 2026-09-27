//! Values a projection writes out in full — a number with a point or an
//! exponent, a hexadecimal or bit literal, a cast or a `CONVERT` of a written
//! value — and the columns MySQL reports for them.
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
            AccountId::from_bytes([151; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE posts (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, title VARCHAR(200) NOT NULL, views INT NOT NULL DEFAULT 0)",
        "INSERT INTO posts (title, views) VALUES ('hello', 5)",
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

/// One row of values, with NULL written as `NULL`.
fn row(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    let result = result(adapter, sql);
    let [row] = result.rows.as_slice() else {
        panic!("{sql} must answer one row");
    };
    row.iter()
        .map(|value| match value {
            Some(value) => String::from_utf8_lossy(value).into_owned(),
            None => "NULL".to_owned(),
        })
        .collect()
}

/// The name, type, length, decimals, flags and character set of each column,
/// which the binary protocol has to report the same as the text one.
fn shapes(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<(String, u8, u32, u8, u16, u16)> {
    let text = result(adapter, sql);
    let prepared = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    assert_eq!(prepared.columns, text.columns, "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
    text.columns
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
                column.character_set,
            )
        })
        .collect()
}

fn binary_row(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<BinaryResultValue> {
    let prepared = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"))
    else {
        panic!("{sql} must return a result set");
    };
    adapter.execute_stmt_close(prepared.statement_id);
    let [row] = <[Vec<BinaryResultValue>; 1]>::try_from(result.rows).expect("one row");
    row
}

const DECIMAL: u16 = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
const TEXT: u16 = DEFAULT_UTF8MB4_COLLATION as u16;
const BINARY: u16 = 63;

#[test]
fn a_number_written_with_a_point_is_a_decimal_as_wide_as_its_digits() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT 1.5, .5, 0.10, -1.50, 123456789012345678901234567890.5, +1.5, 100., -.5, 0.0";
    assert_eq!(
        row(&mut adapter, sql),
        [
            "1.5",
            "0.5",
            "0.10",
            "-1.50",
            "123456789012345678901234567890.5",
            "1.5",
            "100",
            "-0.5",
            "0.0"
        ]
    );
    let shape = |name: &str, length, decimals| {
        (
            name.to_owned(),
            MYSQL_TYPE_NEWDECIMAL,
            length,
            decimals,
            DECIMAL,
            BINARY,
        )
    };
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            shape("1.5", 4, 1),
            shape(".5", 3, 1),
            shape("0.10", 5, 2),
            shape("-1.50", 5, 2),
            shape("123456789012345678901234567890.5", 33, 1),
            shape("1.5", 4, 1),
            shape("100.", 4, 0),
            shape("-.5", 3, 1),
            shape("0.0", 4, 1),
        ]
    );
    assert_eq!(
        binary_row(&mut adapter, "SELECT 0.10, -1.50"),
        [
            BinaryResultValue::Text("0.10".to_owned()),
            BinaryResultValue::Text("-1.50".to_owned())
        ]
    );
}

#[test]
fn a_number_written_with_an_exponent_is_a_double_as_wide_as_it_was_written() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT 1e3, 1.5e-3, -2E2, 1e308, +1e3, 1e-400";
    assert_eq!(
        row(&mut adapter, sql),
        ["1000", "0.0015", "-200", "1e308", "1000", "0"]
    );
    let shape = |name: &str, length| {
        (
            name.to_owned(),
            MYSQL_TYPE_DOUBLE,
            length,
            NOT_FIXED_DECIMALS,
            DECIMAL,
            BINARY,
        )
    };
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            shape("1e3", 3),
            shape("1.5e-3", 6),
            shape("-2E2", 23),
            shape("1e308", 5),
            shape("1e3", 3),
            shape("1e-400", 6),
        ]
    );
    assert_eq!(
        binary_row(&mut adapter, "SELECT 1e3"),
        [BinaryResultValue::Real(1000.0)]
    );
}

/// Measured: the hexadecimal spellings carry the unsigned flag and the bit
/// spelling does not.
#[test]
fn a_hexadecimal_or_bit_literal_is_a_binary_string_of_its_bytes() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT 0x41, X'41', b'101', X'', b'', 0x4142, b'0100000101000010', 0x00FF";
    assert_eq!(
        result(&mut adapter, sql).rows,
        [[
            Some(b"A".to_vec()),
            Some(b"A".to_vec()),
            Some(vec![5]),
            Some(Vec::new()),
            Some(Vec::new()),
            Some(b"AB".to_vec()),
            Some(b"AB".to_vec()),
            Some(vec![0, 255]),
        ]]
    );
    let hexadecimal = MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG;
    let bits = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG;
    let shape = |name: &str, length, flags| {
        (
            name.to_owned(),
            MYSQL_TYPE_VAR_STRING,
            length,
            0,
            flags,
            BINARY,
        )
    };
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            shape("0x41", 1, hexadecimal),
            shape("X'41'", 1, hexadecimal),
            shape("b'101'", 1, bits),
            shape("X''", 0, hexadecimal),
            shape("b''", 0, bits),
            shape("0x4142", 2, hexadecimal),
            shape("b'0100000101000010'", 2, bits),
            shape("0x00FF", 2, hexadecimal),
        ]
    );
    assert_eq!(
        binary_row(&mut adapter, "SELECT 0x00FF, b'101'"),
        [
            BinaryResultValue::Blob(vec![0, 255]),
            BinaryResultValue::Blob(vec![5])
        ]
    );
}

#[test]
fn a_written_value_cast_to_decimal_is_rounded_half_away_from_zero() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT CAST(1 AS DECIMAL(10,2)), CAST(1.567 AS DECIMAL(10,2)), CAST(-1.565 AS DECIMAL(10,2)), CAST(NULL AS DECIMAL(10,2)), CAST(1 AS DECIMAL), CAST(1.5 AS DECIMAL(5)), CAST(-0.001 AS DECIMAL(10,2)), CAST(0.005 AS DECIMAL(10,2)), CAST(-0.005 AS DECIMAL(10,2)), CAST(99999999.994 AS DECIMAL(10,2)), CAST(18446744073709551615 AS DECIMAL(30,2)), CAST(0 AS DECIMAL(65,30)), CONVERT(1.5, DECIMAL(10,2))";
    assert_eq!(
        row(&mut adapter, sql),
        [
            "1.00",
            "1.57",
            "-1.57",
            "NULL",
            "1",
            "2",
            "0.00",
            "0.01",
            "-0.01",
            "99999999.99",
            "18446744073709551615.00",
            "0.000000000000000000000000000000",
            "1.50"
        ]
    );
    let lengths: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, decimals, flags, _)| (column_type, length, decimals, flags))
        .collect();
    let decimal = |length, decimals| (MYSQL_TYPE_NEWDECIMAL, length, decimals, DECIMAL);
    assert_eq!(
        lengths,
        [
            decimal(12, 2),
            decimal(12, 2),
            decimal(12, 2),
            (
                MYSQL_TYPE_NEWDECIMAL,
                12,
                2,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            decimal(11, 0),
            decimal(6, 0),
            decimal(12, 2),
            decimal(12, 2),
            decimal(12, 2),
            decimal(12, 2),
            decimal(32, 2),
            decimal(67, 30),
            decimal(12, 2),
        ]
    );
    // MySQL holds a number too wide for the type to the widest one and warns,
    // which is refused rather than answered without the warning.
    for sql in [
        "SELECT CAST(99999999.995 AS DECIMAL(10,2))",
        "SELECT CAST(1.5 AS DECIMAL(3,3))",
        "SELECT CAST(1e3 AS DECIMAL(10,2))",
        "SELECT CAST('1.5' AS DECIMAL(10,2))",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

#[test]
fn a_written_day_moment_or_document_cast_reads_the_way_mysql_stores_it() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT CAST('2026-01-01' AS DATE), CAST('2026-1-1' AS DATE), CAST('20260101' AS DATE), CAST(NULL AS DATE), CAST('2026-01-01 10:20:30' AS DATETIME), CAST('2026-01-01' AS DATETIME), CAST('2026-01-01 10:20:30.5' AS DATETIME), CONVERT('2026-01-01', DATE)";
    assert_eq!(
        row(&mut adapter, sql),
        [
            "2026-01-01",
            "2026-01-01",
            "2026-01-01",
            "NULL",
            "2026-01-01 10:20:30",
            "2026-01-01 00:00:00",
            "2026-01-01 10:20:31",
            "2026-01-01"
        ]
    );
    let shapes_answered = shapes(&mut adapter, sql);
    let names: Vec<_> = shapes_answered
        .iter()
        .map(|(name, ..)| name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "CAST('2026-01-01' AS DATE)",
            "CAST('2026-1-1' AS DATE)",
            "CAST('20260101' AS DATE)",
            "CAST(NULL AS DATE)",
            "CAST('2026-01-01 10:20:30' AS DATETIME)",
            "CAST('2026-01-01' AS DATETIME)",
            "CAST('2026-01-01 10:20:30.5' AS DATETIME)",
            "CONVERT('2026-01-01', DATE)"
        ]
    );
    let kinds: Vec<_> = shapes_answered
        .into_iter()
        .map(|(_, column_type, length, _, flags, character_set)| {
            (column_type, length, flags, character_set)
        })
        .collect();
    let day = (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG, BINARY);
    let moment = (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG, BINARY);
    assert_eq!(kinds, [day, day, day, day, moment, moment, moment, day]);
    assert_eq!(
        binary_row(
            &mut adapter,
            "SELECT CAST('2026-01-01 10:20:30' AS DATETIME)"
        ),
        [BinaryResultValue::DateTime {
            year: 2026,
            month: 1,
            day: 1,
            hour: 10,
            minute: 20,
            second: 30
        }]
    );

    // A document is written back the way MySQL stores one: keys sorted, the
    // last of a repeated key kept, and numbers printed from what they read as.
    let sql = r#"SELECT CAST('{}' AS JSON), CAST('{"b":1,"a":2,"a":3}' AS JSON), CAST('[1, "x", null, true, 1.50, 1e2]' AS JSON), CAST(NULL AS JSON), CONVERT('{}', JSON)"#;
    assert_eq!(
        row(&mut adapter, sql),
        [
            "{}",
            r#"{"a": 3, "b": 1}"#,
            r#"[1, "x", null, true, 1.5, 100.0]"#,
            "NULL",
            "{}"
        ]
    );
    for (_, column_type, length, decimals, flags, character_set) in shapes(&mut adapter, sql) {
        assert_eq!(
            (column_type, length, decimals, flags, character_set),
            (
                MYSQL_TYPE_JSON,
                u32::MAX - 3,
                NOT_FIXED_DECIMALS,
                MYSQL_BINARY_FLAG,
                TEXT
            )
        );
    }

    // A word naming no day answers NULL and warns in MySQL, and a word that is
    // no document is refused with 3141; neither is answered here.
    for sql in [
        "SELECT CAST('nope' AS DATE)",
        "SELECT CAST('2026-02-30' AS DATE)",
        "SELECT CAST('0000-00-00' AS DATE)",
        "SELECT CAST('{bad' AS JSON)",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

#[test]
fn a_written_word_converted_to_utf8mb4_is_as_wide_as_its_characters() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT CONVERT('abc' USING utf8mb4), CONVERT('日本' USING utf8mb4), CONVERT(NULL USING utf8mb4), CAST('abc' AS CHAR)";
    assert_eq!(row(&mut adapter, sql), ["abc", "日本", "NULL", "abc"]);
    let lengths: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, decimals, flags, character_set)| {
            assert_eq!(
                (column_type, decimals, flags, character_set),
                (MYSQL_TYPE_VAR_STRING, NOT_FIXED_DECIMALS, 0, TEXT)
            );
            length
        })
        .collect();
    assert_eq!(lengths, [12, 8, 0, 12]);
    // Over a column it answers what `CAST(col AS CHAR)` answers: the column
    // written out, four bytes to each character it can spell.
    let sql = "SELECT CONVERT(title USING utf8mb4), CONVERT(views USING utf8mb4) FROM posts";
    assert_eq!(row(&mut adapter, sql), ["hello", "5"]);
    let shapes = shapes(&mut adapter, sql);
    assert_eq!(
        shapes,
        [
            (
                "CONVERT(title USING utf8mb4)".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                800,
                NOT_FIXED_DECIMALS,
                0,
                TEXT
            ),
            (
                "CONVERT(views USING utf8mb4)".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                44,
                NOT_FIXED_DECIMALS,
                0,
                TEXT
            ),
        ]
    );
    // Another character set would change the collation the answer carries.
    assert!(adapter
        .execute_query("SELECT CONVERT('abc' USING latin1)")
        .is_err());
}

#[test]
fn a_written_value_stands_beside_columns_under_its_alias() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT 1.5 AS x, 0x41 AS y, id FROM posts";
    assert_eq!(row(&mut adapter, sql), ["1.5", "A", "1"]);
    let names: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    assert_eq!(names, ["x", "y", "id"]);
}

/// Only the statement's own result reports the shape a written value answers,
/// so the value is worked out nowhere else. A written number beside a column
/// in a condition is read the way it always was.
#[test]
fn a_written_value_is_refused_where_its_shape_would_not_be_reported() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT 1.5 UNION SELECT 2.5",
        "SELECT x FROM (SELECT 1.5 AS x) d",
        // Measured: MySQL counts leading zeroes by a rule of its own, `007.5`
        // three digits wide and `000.5` two.
        "SELECT 007.5",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert_eq!(
        row(&mut adapter, "SELECT id FROM posts WHERE views > 1.5"),
        ["1"]
    );
}
