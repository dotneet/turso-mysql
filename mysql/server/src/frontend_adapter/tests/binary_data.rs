//! Columns of bytes — `TINYBLOB`, `BLOB`, `MEDIUMBLOB`, `LONGBLOB` and
//! `VARBINARY` — holding what MySQL holds, compared and counted the way MySQL
//! compares and counts them.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([171; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT PRIMARY KEY, b BLOB, tb TINYBLOB, mb MEDIUMBLOB, lb LONGBLOB, vb VARBINARY(4), UNIQUE KEY vb_unique (vb))",
    );
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn result(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<Vec<u8>>>> {
    result(adapter, sql).rows
}

/// The first column of each row, read as text.
fn ids(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    rows(adapter, sql)
        .into_iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect()
}

/// A word and a whole number are stored as their bytes: `12` as the two bytes
/// `1` and `2`. Kept as bytes, they sort by their bytes, the empty value
/// first, where a number kept as a number would sort before every word.
#[test]
fn a_word_or_a_whole_number_is_stored_as_its_bytes() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "INSERT INTO t (id, b, vb) VALUES (1, 'abc', 'abc'), (2, 'ABC', 'ABC'), (3, 12, 12), (4, '', ''), (5, -5, -5)",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT b, vb FROM t WHERE id IN (3, 5) ORDER BY id"
        ),
        vec![
            vec![Some(b"12".to_vec()), Some(b"12".to_vec())],
            vec![Some(b"-5".to_vec()), Some(b"-5".to_vec())],
        ]
    );
    for column in ["b", "vb"] {
        assert_eq!(
            ids(&mut adapter, &format!("SELECT id FROM t ORDER BY {column}")),
            ["4", "5", "3", "2", "1"],
            "{column}"
        );
    }
}

/// Measured: MySQL writes a number with a fraction in its own spelling of
/// it — `0.10` stores `0.10` and `1e3` stores `1000` — which the engine does
/// not work out, so such a number is refused, in an `UPDATE` as well.
#[test]
fn a_number_with_a_fraction_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "INSERT INTO t (id, b) VALUES (1, 1.5)",
        "INSERT INTO t (id, vb) VALUES (1, -1e3)",
        "INSERT INTO t SET id = 1, b = .5",
        "UPDATE t SET b = 0.10",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    assert!(rows(&mut adapter, "SELECT id FROM t").is_empty());
}

/// A value wider than its column answers 1406, counting bytes: 256 bytes in
/// a `TINYBLOB`, five in a `VARBINARY(4)`. Trailing spaces count like any
/// other byte and are kept.
#[test]
fn too_many_bytes_answer_1406_and_spaces_are_kept() {
    let (_directory, mut adapter) = adapter();
    let wide = "a".repeat(256);
    for sql in [
        format!("INSERT INTO t (id, tb) VALUES (1, '{wide}')"),
        "INSERT INTO t (id, vb) VALUES (1, 'abcde')".to_owned(),
    ] {
        assert_eq!(
            adapter.execute_query(&sql),
            Err(FrontendErrorKind::DataTooLong),
            "{sql}"
        );
    }
    run(
        &mut adapter,
        &format!(
            "INSERT INTO t (id, tb, vb) VALUES (1, '{}', 'ab  ')",
            &wide[1..]
        ),
    );
    assert_eq!(
        rows(&mut adapter, "SELECT LENGTH(tb), vb FROM t"),
        vec![vec![Some(b"255".to_vec()), Some(b"ab  ".to_vec())]]
    );
}

/// A word compared with a column of bytes compares by its bytes, with no
/// regard to case and no padding: `b = 'ABC'` finds `ABC` alone, `vb = 'a'`
/// does not find `a `, and the order is the bytes' order. The unique key
/// holds `a`, `A` and `a ` apart and refuses a second `a` with 1062.
#[test]
fn a_word_compares_with_bytes_by_its_bytes() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "INSERT INTO t (id, b, vb) VALUES (1, 'abc', 'a'), (2, 'ABC', 'A'), (3, 'ab', 'a '), (5, '', ''), (7, 'b', 'b')",
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO t (id, vb) VALUES (8, 'a')"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    for (sql, expected) in [
        ("SELECT id FROM t WHERE b = 'ABC'", &["2"][..]),
        ("SELECT id FROM t WHERE b = 'abc'", &["1"]),
        ("SELECT id FROM t WHERE vb = 'a'", &["1"]),
        ("SELECT id FROM t WHERE vb = 'a '", &["3"]),
        ("SELECT id FROM t WHERE b > 'ab' ORDER BY id", &["1", "7"]),
        (
            "SELECT id FROM t WHERE b IN ('abc', 'b') ORDER BY id",
            &["1", "7"],
        ),
        (
            "SELECT id FROM t WHERE b <> 'abc' AND b < 'b' ORDER BY id",
            &["2", "3", "5"],
        ),
        (
            "SELECT id FROM t ORDER BY b, id",
            &["5", "2", "3", "1", "7"],
        ),
        (
            "SELECT id FROM t ORDER BY vb, id",
            &["5", "2", "1", "3", "7"],
        ),
    ] {
        assert_eq!(ids(&mut adapter, sql), expected, "{sql}");
    }
    run(&mut adapter, "UPDATE t SET vb = 'zz' WHERE b = 'abc'");
    run(&mut adapter, "DELETE FROM t WHERE vb = ''");
    assert_eq!(
        ids(&mut adapter, "SELECT id FROM t WHERE vb = 'zz' OR id = 5"),
        ["1"]
    );
}

/// Measured: MySQL compares a column of bytes with a number as two numbers —
/// `b = 0` finds every row whose bytes do not begin with a digit — and
/// matches a `LIKE` over its bytes with regard to case, neither of which the
/// engine does, so both are refused.
#[test]
fn a_number_or_a_like_against_bytes_is_refused() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "INSERT INTO t (id, b) VALUES (1, 'abc')");
    for sql in [
        "SELECT id FROM t WHERE b = 0",
        "SELECT id FROM t WHERE b LIKE 'a%'",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
}

/// A bound word and bound bytes both compare by their bytes; a bound number
/// is refused, for the reason a written one is.
#[test]
fn a_bound_word_or_bytes_compares_with_bytes() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "INSERT INTO t (id, b) VALUES (1, 'abc'), (2, 'ABC')",
    );
    let statement = adapter
        .execute_stmt_prepare("SELECT id FROM t WHERE b = ?")
        .unwrap();
    let found = |adapter: &mut Adapter, type_code: u8, value: &[u8]| {
        let mut payload = vec![0, 1, type_code, 0];
        if type_code == MYSQL_TYPE_LONGLONG {
            payload.extend_from_slice(value);
        } else {
            payload.push(u8::try_from(value.len()).unwrap());
            payload.extend_from_slice(value);
        }
        match adapter.execute_stmt_execute(statement.statement_id, &payload)? {
            PreparedStatementExecutionResult::ResultSet(result) => Ok(result.rows),
            other => panic!("the statement must return rows, answered {other:?}"),
        }
    };
    assert_eq!(
        found(&mut adapter, MYSQL_TYPE_STRING, b"ABC"),
        Ok(vec![vec![BinaryResultValue::Integer(2)]])
    );
    assert_eq!(
        found(&mut adapter, MYSQL_TYPE_BLOB, b"abc"),
        Ok(vec![vec![BinaryResultValue::Integer(1)]])
    );
    assert_eq!(
        found(&mut adapter, MYSQL_TYPE_LONGLONG, &0i64.to_le_bytes()),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// `HEX` over bytes answers two characters for each byte, in a column as
/// wide as MySQL reports, and NULL for NULL; `LENGTH`, `OCTET_LENGTH` and
/// `CHAR_LENGTH` all count bytes.
#[test]
fn hex_and_length_read_the_bytes() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "INSERT INTO t (id, b, vb) VALUES (1, 'a''\\\\', 'é')",
    );
    // The collation a result in words reports is the connection's, which is
    // utf8mb4_general_ci on this server; see COMPAT.md.
    const TEXT: u16 = DEFAULT_UTF8MB4_COLLATION as u16;
    let hexadecimal = result(
        &mut adapter,
        "SELECT HEX(b), HEX(tb), HEX(mb), HEX(lb), HEX(vb) FROM t",
    );
    assert_eq!(
        hexadecimal
            .columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.character_set
            ))
            .collect::<Vec<_>>(),
        [
            (MYSQL_TYPE_MEDIUM_BLOB, 2_097_120, TEXT),
            (MYSQL_TYPE_VAR_STRING, 2040, TEXT),
            (MYSQL_TYPE_LONG_BLOB, 536_870_880, TEXT),
            (MYSQL_TYPE_LONG_BLOB, 4_294_967_295, TEXT),
            (MYSQL_TYPE_VAR_STRING, 32, TEXT),
        ]
    );
    assert_eq!(
        hexadecimal.rows,
        vec![vec![
            Some(b"61275C".to_vec()),
            None,
            None,
            None,
            Some(b"C3A9".to_vec())
        ]]
    );
    let lengths = result(
        &mut adapter,
        "SELECT LENGTH(b), OCTET_LENGTH(vb), CHAR_LENGTH(vb) FROM t",
    );
    assert!(lengths
        .columns
        .iter()
        .all(|column| column.column_type == MYSQL_TYPE_LONGLONG && column.column_length == 10));
    assert_eq!(
        lengths.rows,
        vec![vec![
            Some(b"3".to_vec()),
            Some(b"2".to_vec()),
            Some(b"2".to_vec())
        ]]
    );
}
