//! Text calls over a `TEXT`, over a number written out, and the NOT NULL a
//! count carries over a NOT NULL column.
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
            AccountId::from_bytes([154; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE pads (id BIGINT UNSIGNED PRIMARY KEY, n INT, dt DATETIME, t TEXT NOT NULL, v VARCHAR(10) NOT NULL, mt MEDIUMTEXT, views INT NOT NULL, d DECIMAL(10,2), r DOUBLE)",
        "INSERT INTO pads VALUES (7, -5, '2026-01-15 10:20:30', 'long text', 'abc', 'm', 5, 1.50, 1.5), (18446744073709551615, NULL, NULL, '', '', NULL, 0, NULL, NULL)",
        "CREATE TABLE owners (id INT NOT NULL PRIMARY KEY, pad BIGINT UNSIGNED)",
        "INSERT INTO owners VALUES (1, 7), (2, NULL)",
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

/// The type, length and flags of each column, which the binary protocol has
/// to report the same as the text one.
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

/// Measured: beside a `TEXT` every part counts four times over — the `TEXT`'s
/// own 262140 bytes and each word and column beside it — and the answer is a
/// MEDIUM_BLOB, nullable even over NOT NULL columns.
#[test]
fn a_text_call_over_a_text_answers_a_medium_blob() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT CONCAT(t, v), CONCAT_WS('/', v, t), CONCAT_WS('-', t), LOWER(t), UPPER(t), REPLACE(t, 'o', '0'), TRIM(t), REVERSE(t), CONCAT(t), CONCAT('x', t), CONCAT(t, n), CONCAT(t, dt), LEFT(t, 3) FROM pads ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            [
                "long textabc",
                "abc/long text",
                "long text",
                "long text",
                "LONG TEXT",
                "l0ng text",
                "long text",
                "txet gnol",
                "long text",
                "xlong text",
                "long text-5",
                "long text2026-01-15 10:20:30",
                "lon"
            ],
            ["", "/", "", "", "", "", "", "", "", "x", "NULL", "NULL", ""],
        ]
    );
    let blob = |length| (MYSQL_TYPE_MEDIUM_BLOB, length, 0);
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            blob(1_048_720),
            blob(1_048_736),
            blob(1_048_560),
            blob(1_048_560),
            blob(1_048_560),
            blob(1_048_560),
            blob(1_048_560),
            blob(1_048_560),
            blob(1_048_560),
            blob(1_048_576),
            blob(1_048_736),
            blob(1_048_864),
            (MYSQL_TYPE_VAR_STRING, 12, 0),
        ]
    );
    let prepared = adapter
        .execute_stmt_prepare("SELECT CONCAT(t, v) FROM pads ORDER BY id")
        .unwrap();
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert_eq!(
        binary.rows[0],
        [BinaryResultValue::Blob(b"long textabc".to_vec())]
    );
    // Over a MEDIUMTEXT the answer is a LONG_BLOB, which is not taken.
    for sql in [
        "SELECT LOWER(mt) FROM pads",
        "SELECT CONCAT(mt, 'a') FROM pads",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Measured: a number or a moment is written out first and then padded or
/// cut, `LPAD(id, 5, '0')` answering `00001`, and the answer is as wide as the
/// count asked for.
#[test]
fn a_number_or_a_moment_is_written_out_before_it_is_padded_or_cut() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT LPAD(n, 4, '0'), LPAD(id, 22, '0'), RPAD(dt, 21, '*'), LEFT(dt, 10), RIGHT(id, 3), CONCAT(id, '-', n) FROM pads ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            [
                "00-5",
                "0000000000000000000007",
                "2026-01-15 10:20:30**",
                "2026-01-15",
                "7",
                "7--5"
            ],
            [
                "NULL",
                "0018446744073709551615",
                "NULL",
                "NULL",
                "615",
                "NULL"
            ],
        ]
    );
    let text = |length| (MYSQL_TYPE_VAR_STRING, length, 0);
    assert_eq!(
        shapes(&mut adapter, sql),
        [text(16), text(88), text(84), text(40), text(12), text(128)]
    );
    // A `DECIMAL` with places and a `DOUBLE` are spelled by rules of their
    // own, and nothing to pad with answers an empty word where padding is
    // needed — `LPAD('hi', 5, '')` is `''` — where the engine pads nothing.
    for sql in [
        "SELECT LPAD(d, 6, '0') FROM pads",
        "SELECT LPAD(r, 6, '0') FROM pads",
        "SELECT LEFT(r, 2) FROM pads",
        "SELECT LPAD(v, 6, '') FROM pads",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Measured: a count or a place read out of a NOT NULL column cannot be null
/// either.
#[test]
fn a_count_over_a_not_null_column_reports_not_null() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT LENGTH(t), OCTET_LENGTH(t), CHAR_LENGTH(v), INSTR(v, 'b'), LOCATE('b', v), LOCATE('b', v, 2), TRUNCATE(views, 0) FROM pads ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["9", "9", "3", "2", "2", "2", "5"],
            ["0", "0", "0", "0", "0", "0", "0"],
        ]
    );
    let not_null = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let nullable = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            (MYSQL_TYPE_LONGLONG, 10, not_null),
            (MYSQL_TYPE_LONGLONG, 10, not_null),
            (MYSQL_TYPE_LONGLONG, 10, not_null),
            (MYSQL_TYPE_LONGLONG, 11, not_null),
            (MYSQL_TYPE_LONGLONG, 11, not_null),
            (MYSQL_TYPE_LONGLONG, 11, not_null),
            (MYSQL_TYPE_LONGLONG, 21, not_null),
        ]
    );
    assert_eq!(
        shapes(&mut adapter, "SELECT LENGTH(mt) FROM pads"),
        [(MYSQL_TYPE_LONGLONG, 10, nullable)]
    );
}
