//! `POSITION`, and the calls that read the bytes of a value: `ASCII`, `ORD`,
//! `CRC32`, `QUOTE` and `TO_BASE64`.
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
            AccountId::from_bytes([158; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    let sixty = "b".repeat(60);
    for sql in [
        "CREATE TABLE br (id INT PRIMARY KEY, v VARCHAR(10), vn VARCHAR(10) NOT NULL, w VARCHAR(80), n INT, dt DATETIME, t TEXT, c CHAR(4))".to_owned(),
        format!("INSERT INTO br VALUES (1, 'hello', 'x', '{sixty}', 7, '2026-01-15 10:20:30', 'long', 'ab'), (2, NULL, '', NULL, NULL, NULL, NULL, NULL), (3, 'Ünï', 'a''b\\\\c', 'é', -1, NULL, 'a', 'é')"),
        "CREATE TABLE words (id INT PRIMARY KEY, v VARCHAR(10), vn VARCHAR(10) NOT NULL)".to_owned(),
        "INSERT INTO words VALUES (1, 'hello', 'world'), (2, NULL, '')".to_owned(),
    ] {
        adapter
            .execute_query(&sql)
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

/// Measured: `POSITION(x IN col)` answers what `LOCATE(x, col)` answers,
/// under the column's collation, and an empty word is found at 1.
#[test]
fn position_is_locate_written_with_a_keyword() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT POSITION('l' IN v), POSITION('L' IN vn), POSITION('' IN v), POSITION('o' IN vn) FROM words ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [["3", "4", "1", "2"], ["NULL", "0", "NULL", "0"]]
    );
    let number = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            (
                "POSITION('l' IN v)".to_owned(),
                MYSQL_TYPE_LONGLONG,
                11,
                number
            ),
            (
                "POSITION('L' IN vn)".to_owned(),
                MYSQL_TYPE_LONGLONG,
                11,
                number | MYSQL_NOT_NULL_FLAG
            ),
            (
                "POSITION('' IN v)".to_owned(),
                MYSQL_TYPE_LONGLONG,
                11,
                number
            ),
            (
                "POSITION('o' IN vn)".to_owned(),
                MYSQL_TYPE_LONGLONG,
                11,
                number | MYSQL_NOT_NULL_FLAG
            ),
        ]
    );
}

/// Measured: `ASCII` reads the first byte and `ORD` the bytes of the first
/// character, `Ü` being 195 to one and 50076 to the other, and `CRC32` is
/// the checksum of the value written out, a number included.
#[test]
fn ascii_ord_and_crc32_read_the_bytes() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT ASCII(v), ORD(v), ASCII(vn), ORD(vn), ASCII(t), CRC32(v), CRC32(vn), CRC32(t), CRC32(n) FROM br ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            [
                "104",
                "104",
                "120",
                "120",
                "108",
                "907060870",
                "2363233923",
                "999795048",
                "1790921346"
            ],
            ["NULL", "NULL", "0", "0", "NULL", "NULL", "0", "NULL", "NULL"],
            [
                "195",
                "50076",
                "97",
                "97",
                "97",
                "2079778772",
                "3597861779",
                "3904355907",
                "808273962"
            ],
        ]
    );
    let number = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let unsigned = number | MYSQL_UNSIGNED_FLAG;
    let lengths: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, flags)| (column_type, length, flags))
        .collect();
    assert_eq!(
        lengths,
        [
            (MYSQL_TYPE_LONGLONG, 3, number),
            (MYSQL_TYPE_LONGLONG, 21, number),
            (MYSQL_TYPE_LONGLONG, 3, number | MYSQL_NOT_NULL_FLAG),
            (MYSQL_TYPE_LONGLONG, 21, number | MYSQL_NOT_NULL_FLAG),
            (MYSQL_TYPE_LONGLONG, 3, number),
            (MYSQL_TYPE_LONGLONG, 10, unsigned),
            (MYSQL_TYPE_LONGLONG, 10, unsigned | MYSQL_NOT_NULL_FLAG),
            (MYSQL_TYPE_LONGLONG, 10, unsigned),
            (MYSQL_TYPE_LONGLONG, 10, unsigned),
        ]
    );
    let prepared = adapter
        .execute_stmt_prepare("SELECT CRC32(vn) FROM br WHERE id = 1")
        .unwrap();
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert_eq!(
        binary.rows,
        [[BinaryResultValue::UnsignedInteger(2_363_233_923)]]
    );
}

/// Measured: `QUOTE` escapes a backslash and a quote and answers the word
/// `NULL` for a NULL, and `TO_BASE64` puts a newline after every 76
/// characters; each is as wide as the value it writes out can make it.
#[test]
fn quote_and_to_base64_write_the_value_out() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT QUOTE(v), QUOTE(vn), QUOTE(n), QUOTE(dt), TO_BASE64(v), TO_BASE64(c), TO_BASE64(n), TO_BASE64(dt), TO_BASE64(w) FROM br ORDER BY id";
    let long = format!("{}\n{}", "YmJi".repeat(19), "YmJi");
    assert_eq!(
        rows(&mut adapter, sql),
        [
            [
                "'hello'",
                "'x'",
                "'7'",
                "'2026-01-15 10:20:30'",
                "aGVsbG8=",
                "YWI=",
                "Nw==",
                "MjAyNi0wMS0xNSAxMDoyMDozMA==",
                long.as_str()
            ],
            ["NULL", "''", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL"],
            [
                "'Ünï'",
                r"'a\'b\\c'",
                "'-1'",
                "NULL",
                "w5xuw68=",
                "w6k=",
                "LTE=",
                "NULL",
                "w6k="
            ],
        ]
    );
    let lengths: Vec<_> = shapes(&mut adapter, sql)
        .into_iter()
        .map(|(_, column_type, length, flags)| {
            assert_eq!((column_type, flags), (MYSQL_TYPE_VAR_STRING, 0));
            length
        })
        .collect();
    assert_eq!(lengths, [88, 88, 96, 160, 224, 96, 64, 112, 1732]);
    // Over a TEXT MySQL answers a MEDIUM_BLOB by a width rule not measured
    // here.
    for sql in ["SELECT QUOTE(t) FROM br", "SELECT TO_BASE64(t) FROM br"] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
