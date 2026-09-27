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

/// Measured: a word written in quotes is named after the word as it reads —
/// its escapes worked out and without its quotes — and is a `VAR_STRING`
/// four bytes to each character, never null, in both protocols.
#[test]
fn a_written_word_is_named_after_itself_and_as_wide_as_its_characters() {
    let (_directory, mut adapter) = adapter();
    let sql = r#"SELECT 'abc', "abc", '', 'it''s', 'ab\'c', 'a\nb', 'é', 'éé😀', '\%a', 'a' AS x, _utf8mb4'abc', id FROM posts"#;
    assert_eq!(
        row(&mut adapter, sql),
        ["abc", "abc", "", "it's", "ab'c", "a\nb", "é", "éé😀", "\\%a", "a", "abc", "1"]
    );
    let shapes = shapes(&mut adapter, sql);
    let words: Vec<_> = shapes[..11]
        .iter()
        .map(
            |(name, column_type, length, decimals, flags, character_set)| {
                assert_eq!(
                    (*column_type, *decimals, *flags, *character_set),
                    (
                        MYSQL_TYPE_VAR_STRING,
                        NOT_FIXED_DECIMALS,
                        MYSQL_NOT_NULL_FLAG,
                        TEXT
                    ),
                    "{name}"
                );
                (name.as_str(), *length)
            },
        )
        .collect();
    assert_eq!(
        words,
        [
            ("abc", 12),
            ("abc", 12),
            ("", 0),
            ("it's", 16),
            ("ab'c", 16),
            ("a\nb", 12),
            ("é", 4),
            // MySQL keeps a name in utf8mb3, which has no room for 😀.
            ("éé?", 12),
            ("\\%a", 12),
            ("x", 4),
            ("abc", 12),
        ]
    );
    assert_eq!(shapes[11].0, "id");
    assert_eq!(
        binary_row(&mut adapter, "SELECT 'abc'"),
        [BinaryResultValue::Text("abc".to_owned())]
    );
}

/// Measured: the spaces and control characters before a word are left out of
/// its name, which stops at a NUL and at 255 bytes, never splitting a
/// character, while the column still counts every character.
#[test]
fn a_written_word_s_name_is_cut_where_mysql_cuts_it() {
    let (_directory, mut adapter) = adapter();
    let sql = r"SELECT '   lead', '\t', 'a\0b', 'trail   ', '\rab'";
    let names: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(name, _, length, ..)| (name, length))
        .collect();
    assert_eq!(
        names,
        [
            ("lead".to_owned(), 28),
            (String::new(), 4),
            ("a".to_owned(), 12),
            ("trail   ".to_owned(), 32),
            ("ab".to_owned(), 12),
        ]
    );
    for (word, name, length) in [
        ("c".repeat(300), "c".repeat(255), 1200),
        (format!("{}éz", "c".repeat(254)), "c".repeat(254), 1024),
        (
            format!("{}😀z", "c".repeat(253)),
            format!("{}?z", "c".repeat(253)),
            1020,
        ),
    ] {
        let sql = format!("SELECT '{word}'");
        let result = result(&mut adapter, &sql);
        assert_eq!(result.columns[0].name, name);
        assert_eq!(result.columns[0].column_length, length);
        assert_eq!(result.rows[0][0].as_deref(), Some(word.as_bytes()));
    }
}

/// MySQL joins words written one after another into one value and names the
/// column after the first alone, which sqlparser does not keep apart.
#[test]
fn words_written_one_after_another_are_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in ["SELECT 'a' 'b'", "SELECT 'a' \"b\"", "SELECT 'a''' 'b'"] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Measured: a signed whole number is named as written, a minus sign kept and
/// a plus sign left out, where the engine names it `(-1)`.
#[test]
fn a_signed_whole_number_is_named_as_written() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT -1, - 1, +1, -9223372036854775808";
    assert_eq!(
        row(&mut adapter, sql),
        ["-1", "-1", "1", "-9223372036854775808"]
    );
    let names: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(name, column_type, length, ..)| (name, column_type, length))
        .collect();
    assert_eq!(
        names,
        [
            ("-1".to_owned(), MYSQL_TYPE_LONGLONG, 2),
            ("- 1".to_owned(), MYSQL_TYPE_LONGLONG, 2),
            ("1".to_owned(), MYSQL_TYPE_LONGLONG, 2),
            ("-9223372036854775808".to_owned(), MYSQL_TYPE_LONGLONG, 20),
        ]
    );
}

/// Measured: a statement grouping by a call stores its groups in a table of
/// their own, and a written word or whole number beside them is not stored
/// there and keeps the shape it has on its own.
#[test]
fn a_written_word_or_number_keeps_its_shape_beside_a_grouping_by_a_call() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE visits (id INT PRIMARY KEY, at DATETIME NULL)",
        "INSERT INTO visits VALUES (1, '2026-01-02 03:04:05'), (2, '2026-01-02 05:00:00'), (3, '2026-01-03 00:00:00')",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let sql = "SELECT 'daily' AS kind, 'é', 7, -1, DATE(at) AS d, COUNT(*) FROM visits GROUP BY DATE(at) ORDER BY d";
    let grouped = result(&mut adapter, sql);
    assert_eq!(grouped.rows.len(), 2);
    let shapes: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .take(4)
        .map(|(name, column_type, length, decimals, flags, _)| {
            (name, column_type, length, decimals, flags)
        })
        .collect();
    assert_eq!(
        shapes,
        [
            (
                "kind".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                20,
                NOT_FIXED_DECIMALS,
                MYSQL_NOT_NULL_FLAG
            ),
            (
                "é".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                4,
                NOT_FIXED_DECIMALS,
                MYSQL_NOT_NULL_FLAG
            ),
            (
                "7".to_owned(),
                MYSQL_TYPE_LONGLONG,
                2,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
            (
                "-1".to_owned(),
                MYSQL_TYPE_LONGLONG,
                2,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
        ]
    );
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

/// Measured: each call over written values alone answers what MySQL works out
/// for it, named after the call as written.
#[test]
fn a_call_over_written_values_alone_is_worked_out_in_full() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT HEX(255), BIN(5), OCT(8), HEX(-1), HEX('abc'), HEX('日'), BIN(-1), OCT(-8)";
    assert_eq!(
        row(&mut adapter, sql),
        [
            "FF",
            "101",
            "10",
            "FFFFFFFFFFFFFFFF",
            "616263",
            "E697A5",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "1777777777777777777770"
        ]
    );
    let text = |name: &str, length| {
        (
            name.to_owned(),
            MYSQL_TYPE_VAR_STRING,
            length,
            NOT_FIXED_DECIMALS,
            0,
            TEXT,
        )
    };
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            text("HEX(255)", 64),
            text("BIN(5)", 260),
            text("OCT(8)", 260),
            text("HEX(-1)", 64),
            text("HEX('abc')", 96),
            text("HEX('日')", 32),
            text("BIN(-1)", 260),
            text("OCT(-8)", 260),
        ]
    );

    let sql = "SELECT CHAR(65), CHAR(65, 66), CHAR(256)";
    assert_eq!(
        result(&mut adapter, sql).rows,
        [[Some(b"A".to_vec()), Some(b"AB".to_vec()), Some(vec![1, 0])]]
    );
    let lengths: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, decimals, flags, character_set)| {
            assert_eq!(
                (column_type, decimals, flags, character_set),
                (
                    MYSQL_TYPE_VAR_STRING,
                    NOT_FIXED_DECIMALS,
                    MYSQL_BINARY_FLAG,
                    BINARY
                )
            );
            length
        })
        .collect();
    assert_eq!(lengths, [4, 8, 4]);
    assert_eq!(
        binary_row(&mut adapter, "SELECT CHAR(256)"),
        [BinaryResultValue::Blob(vec![1, 0])]
    );

    // A word is found among the ones after it without regard to case.
    let sql = "SELECT ASCII('a'), ORD('é'), ASCII(''), FIELD('B', 'a', 'b'), FIELD('x', 'a'), ELT(2, 'a', 'bb'), ELT(3, 'a', 'b')";
    assert_eq!(
        row(&mut adapter, sql),
        ["97", "50089", "0", "2", "0", "bb", "NULL"]
    );
    let whole = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let kinds: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, _, flags, _)| (column_type, length, flags))
        .collect();
    assert_eq!(
        kinds,
        [
            (MYSQL_TYPE_LONGLONG, 3, whole),
            (MYSQL_TYPE_LONGLONG, 21, whole),
            (MYSQL_TYPE_LONGLONG, 3, whole),
            (MYSQL_TYPE_LONGLONG, 3, whole),
            (MYSQL_TYPE_LONGLONG, 3, whole),
            (MYSQL_TYPE_VAR_STRING, 8, 0),
            (MYSQL_TYPE_VAR_STRING, 4, 0),
        ]
    );

    // The places asked for held to the ones written, and a zero written
    // before the point counting as a digit where an empty whole part does not.
    let sql = "SELECT TRUNCATE(1.567, 2), TRUNCATE(1.567, 0), TRUNCATE(-1.567, 1), TRUNCATE(1.5, 3), TRUNCATE(0.567, 2), TRUNCATE(.567, 2), TRUNCATE(-0.001, 2), TRUNCATE(1567, -2), TRUNCATE(-1567, -2), TRUNCATE(1567, -20)";
    assert_eq!(
        row(&mut adapter, sql),
        ["1.56", "1", "-1.5", "1.5", "0.56", "0.56", "0.00", "1500", "-1500", "0"]
    );
    let kinds: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, decimals, flags, _)| {
            assert_eq!(flags, DECIMAL);
            (column_type, length, decimals)
        })
        .collect();
    assert_eq!(
        kinds,
        [
            (MYSQL_TYPE_NEWDECIMAL, 5, 2),
            (MYSQL_TYPE_NEWDECIMAL, 2, 0),
            (MYSQL_TYPE_NEWDECIMAL, 4, 1),
            (MYSQL_TYPE_NEWDECIMAL, 4, 1),
            (MYSQL_TYPE_NEWDECIMAL, 5, 2),
            (MYSQL_TYPE_NEWDECIMAL, 4, 2),
            (MYSQL_TYPE_NEWDECIMAL, 5, 2),
            (MYSQL_TYPE_LONGLONG, 21, 0),
            (MYSQL_TYPE_LONGLONG, 21, 0),
            (MYSQL_TYPE_LONGLONG, 21, 0),
        ]
    );

    // A two-digit year is read in this century under seventy and in the last
    // from there, the way MySQL reads one.
    let sql = "SELECT MAKEDATE(2026, 32), MAKEDATE(2024, 366), MAKEDATE(69, 1), MAKEDATE(70, 1), FROM_DAYS(739000), FROM_DAYS(366), MAKETIME(-1, 2, 3), MAKETIME(838, 59, 59), PERIOD_DIFF(202603, 202512), PERIOD_DIFF(7001, 6912), GET_FORMAT(DATE, 'usa'), GET_FORMAT(TIME, 'USA')";
    assert_eq!(
        row(&mut adapter, sql),
        [
            "2026-02-01",
            "2024-12-31",
            "2069-01-01",
            "1970-01-01",
            "2023-04-25",
            "0001-01-01",
            "-01:02:03",
            "838:59:59",
            "3",
            "-1199",
            "%m.%d.%Y",
            "%h:%i:%s %p"
        ]
    );
    let kinds: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, _, flags, _)| (column_type, length, flags))
        .collect();
    let day = (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG);
    let counted_day = (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG);
    let time = (MYSQL_TYPE_TIME, 10, MYSQL_BINARY_FLAG);
    let months = (MYSQL_TYPE_LONGLONG, 21, whole);
    let format = (MYSQL_TYPE_VAR_STRING, 68, 0);
    assert_eq!(
        kinds,
        [
            day,
            day,
            day,
            day,
            counted_day,
            counted_day,
            time,
            time,
            months,
            months,
            format,
            format
        ]
    );
    assert_eq!(
        binary_row(&mut adapter, "SELECT MAKETIME(-1, 2, 3)"),
        [BinaryResultValue::Time {
            negative: true,
            days: 0,
            hour: 1,
            minute: 2,
            second: 3
        }]
    );

    // A word outside ASCII compares by weights of the collation's own, and
    // `CONV` reads digits by rules not followed here. A value MySQL answers
    // NULL or the zero day for, or refuses with 1210, is refused.
    for sql in [
        "SELECT FIELD('é', 'e')",
        "SELECT CONV(255, 10, 16)",
        "SELECT MAKEDATE(2026, 0)",
        "SELECT MAKEDATE(9999, 366)",
        "SELECT FROM_DAYS(365)",
        "SELECT MAKETIME(12, 60, 0)",
        "SELECT PERIOD_DIFF(0, 0)",
        "SELECT GET_FORMAT(DATE, 'nope')",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
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
