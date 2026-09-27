//! What a `UNION` answers when its branches read columns of different sizes
//! or types, which MySQL settles by a rule of its own for each pair.
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
            AccountId::from_bytes([141; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE u1 (id INT NOT NULL PRIMARY KEY, i INT, b BIGINT, s10 VARCHAR(10), s20 VARCHAR(20), r DOUBLE, t TINYINT, c CHAR(5), c8 CHAR(8), tx TEXT, dt DATETIME, dd DATE, fl TINYINT(1), sm SMALLINT, sb VARCHAR(10) COLLATE utf8mb4_bin)",
        "CREATE TABLE u2 (id INT NOT NULL PRIMARY KEY, i INT, b BIGINT, s10 VARCHAR(10), s20 VARCHAR(20), r DOUBLE, t TINYINT, c CHAR(5), c8 CHAR(8), tx TEXT, dt DATETIME, dd DATE, fl TINYINT(1), sm SMALLINT, sb VARCHAR(10) COLLATE utf8mb4_bin)",
        "INSERT INTO u1 VALUES (1, 10, 100, 'aa', 'bbbb', 0.5, 3, 'cc', 'cccccc', 'tt', '2026-01-01 00:00:00', '2026-01-01', 1, 7, 'aa')",
        "INSERT INTO u2 VALUES (2, 20, 200, 'AA', 'BBBB', 1.5, 4, 'dd', 'dddddd', 'TT', '2026-01-02 00:00:00', '2026-01-02', 0, 8, 'AA')",
        "CREATE TABLE items (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100), price DECIMAL(10,2), qty INT)",
        "INSERT INTO items (name, price, qty) VALUES ('apple', 1.25, 3), ('pear', 2.35, -4)",
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

fn texts(result: &TextResultSet) -> Vec<Option<String>> {
    result
        .rows
        .iter()
        .map(|row| {
            row[0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap())
        })
        .collect()
}

#[test]
fn a_union_column_takes_the_shape_mysql_gives_both_branches() {
    let (_directory, mut adapter) = adapter();
    for (sql, column_type, length, decimals, flags) in [
        // The wider whole number wins, at its own width.
        (
            "SELECT i FROM u1 UNION SELECT b FROM u2",
            MYSQL_TYPE_LONGLONG,
            20,
            0,
            MYSQL_NUM_FLAG,
        ),
        (
            "SELECT t FROM u1 UNION SELECT sm FROM u2",
            MYSQL_TYPE_SHORT,
            6,
            0,
            MYSQL_NUM_FLAG,
        ),
        // A TINYINT(1) reports 1 on its own and a TINYINT's 4 in a union.
        (
            "SELECT fl FROM u1 UNION SELECT fl FROM u2",
            MYSQL_TYPE_TINY,
            4,
            0,
            MYSQL_NUM_FLAG,
        ),
        (
            "SELECT fl FROM u1 UNION SELECT i FROM u2",
            MYSQL_TYPE_LONG,
            11,
            0,
            MYSQL_NUM_FLAG,
        ),
        (
            "SELECT i FROM u1 EXCEPT SELECT b FROM u2",
            MYSQL_TYPE_LONGLONG,
            20,
            0,
            MYSQL_NUM_FLAG,
        ),
        // Words take the wider width, four bytes to the character.
        (
            "SELECT s10 FROM u1 UNION ALL SELECT s20 FROM u2",
            MYSQL_TYPE_VAR_STRING,
            80,
            0,
            0,
        ),
        (
            "SELECT c FROM u1 UNION ALL SELECT s10 FROM u2",
            MYSQL_TYPE_VAR_STRING,
            40,
            0,
            0,
        ),
        (
            "SELECT c FROM u1 UNION ALL SELECT c8 FROM u2",
            MYSQL_TYPE_STRING,
            32,
            0,
            0,
        ),
        // A TEXT reports 262140 on its own and 1048560 in a union.
        (
            "SELECT tx FROM u1 UNION ALL SELECT tx FROM u2",
            MYSQL_TYPE_BLOB,
            1_048_560,
            0,
            MYSQL_BLOB_FLAG,
        ),
        (
            "SELECT s10 FROM u1 UNION ALL SELECT tx FROM u2",
            MYSQL_TYPE_BLOB,
            1_048_560,
            0,
            MYSQL_BLOB_FLAG,
        ),
        // A DOUBLE reports 22 on its own and 23 in a union.
        (
            "SELECT r FROM u1 UNION SELECT r FROM u2",
            MYSQL_TYPE_DOUBLE,
            23,
            NOT_FIXED_DECIMALS,
            MYSQL_NUM_FLAG,
        ),
        (
            "SELECT dt FROM u1 UNION SELECT dt FROM u2",
            MYSQL_TYPE_DATETIME,
            19,
            0,
            MYSQL_BINARY_FLAG,
        ),
        (
            "SELECT i FROM u1 UNION SELECT NULL FROM u2",
            MYSQL_TYPE_LONG,
            11,
            0,
            MYSQL_NUM_FLAG,
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
        assert_eq!(column.table, "", "{sql}");
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(prepared.columns, text.columns, "{sql}");
        // Executing reads the column's shape again, and has to answer the one
        // preparing announced.
        let executed = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap_or_else(|error| panic!("execute {sql}: {error:?}")),
        );
        assert_eq!(executed.columns, text.columns, "{sql}");
        adapter.execute_stmt_close(prepared.statement_id);
    }
    assert_eq!(
        texts(&result(
            &mut adapter,
            "SELECT i FROM u1 UNION SELECT b FROM u2 ORDER BY 1"
        )),
        [Some("10".to_owned()), Some("200".to_owned())]
    );
    // Ordered under the column's collation, where `B` sorts after `a`.
    assert_eq!(
        texts(&result(
            &mut adapter,
            "SELECT s10 AS x FROM u1 UNION ALL SELECT s20 FROM u2 ORDER BY x"
        )),
        [Some("aa".to_owned()), Some("BBBB".to_owned())]
    );
}

#[test]
fn a_union_refuses_a_pair_mysql_answers_by_a_rule_of_its_own() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // A number beside a word answers a word in MySQL, a VAR_STRING of 44,
        // and the engine keeps the two kinds apart.
        "SELECT i FROM u1 UNION SELECT s10 FROM u2",
        // A whole number beside a DOUBLE or a DATE beside a DATETIME answers a
        // conversion.
        "SELECT i FROM u1 UNION SELECT r FROM u2",
        "SELECT dd FROM u1 UNION SELECT dt FROM u2",
        // A column beside a written number answers a LONGLONG of 11.
        "SELECT i FROM u1 UNION SELECT 1 FROM u2",
        // Two collations.
        "SELECT sb FROM u1 UNION ALL SELECT s10 FROM u2",
        // MySQL keeps the first of two words equal but for case, `aa`, and
        // the engine the last, `AA`.
        "SELECT s10 FROM u1 UNION SELECT s10 FROM u2",
        "SELECT tx FROM u1 UNION SELECT tx FROM u2",
    ] {
        assert_eq!(
            adapter.execute_query(sql).err(),
            Some(FrontendErrorKind::Unsupported),
            "{sql}"
        );
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
    // Words compared by their bytes have no two spellings of one word.
    assert_eq!(
        texts(&result(
            &mut adapter,
            "SELECT sb FROM u1 UNION SELECT sb FROM u2 ORDER BY 1"
        )),
        [Some("AA".to_owned()), Some("aa".to_owned())]
    );
}

/// A table holding a `DECIMAL` can be read by a union that does not name it.
#[test]
fn a_union_reads_the_columns_beside_a_decimal() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        texts(&result(
            &mut adapter,
            "SELECT id FROM items UNION ALL SELECT id FROM items ORDER BY id"
        )),
        ["1", "1", "2", "2"].map(|id| Some(id.to_owned()))
    );
    assert_eq!(
        texts(&result(
            &mut adapter,
            "SELECT name FROM items WHERE qty > 0 UNION ALL SELECT name FROM items WHERE qty < 0"
        )),
        [Some("apple".to_owned()), Some("pear".to_owned())]
    );
    for sql in [
        "SELECT price FROM items UNION ALL SELECT price FROM items",
        "SELECT * FROM items UNION ALL SELECT * FROM items",
        "SELECT id FROM items WHERE price > 1 UNION ALL SELECT id FROM items",
    ] {
        assert_eq!(
            adapter.execute_query(sql).err(),
            Some(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
}
