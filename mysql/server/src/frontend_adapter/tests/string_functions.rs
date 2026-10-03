//! `SUBSTRING_INDEX`, `CONCAT_WS`, `SUBSTRING` and `SUBSTR`, and the `SHA1`
//! and `SHA2` digests: the values each answers and the columns it reports.
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
            AccountId::from_bytes([142; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE s (id INT PRIMARY KEY, a VARCHAR(20), b VARCHAR(30), c CHAR(8), t TEXT, nn VARCHAR(10) NOT NULL DEFAULT '', n INT, d DECIMAL(10,2), ub VARCHAR(20) COLLATE utf8mb4_bin)",
        "INSERT INTO s VALUES (1, 'www.mysql.com', 'x', 'ab', 'long text', 'q', 7, 1.5, 'A.b'), (2, '', '', '', '', '', NULL, NULL, ''), (3, NULL, 'y', NULL, NULL, 'r', -3, NULL, NULL), (4, 'aXbxcXd', 'Ünïcödé.日本.語', 'é', 'a.b', 's', 0, NULL, 'x'), (5, 'aaaaa', NULL, 'x', 't', 'u', 12, NULL, 'y')",
        "CREATE TABLE items (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100), email VARCHAR(100))",
        "INSERT INTO items (name, email) VALUES ('apple', 'a@x.com'), ('Bänana', 'b@y.org'), (NULL, NULL)",
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

fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<Option<String>>> {
    result(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn expected(rows: &[&[Option<&str>]]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|row| row.iter().map(|value| value.map(str::to_owned)).collect())
        .collect()
}

/// Each answers a VAR_STRING with the not-fixed decimals value and no flags,
/// nullable even over a NOT NULL column, in both protocols.
#[test]
fn string_calls_report_the_column_mysql_reports() {
    let (_directory, mut adapter) = adapter();
    for (sql, length) in [
        // As wide as the column, however it is cut.
        ("SELECT SUBSTRING_INDEX(a, '.', 1) FROM s", 80),
        ("SELECT SUBSTRING_INDEX(a, '.', -2) FROM s", 80),
        ("SELECT SUBSTRING_INDEX(c, '.', 1) FROM s", 32),
        ("SELECT SUBSTRING_INDEX(nn, '.', 1) FROM s", 40),
        // Every part laid end to end, with the separator in each gap; a
        // number counts its own width.
        ("SELECT CONCAT_WS('-', a, b) FROM s", 204),
        ("SELECT CONCAT_WS('', a, b) FROM s", 200),
        ("SELECT CONCAT_WS(', ', a, 'lit', b) FROM s", 228),
        ("SELECT CONCAT_WS('-', a) FROM s", 80),
        ("SELECT CONCAT_WS('-', c, nn) FROM s", 76),
        ("SELECT CONCAT_WS('-', a, n) FROM s", 128),
        // What is left after the place, held to the count.
        ("SELECT SUBSTR(a, 2) FROM s", 76),
        ("SELECT SUBSTRING(a, 2, 3) FROM s", 12),
        ("SELECT SUBSTR(a, -2) FROM s", 8),
        ("SELECT SUBSTR(a, 0) FROM s", 0),
        ("SELECT SUBSTR(a, -30) FROM s", 0),
        ("SELECT SUBSTR(a, 2, -1) FROM s", 0),
        ("SELECT SUBSTRING(a, 19, 5) FROM s", 8),
        ("SELECT SUBSTRING(b FROM 2) FROM s", 116),
        // The digest's hexadecimal characters.
        ("SELECT MD5(a) FROM s", 128),
        ("SELECT SHA1(a) FROM s", 160),
        ("SELECT SHA(a) FROM s", 160),
        ("SELECT SHA2(a, 224) FROM s", 224),
        ("SELECT SHA2(a, 0) FROM s", 256),
        ("SELECT SHA2(a, 256) FROM s", 256),
        ("SELECT SHA2(a, 384) FROM s", 384),
        ("SELECT SHA2(a, 512) FROM s", 512),
        ("SELECT SHA2(t, 256) FROM s", 256),
    ] {
        let text = result(&mut adapter, sql);
        let column = &text.columns[0];
        assert_eq!(
            (
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
                column.table.as_str()
            ),
            (MYSQL_TYPE_VAR_STRING, length, NOT_FIXED_DECIMALS, 0, ""),
            "{sql}"
        );
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(prepared.columns, text.columns, "{sql}");
        adapter.execute_stmt_close(prepared.statement_id);
    }
}

#[test]
fn substring_index_cuts_at_the_delimiter_mysql_cuts_at() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT SUBSTRING_INDEX(a, '.', 1), SUBSTRING_INDEX(a, '.', 2), SUBSTRING_INDEX(a, '.', -1), SUBSTRING_INDEX(a, '.', -2), SUBSTRING_INDEX(a, '.', 0), SUBSTRING_INDEX(a, '.', 10), SUBSTRING_INDEX(a, '.', -10), SUBSTRING_INDEX(a, '', 1) FROM s ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        expected(&[
            &[
                Some("www"),
                Some("www.mysql"),
                Some("com"),
                Some("mysql.com"),
                Some(""),
                Some("www.mysql.com"),
                Some("www.mysql.com"),
                Some(""),
            ],
            &[Some(""); 8],
            &[None; 8],
            &[
                Some("aXbxcXd"),
                Some("aXbxcXd"),
                Some("aXbxcXd"),
                Some("aXbxcXd"),
                Some(""),
                Some("aXbxcXd"),
                Some("aXbxcXd"),
                Some(""),
            ],
            &[
                Some("aaaaa"),
                Some("aaaaa"),
                Some("aaaaa"),
                Some("aaaaa"),
                Some(""),
                Some("aaaaa"),
                Some("aaaaa"),
                Some(""),
            ],
        ])
    );
    // The delimiter is matched by its bytes, so case counts, and copies of it
    // are found from the left without overlapping whichever way the count
    // runs.
    let sql = "SELECT SUBSTRING_INDEX(a, 'x', 1), SUBSTRING_INDEX(a, 'X', 2), SUBSTRING_INDEX(a, 'aa', 1), SUBSTRING_INDEX(a, 'aa', -1), SUBSTRING_INDEX(a, 'aa', -2), SUBSTRING_INDEX(a, 'aa', -3), SUBSTRING_INDEX(b, '.', 2), SUBSTRING_INDEX(b, '.', -1) FROM s WHERE id IN (4, 5) ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        expected(&[
            &[
                Some("aXb"),
                Some("aXbxc"),
                Some("aXbxcXd"),
                Some("aXbxcXd"),
                Some("aXbxcXd"),
                Some("aXbxcXd"),
                Some("Ünïcödé.日本"),
                Some("語"),
            ],
            &[
                Some("aaaaa"),
                Some("aaaaa"),
                Some(""),
                Some("a"),
                Some("aaa"),
                Some("aaaaa"),
                None,
                None,
            ],
        ])
    );
    // In a WHERE, compared the way the column compares, and in an ORDER BY.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id FROM items WHERE SUBSTRING_INDEX(email, '@', -1) = 'X.COM'"
        ),
        [[Some("1".to_owned())]]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id FROM items ORDER BY SUBSTRING_INDEX(email, '@', -1) DESC"
        ),
        [
            [Some("2".to_owned())],
            [Some("1".to_owned())],
            [Some("3".to_owned())]
        ]
    );
}

/// A client that prepares the call binds the delimiter.
#[test]
fn substring_index_takes_a_bound_delimiter() {
    let (_directory, mut adapter) = adapter();
    let statement = adapter
        .execute_stmt_prepare("SELECT SUBSTRING_INDEX(email, ?, -1) FROM items ORDER BY id")
        .unwrap();
    assert_eq!(statement.columns[0].column_length, 400);
    // The null bitmap, the new-parameters flag and one VAR_STRING parameter.
    let payload = [0, 1, MYSQL_TYPE_VAR_STRING, 0, 1, b'@'];
    let Ok(PreparedStatementExecutionResult::ResultSet(result)) =
        adapter.execute_stmt_execute(statement.statement_id, &payload)
    else {
        panic!("the prepared SUBSTRING_INDEX must answer rows");
    };
    assert_eq!(
        result.rows,
        [
            vec![BinaryResultValue::Text("x.com".to_owned())],
            vec![BinaryResultValue::Text("y.org".to_owned())],
            vec![BinaryResultValue::Null],
        ]
    );
}

/// A comma and a space written inside one of `CONCAT`'s words are part of the
/// word, in a projection and in an `UPDATE` alike; they used to be read as
/// the gap between two arguments, and `CONCAT(a, ', ', b)` answered
/// `www.mysql.com || x`.
#[test]
fn concat_keeps_a_comma_written_inside_a_word() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT CONCAT(a, ', ', b), CONCAT('a, b', a) FROM s ORDER BY id"
        ),
        expected(&[
            &[Some("www.mysql.com, x"), Some("a, bwww.mysql.com")],
            &[Some(", "), Some("a, b")],
            &[None, None],
            &[Some("aXbxcXd, Ünïcödé.日本.語"), Some("a, baXbxcXd")],
            &[None, Some("a, baaaaa")],
        ])
    );
    adapter
        .execute_query("UPDATE s SET b = CONCAT(b, ', ', 'z') WHERE id IN (1, 2)")
        .unwrap();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT b FROM s WHERE id IN (1, 2) ORDER BY id"
        ),
        expected(&[&[Some("x, z")], &[Some(", z")]])
    );
}

#[test]
fn concat_ws_skips_a_null_part_and_keeps_an_empty_one() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT CONCAT_WS('-', a, b), CONCAT_WS(', ', a, 'lit', b), CONCAT_WS('-', a), CONCAT_WS('-', a, n) FROM s ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        expected(&[
            &[
                Some("www.mysql.com-x"),
                Some("www.mysql.com, lit, x"),
                Some("www.mysql.com"),
                Some("www.mysql.com-7"),
            ],
            &[Some("-"), Some(", lit, "), Some(""), Some("")],
            // Every part NULL answers an empty word, not NULL.
            &[Some("y"), Some("lit, y"), Some(""), Some("-3")],
            &[
                Some("aXbxcXd-Ünïcödé.日本.語"),
                Some("aXbxcXd, lit, Ünïcödé.日本.語"),
                Some("aXbxcXd"),
                Some("aXbxcXd-0"),
            ],
            &[
                Some("aaaaa"),
                Some("aaaaa, lit"),
                Some("aaaaa"),
                Some("aaaaa-12"),
            ],
        ])
    );
}

/// A written `', '` between two columns is written into the answer as it
/// reads — the rendering once turned it into the engine's `||` — whether the
/// answer is read or written into a column.
#[test]
fn concat_keeps_a_written_comma_and_space() {
    let (_directory, mut adapter) = adapter();
    let sql =
        "SELECT CONCAT(a, ', ', b), CONCAT(b, ', ', ', ') FROM s WHERE id IN (1, 2, 3) ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        expected(&[
            &[Some("www.mysql.com, x"), Some("x, , ")],
            &[Some(", "), Some(", , ")],
            &[None, Some("y, , ")],
        ])
    );
    adapter
        .execute_query("UPDATE s SET b = CONCAT(a, ', ', b) WHERE id = 1")
        .unwrap();
    assert_eq!(
        rows(&mut adapter, "SELECT b FROM s WHERE id = 1"),
        expected(&[&[Some("www.mysql.com, x")]])
    );
}

#[test]
fn substring_answers_nothing_where_mysql_does() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT SUBSTR(a, 2), SUBSTR(a, 2, 3), SUBSTR(a, -2), SUBSTR(a, 0), SUBSTR(a, 0, 3), SUBSTR(a, -30), SUBSTR(a, 1, 0), SUBSTR(b, 2, 3) FROM s WHERE id IN (1, 3, 4) ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        expected(&[
            &[
                Some("ww.mysql.com"),
                Some("ww."),
                Some("om"),
                Some(""),
                Some(""),
                Some(""),
                Some(""),
                Some(""),
            ],
            &[None, None, None, None, None, None, None, Some("")],
            &[
                Some("XbxcXd"),
                Some("Xbx"),
                Some("Xd"),
                Some(""),
                Some(""),
                Some(""),
                Some(""),
                Some("nïc"),
            ],
        ])
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id FROM items WHERE SUBSTR(name, 2) = 'PPLE'"
        ),
        [[Some("1".to_owned())]]
    );
}

#[test]
fn sha1_and_sha2_write_the_digest_mysql_writes() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT SHA1(a), SHA2(a, 224), SHA2(a, 256), SHA2(a, 384), SHA2(a, 512) FROM s WHERE id = 1"
        ),
        [[
            "6d24063944ad733529bca2046b7e06ef5617173a",
            "c84da09eca0a441635de9262d44182ca164a74fecf2044d9d7e7aa17",
            "25f94c7fcd9c66540ec3b1b534706517887e3472df7d8973a0b6b8318ecf49fe",
            "9655f262dd20b6aaca018adbf8a9bcdb6be07c5aab3ba35afa884a174e116658bc31793f63b333d4c6c8910141002dbb",
            "3e0e5743a21a36a92cb6cc50e998251dec62b417dbfdef91ae02826c5feb4f6071a81dca5dcbc6cc73f4ec5c4c88d44fca1a75ed1feb4a262c3878b3bb04641d",
        ]
        .map(|digest| Some(digest.to_owned()))]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT SHA1(a), SHA2(a, 0) FROM s WHERE id IN (2, 3) ORDER BY id"
        ),
        [
            [
                Some("da39a3ee5e6b4b0d3255bfef95601890afd80709".to_owned()),
                Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned())
            ],
            [None, None]
        ]
    );
}

/// Measured on MySQL 8.4.11: a call over a `utf8mb4_bin` column answers that
/// collation, so its answer is compared with a word case by case.
#[test]
fn a_string_call_compares_under_the_collation_of_its_column() {
    let (_directory, mut adapter) = adapter();
    let ids = |adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>, sql| {
        rows(adapter, sql)
            .into_iter()
            .map(|row| row[0].clone().unwrap())
            .collect::<Vec<_>>()
    };
    assert!(ids(
        &mut adapter,
        "SELECT id FROM s WHERE SUBSTRING_INDEX(ub, '.', 1) = 'a'"
    )
    .is_empty());
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM s WHERE SUBSTRING_INDEX(ub, '.', 1) = 'A'"
        ),
        ["1"]
    );
}

#[test]
fn string_calls_refuse_what_mysql_answers_by_another_rule() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // A TEXT answers a MEDIUM_BLOB of 1048560, a shape of its own.
        "SELECT SUBSTRING_INDEX(t, '.', 1) FROM s",
        "SELECT SUBSTR(t, 2) FROM s",
        // A number is written out before it is cut or digested.
        "SELECT SUBSTRING_INDEX(n, '1', 1) FROM s",
        "SELECT SHA1(n) FROM s",
        "SELECT MD5(n) FROM s",
        "SELECT SUBSTRING_INDEX(d, '.', 1) FROM s",
        // MySQL answers NULL for a size SHA2 does not have, with a warning.
        "SELECT SHA2(a, 1) FROM s",
        // A count read from the row leaves no width to answer with.
        "SELECT SUBSTRING_INDEX(a, '.', n) FROM s",
        "SELECT SUBSTR(a, n) FROM s",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
}
