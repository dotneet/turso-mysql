//! `GROUP_CONCAT` cut at the session's `group_concat_max_len`, the warning
//! MySQL raises for each cut, and the column the limit sizes, in both
//! protocols.
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
            AccountId::from_bytes([152; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    let long = format!(
        "INSERT INTO w VALUES (1, '{}'), (2, '{}')",
        "x".repeat(600),
        "y".repeat(600)
    );
    for sql in [
        "CREATE TABLE f (id INT PRIMARY KEY, g INT, a VARCHAR(20), b VARCHAR(20))",
        "INSERT INTO f VALUES (1,1,'x','yyyyy'),(2,1,'x','y'),(3,1,'x','y'),(4,2,NULL,NULL),(5,2,NULL,NULL),(6,3,'zz','zz'),(7,3,'zz','zz')",
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20))",
        "INSERT INTO t VALUES (1,'aé'),(2,'bb'),(3,'cc'),(4,'dd'),(5,'ee'),(6,'ff')",
        "CREATE TABLE u (id INT PRIMARY KEY, word VARCHAR(20))",
        "INSERT INTO u VALUES (1,'aéé'),(2,'😀x')",
        "CREATE TABLE w (id INT PRIMARY KEY, txt TEXT)",
        long.as_str(),
        "CREATE TABLE sink (s TEXT)",
        "CREATE TABLE kinds (id INT PRIMARY KEY, dbl DOUBLE, flt FLOAT, bl BLOB, vb VARBINARY(10), js JSON)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn run(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
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

/// Every row, NULL written as `NULL`.
fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<String>> {
    result(adapter, sql)
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    Some(value) => String::from_utf8(value.clone()).unwrap(),
                    None => "NULL".to_owned(),
                })
                .collect()
        })
        .collect()
}

/// The rows each warning of the last statement names, after checking each is
/// MySQL's 1260.
fn cut_rows(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>) -> Vec<u64> {
    rows(adapter, "SHOW WARNINGS")
        .into_iter()
        .map(|warning| {
            assert_eq!(warning[..2], ["Warning", "1260"]);
            warning[2]
                .strip_prefix("Row ")
                .and_then(|rest| rest.strip_suffix(" was cut by GROUP_CONCAT()"))
                .unwrap_or_else(|| panic!("unexpected warning {warning:?}"))
                .parse()
                .unwrap()
        })
        .collect()
}

fn shape(column: &ColumnDefinitionConfig) -> (u8, u32, u8, u16, u16) {
    (
        column.column_type,
        column.column_length,
        column.decimals,
        column.flags,
        column.character_set,
    )
}

const TEXT: u16 = DEFAULT_UTF8MB4_COLLATION as u16;

#[test]
fn a_group_concat_is_cut_at_1024_bytes_until_the_session_says_otherwise() {
    let (_directory, mut adapter) = adapter();
    let cut = result(&mut adapter, "SELECT GROUP_CONCAT(txt) FROM w");
    let value = cut.rows[0][0].clone().unwrap();
    assert_eq!(value.len(), 1024);
    assert_eq!(value[599..602], *b"x,y");
    assert_eq!(cut.warnings, 1);
    assert_eq!(
        shape(&cut.columns[0]),
        (MYSQL_TYPE_LONG_BLOB, 65536, 31, 0, TEXT)
    );
    assert_eq!(cut_rows(&mut adapter), [2]);
    // Reading the warnings leaves them, and the next statement that can
    // raise one clears them.
    assert_eq!(cut_rows(&mut adapter), [2]);
    run(&mut adapter, "SELECT GROUP_CONCAT(name) FROM t");
    assert_eq!(cut_rows(&mut adapter), Vec::<u64>::new());

    // The setting an ORM sends right after connecting.
    run(&mut adapter, "SET SESSION group_concat_max_len = 1000000");
    let whole = result(&mut adapter, "SELECT GROUP_CONCAT(txt) FROM w");
    assert_eq!(whole.rows[0][0].as_ref().unwrap().len(), 1201);
    assert_eq!(whole.warnings, 0);
    assert_eq!(cut_rows(&mut adapter), Vec::<u64>::new());
}

#[test]
fn the_limit_is_read_back_as_mysql_reads_it() {
    let (_directory, mut adapter) = adapter();
    let read = result(&mut adapter, "SELECT @@group_concat_max_len");
    assert_eq!(
        rows(&mut adapter, "SELECT @@group_concat_max_len"),
        [["1024"]]
    );
    assert_eq!(
        shape(&read.columns[0]),
        (
            MYSQL_TYPE_LONGLONG,
            21,
            0,
            MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            63
        )
    );
    for (sql, read_back) in [
        ("SET SESSION group_concat_max_len = 1000000", "1000000"),
        ("SET @@group_concat_max_len = 7", "7"),
        ("SET group_concat_max_len = 8", "8"),
        ("SET @@session.group_concat_max_len = 9", "9"),
        ("SET LOCAL group_concat_max_len = 10", "10"),
        (
            "SET SESSION group_concat_max_len = 18446744073709551615",
            "18446744073709551615",
        ),
        ("SET SESSION group_concat_max_len = DEFAULT", "1024"),
    ] {
        run(&mut adapter, sql);
        assert_eq!(
            rows(&mut adapter, "SELECT @@group_concat_max_len"),
            [[read_back]],
            "{sql}"
        );
        assert_eq!(
            rows(&mut adapter, "SHOW VARIABLES LIKE 'group_concat_max_len'"),
            [["group_concat_max_len", read_back]],
            "{sql}"
        );
    }
    // MySQL answers 1232 for a word, a number with a point, NULL and ON, and
    // takes anything below 4 as 4 with warning 1292, which is refused here
    // rather than raised. A global limit would reach other sessions.
    for sql in [
        "SET group_concat_max_len = '100'",
        "SET group_concat_max_len = 1.5",
        "SET group_concat_max_len = NULL",
        "SET group_concat_max_len = ON",
        "SET group_concat_max_len = -1",
        "SET group_concat_max_len = 3",
        "SET GLOBAL group_concat_max_len = 100",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert_eq!(
        rows(&mut adapter, "SELECT @@group_concat_max_len"),
        [["1024"]]
    );
}

#[test]
fn the_limit_sizes_the_column_in_both_protocols() {
    let (_directory, mut adapter) = adapter();
    for (limit, column_type, length) in [
        (4, MYSQL_TYPE_VAR_STRING, 16),
        (100, MYSQL_TYPE_VAR_STRING, 400),
        (512, MYSQL_TYPE_VAR_STRING, 2048),
        (513, MYSQL_TYPE_LONG_BLOB, 32832),
        (1024, MYSQL_TYPE_LONG_BLOB, 65536),
        (1_000_000, MYSQL_TYPE_LONG_BLOB, 64_000_000),
        (67_108_863, MYSQL_TYPE_LONG_BLOB, 4_294_967_232),
        (4_294_967_295, MYSQL_TYPE_LONG_BLOB, 4_294_967_295),
        (u64::MAX, MYSQL_TYPE_LONG_BLOB, 4_294_967_295),
    ] {
        run(
            &mut adapter,
            &format!("SET SESSION group_concat_max_len = {limit}"),
        );
        let sql = "SELECT GROUP_CONCAT(name) FROM t";
        let text = result(&mut adapter, sql);
        assert_eq!(
            shape(&text.columns[0]),
            (column_type, length, 31, 0, TEXT),
            "{limit}"
        );
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(prepared.columns, text.columns, "{limit}");
        adapter.execute_stmt_close(prepared.statement_id);
    }
    run(&mut adapter, "SET SESSION group_concat_max_len = 4");
    // A scalar subquery answers the shape of the aggregate inside it, and an
    // IFNULL falling back on a whole number the same shape, never null.
    let subquery = result(
        &mut adapter,
        "SELECT (SELECT GROUP_CONCAT(name) FROM t) AS c",
    );
    assert_eq!(
        shape(&subquery.columns[0]),
        (MYSQL_TYPE_VAR_STRING, 16, 31, 0, TEXT)
    );
    let defaulted = result(&mut adapter, "SELECT IFNULL(GROUP_CONCAT(name), 0) FROM t");
    assert_eq!(
        shape(&defaulted.columns[0]),
        (MYSQL_TYPE_VAR_STRING, 16, 31, MYSQL_NOT_NULL_FLAG, TEXT)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT IFNULL(GROUP_CONCAT(name), 0) FROM t WHERE id > 100"
        ),
        [["0"]]
    );
}

#[test]
fn a_cut_counts_bytes_and_keeps_whole_characters() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "SET SESSION group_concat_max_len = 5");
    // Five bytes hold `aé,b`, a separator counting like a value.
    assert_eq!(
        rows(&mut adapter, "SELECT GROUP_CONCAT(name) FROM t"),
        [["aé,b"]]
    );
    assert_eq!(cut_rows(&mut adapter), [2]);
    run(&mut adapter, "SET SESSION group_concat_max_len = 4");
    for (sql, expected) in [
        ("SELECT GROUP_CONCAT(name) FROM t", "aé,"),
        ("SELECT GROUP_CONCAT(word) FROM u WHERE id = 1", "aé"),
        ("SELECT GROUP_CONCAT(word) FROM u WHERE id = 2", "😀"),
        ("SELECT GROUP_CONCAT(name SEPARATOR '') FROM t", "aéb"),
    ] {
        assert_eq!(rows(&mut adapter, sql), [[expected]], "{sql}");
    }
    run(&mut adapter, "SET SESSION group_concat_max_len = 6");
    assert_eq!(
        rows(&mut adapter, "SELECT GROUP_CONCAT(word) FROM u"),
        [["aéé,"]]
    );
    // A result exactly as long as the limit is not cut.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT GROUP_CONCAT(name) FROM t WHERE id IN (3, 4)"
        ),
        [["cc,dd"]]
    );
    assert_eq!(cut_rows(&mut adapter), Vec::<u64>::new());
}

/// MySQL numbers the row a cut names by counting what that call has joined
/// across the statement's groups, leaving out NULLs and the values after a
/// cut, and warns in the order it reads the rows.
#[test]
fn each_call_numbers_its_cuts_by_what_it_has_joined() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "SET SESSION group_concat_max_len = 4");
    let joined = result(
        &mut adapter,
        "SELECT GROUP_CONCAT(a), GROUP_CONCAT(b) FROM f",
    );
    assert_eq!(joined.warnings, 2);
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT GROUP_CONCAT(a), GROUP_CONCAT(b) FROM f"
        ),
        [["x,x,", "yyyy"]]
    );
    assert_eq!(cut_rows(&mut adapter), [1, 3]);

    assert_eq!(
        rows(
            &mut adapter,
            "SELECT g, GROUP_CONCAT(a), GROUP_CONCAT(b) FROM f GROUP BY g"
        ),
        [
            ["1", "x,x,", "yyyy"],
            ["2", "NULL", "NULL"],
            ["3", "zz,z", "zz,z"]
        ]
    );
    assert_eq!(cut_rows(&mut adapter), [1, 3, 5, 3]);

    assert_eq!(
        rows(
            &mut adapter,
            "SELECT g, GROUP_CONCAT(a SEPARATOR '-----') FROM f GROUP BY g"
        ),
        [["1", "x---"], ["2", "NULL"], ["3", "zz--"]]
    );
    assert_eq!(cut_rows(&mut adapter), [2, 4]);

    assert_eq!(
        rows(
            &mut adapter,
            "SELECT g, GROUP_CONCAT(a) FROM f GROUP BY g LIMIT 1"
        ),
        [["1", "x,x,"]]
    );
    assert_eq!(cut_rows(&mut adapter), [3]);

    // A group the HAVING drops is still counted and warned about.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT g, GROUP_CONCAT(a) FROM f GROUP BY g HAVING COUNT(*) < 3"
        ),
        [["2", "NULL"], ["3", "zz,z"]]
    );
    assert_eq!(cut_rows(&mut adapter), [3, 5]);
}

/// A call named again where MySQL reads the same value is not counted or
/// warned about twice, and a subquery warns for its own.
#[test]
fn a_call_named_again_warns_once() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "SET SESSION group_concat_max_len = 4");
    for sql in [
        "SELECT g, GROUP_CONCAT(a) c FROM f GROUP BY g ORDER BY c",
        "SELECT g, GROUP_CONCAT(a) c FROM f GROUP BY g ORDER BY GROUP_CONCAT(a)",
    ] {
        assert_eq!(
            rows(&mut adapter, sql),
            [["2", "NULL"], ["1", "x,x,"], ["3", "zz,z"]],
            "{sql}"
        );
        assert_eq!(cut_rows(&mut adapter), [3, 5], "{sql}");
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT (SELECT GROUP_CONCAT(name) FROM t) AS c"
        ),
        [["aé,"]]
    );
    assert_eq!(cut_rows(&mut adapter), [2]);
    assert_eq!(
        rows(&mut adapter, "SELECT IFNULL(GROUP_CONCAT(name), 0) FROM t"),
        [["aé,"]]
    );
    assert_eq!(cut_rows(&mut adapter), [2]);
}

/// Under the strict mode this server runs, a cut value a statement writes
/// fails it with 1260 and writes nothing.
#[test]
fn writing_a_cut_value_fails_the_statement() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "SET SESSION group_concat_max_len = 4");
    assert!(matches!(
        adapter.execute_query("INSERT INTO sink (s) SELECT GROUP_CONCAT(b) FROM f GROUP BY g"),
        Err(FrontendErrorKind::GroupConcatCut)
    ));
    assert_eq!(rows(&mut adapter, "SELECT COUNT(*) FROM sink"), [["0"]]);
    run(&mut adapter, "SET SESSION group_concat_max_len = 1024");
    run(
        &mut adapter,
        "INSERT INTO sink (s) SELECT GROUP_CONCAT(b) FROM f GROUP BY g",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT s FROM sink"),
        [["yyyyy,y,y"], ["NULL"], ["zz,zz"]]
    );
}

/// Measured over `COM_STMT_PREPARE` and `COM_STMT_EXECUTE`: a prepared
/// statement keeps the largest limit it was prepared or executed under, for
/// the cut and for its column alike.
#[test]
fn a_prepared_statement_keeps_the_largest_limit_it_has_seen() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "SET SESSION group_concat_max_len = 5");
    let prepared = adapter
        .execute_stmt_prepare("SELECT GROUP_CONCAT(name) AS q FROM t")
        .unwrap();
    assert_eq!(
        shape(&prepared.columns[0]),
        (MYSQL_TYPE_VAR_STRING, 20, 31, 0, TEXT)
    );
    for (limit, answer, warnings, column_type, length) in [
        (5, "aé,b", 1, MYSQL_TYPE_VAR_STRING, 20),
        (1024, "aé,bb,cc,dd,ee,ff", 0, MYSQL_TYPE_LONG_BLOB, 65536),
        (8, "aé,bb,cc,dd,ee,ff", 0, MYSQL_TYPE_LONG_BLOB, 65536),
    ] {
        run(
            &mut adapter,
            &format!("SET SESSION group_concat_max_len = {limit}"),
        );
        let Ok(PreparedStatementExecutionResult::ResultSet(executed)) =
            adapter.execute_stmt_execute(prepared.statement_id, &[])
        else {
            panic!("the prepared GROUP_CONCAT must answer rows");
        };
        // Both reach the client as the same length-encoded bytes.
        let value = if column_type == MYSQL_TYPE_VAR_STRING {
            BinaryResultValue::Text(answer.to_owned())
        } else {
            BinaryResultValue::Blob(answer.as_bytes().to_vec())
        };
        assert_eq!(executed.rows, [vec![value]], "{limit}");
        assert_eq!(executed.warnings, warnings, "{limit}");
        assert_eq!(
            shape(&executed.columns[0]),
            (column_type, length, 31, 0, TEXT),
            "{limit}"
        );
    }
    run(&mut adapter, "SET SESSION group_concat_max_len = 5");
    let Ok(PreparedStatementExecutionResult::ResultSet(_)) =
        adapter.execute_stmt_execute(prepared.statement_id, &[])
    else {
        panic!("the prepared GROUP_CONCAT must answer rows");
    };
    assert_eq!(cut_rows(&mut adapter), Vec::<u64>::new());
    // A statement prepared afresh starts from the session's limit.
    let fresh = adapter
        .execute_stmt_prepare("SELECT GROUP_CONCAT(name) AS r FROM t")
        .unwrap();
    let Ok(PreparedStatementExecutionResult::ResultSet(executed)) =
        adapter.execute_stmt_execute(fresh.statement_id, &[])
    else {
        panic!("the prepared GROUP_CONCAT must answer rows");
    };
    assert_eq!(
        executed.rows,
        [vec![BinaryResultValue::Text("aé,b".to_owned())]]
    );
    assert_eq!(cut_rows(&mut adapter), [2]);
}

#[test]
fn a_group_concat_mysql_joins_by_a_rule_of_its_own_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // MySQL drops values equal under the column's collation and joins
        // the rest in its order: over b, B, é, e, A it answers `A,b,é`.
        "SELECT GROUP_CONCAT(DISTINCT name) FROM t",
        // MySQL writes 1e20 and 0.1 where the engine writes 1.0e+20 and
        // 0.100000001490116.
        "SELECT GROUP_CONCAT(dbl) FROM kinds",
        "SELECT GROUP_CONCAT(flt) FROM kinds",
        // A binary string and a JSON document answer a binary result of a
        // width of their own.
        "SELECT GROUP_CONCAT(bl) FROM kinds",
        "SELECT GROUP_CONCAT(vb) FROM kinds",
        "SELECT GROUP_CONCAT(js) FROM kinds",
        // A branch's column is a VAR_STRING or a BLOB by a rule of its own.
        "SELECT GROUP_CONCAT(name) FROM t UNION ALL SELECT GROUP_CONCAT(name) FROM t",
        "SELECT GROUP_CONCAT(name) FROM t UNION SELECT 'x'",
        // MySQL cuts and warns about a call the projection does not name.
        "SELECT g FROM f GROUP BY g ORDER BY GROUP_CONCAT(a)",
        "SELECT g, GROUP_CONCAT(a SEPARATOR ';') FROM f GROUP BY g ORDER BY GROUP_CONCAT(a)",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
}
