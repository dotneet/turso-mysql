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

/// Bytes written out — `X'..'`, `0x..`, `b'..'`, and a word or hexadecimal
/// literal after `_binary` — are stored as those bytes, in an `INSERT` with
/// or without its columns named, an `INSERT ... SET` and an `UPDATE`, and
/// found by a comparison written the same ways. A word after `_binary` is the
/// bytes it is written in, its escapes worked out.
#[test]
fn bytes_written_out_are_stored_and_found() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "INSERT INTO t (id, b, vb) VALUES (1, X'FF00', 0x0001), (2, b'0100000101000010', _binary 'ab')",
        "INSERT INTO t VALUES (3, _binary X'', NULL, NULL, NULL, X'')",
        "INSERT INTO t (id, b) VALUES (4, _binary '\\0\\n\\Z\\\\\\'é')",
        "INSERT INTO t SET id = 5, vb = _binary 'x'",
        "UPDATE t SET b = _binary'zz', vb = 0xFFFF WHERE id = 5",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, HEX(b), HEX(vb) FROM t ORDER BY id"
        ),
        [
            ["1", "FF00", "0001"],
            ["2", "4142", "6162"],
            ["3", "", ""],
            ["4", "000A1A5C27C3A9", ""],
            ["5", "7A7A", "FFFF"],
        ]
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(at, value)| {
                    (at == 0 || !value.is_empty() || row[0] == "3")
                        .then(|| value.as_bytes().to_vec())
                })
                .collect::<Vec<_>>()
        })
    );
    for (sql, expected) in [
        ("SELECT id FROM t WHERE b = X'FF00'", &["1"][..]),
        ("SELECT id FROM t WHERE vb = _binary'ab'", &["2"]),
        (
            "SELECT id FROM t WHERE vb IN (0x0001, X'') ORDER BY id",
            &["1", "3"],
        ),
        ("SELECT id FROM t WHERE b = b'0100000101000010'", &["2"]),
    ] {
        assert_eq!(ids(&mut adapter, sql), expected, "{sql}");
    }
    // sqlparser writes a word out again with its backslashes as they stand,
    // so an `INSERT ... SET`, which is written out again as the column-list
    // form, would read `'\\\''` back as another word.
    assert_eq!(
        adapter.execute_query("INSERT INTO t SET id = 6, b = _binary '\\\\\\''"),
        Err(FrontendErrorKind::Unsupported)
    );
    run(&mut adapter, "DELETE FROM t WHERE vb = 0xFFFF");
    assert_eq!(ids(&mut adapter, "SELECT COUNT(*) FROM t"), ["4"]);
}

/// Measured on MySQL 8.4.11: `X'41'` is the number 65 in an `INT` and the
/// word `A` in a `VARCHAR`, and `_binary X'41'` is a word even in an `INT`,
/// answering 1366 there — rules not followed here, so bytes written out are
/// taken by a column of bytes alone. A hexadecimal literal with an odd count
/// of digits is refused too: MySQL answers 1064 for `X'ABC'` and reads `0xABC`
/// as `0x0ABC`, which reach this spelled alike.
#[test]
fn bytes_written_out_meet_a_column_of_bytes_alone() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE w (id INT PRIMARY KEY, word VARCHAR(10), n INT)",
    );
    for sql in [
        "INSERT INTO w (id, word) VALUES (1, X'41')",
        "INSERT INTO w VALUES (1, 'a', 2), (2, 'b', 0x01)",
        "INSERT INTO w (id, n) VALUES (1, _binary X'41')",
        "UPDATE w SET word = b'01000001'",
        "SELECT id FROM w WHERE word = X'41'",
        "INSERT INTO t (id, b) VALUES (1, X'ABC')",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    assert!(rows(&mut adapter, "SELECT id FROM w").is_empty());
}

/// A `BINARY(n)` holds exactly `n` bytes and reports itself the way MySQL
/// does: a `STRING` as long as its width, in the binary collation, with the
/// binary flag — a key over one with the key's flags besides. MySQL fills a
/// shorter value out with zero bytes, `'ab'` in a `BINARY(4)` reading back
/// `ab\0\0`; that is refused here rather than stored otherwise, and a longer
/// one answers 1406 as MySQL does.
#[test]
fn a_binary_column_holds_exactly_its_width() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE u (id BINARY(16) NOT NULL PRIMARY KEY, bn BINARY(4), b1 BINARY, label VARCHAR(20))",
    );
    run(
        &mut adapter,
        "INSERT INTO u VALUES (X'000102030405060708090A0B0C0D0E0F', 'abcd', 'x', 'a'), (0xFF0102030405060708090A0B0C0D0E0F, 1234, X'00', 'b')",
    );
    for (sql, refused) in [
        (
            "INSERT INTO u (id, bn) VALUES (X'EE0102030405060708090A0B0C0D0E0F', 'ab')",
            FrontendErrorKind::Unsupported,
        ),
        (
            "INSERT INTO u (id) VALUES (X'00')",
            FrontendErrorKind::Unsupported,
        ),
        (
            "INSERT INTO u (id, bn) VALUES (X'EE0102030405060708090A0B0C0D0E0F', 'abcde')",
            FrontendErrorKind::DataTooLong,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(refused), "{sql}");
    }
    let read = result(
        &mut adapter,
        "SELECT id, bn, b1, HEX(bn) FROM u ORDER BY id",
    );
    assert_eq!(
        read.columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.flags,
                column.character_set
            ))
            .collect::<Vec<_>>(),
        [
            (MYSQL_TYPE_STRING, 16, 0x5083, 63),
            (MYSQL_TYPE_STRING, 4, 0x80, 63),
            (MYSQL_TYPE_STRING, 1, 0x80, 63),
            (
                MYSQL_TYPE_VAR_STRING,
                32,
                0,
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            ),
        ]
    );
    assert_eq!(
        read.rows,
        vec![
            vec![
                Some((0..16).collect()),
                Some(b"abcd".to_vec()),
                Some(b"x".to_vec()),
                Some(b"61626364".to_vec()),
            ],
            vec![
                Some([&[0xFF][..], &(1..16).collect::<Vec<u8>>()].concat()),
                Some(b"1234".to_vec()),
                Some(vec![0]),
                Some(b"31323334".to_vec()),
            ],
        ]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT label FROM u WHERE id = X'FF0102030405060708090A0B0C0D0E0F'"
        ),
        ["b"]
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO u (id) VALUES (X'000102030405060708090A0B0C0D0E0F')"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
}

/// Measured on MySQL 8.4.11: a column of bytes prints as it was declared —
/// a bare `BINARY` as `binary(1)` — its collation is NULL, and its default is
/// reported in hexadecimal, `DEFAULT 'J'` as `0x4A`, an empty one as nothing,
/// while `SHOW CREATE TABLE` prints the word. A `BINARY` default narrower than
/// its column, which MySQL fills out and prints filled out, is refused.
#[test]
fn a_column_of_bytes_describes_itself_the_way_mysql_does() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE v (a VARBINARY(8) DEFAULT 'J', b BINARY(2) DEFAULT 'Jk', c VARBINARY(8) DEFAULT 'a''b', e VARBINARY(2) DEFAULT '', f BINARY)",
    );
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
            .collect::<Vec<_>>()
    };
    let described = rows(&mut adapter, "SHOW FULL COLUMNS FROM v")
        .into_iter()
        .map(|row| text(row)[..6].to_vec())
        .collect::<Vec<_>>();
    let expected = [
        ["a", "varbinary(8)", "", "YES", "", "0x4A"],
        ["b", "binary(2)", "", "YES", "", "0x4A6B"],
        ["c", "varbinary(8)", "", "YES", "", "0x612762"],
        ["e", "varbinary(2)", "", "YES", "", ""],
        ["f", "binary(1)", "", "YES", "", ""],
    ]
    .map(|row| {
        row.iter()
            .enumerate()
            .map(|(at, value)| (at != 2 && !(at == 5 && row[0] == "f")).then(|| value.to_string()))
            .collect::<Vec<_>>()
    });
    assert_eq!(described, expected);
    assert_eq!(
        text(rows(&mut adapter, "SHOW CREATE TABLE v").remove(0))[1].as_deref(),
        Some(concat!(
            "CREATE TABLE `v` (\n",
            "  `a` varbinary(8) DEFAULT 'J',\n",
            "  `b` binary(2) DEFAULT 'Jk',\n",
            "  `c` varbinary(8) DEFAULT 'a''b',\n",
            "  `e` varbinary(2) DEFAULT '',\n",
            "  `f` binary(1) DEFAULT NULL\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        ))
    );
    assert_eq!(
        adapter.execute_query("CREATE TABLE narrow (bn BINARY(3) DEFAULT 'y')"),
        Err(FrontendErrorKind::Unsupported)
    );
}
