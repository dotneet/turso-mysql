//! `JSON_ARRAYAGG` and `GROUP_CONCAT`, which collect every value of a group
//! into one, in both protocols.
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
            AccountId::from_bytes([145; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE ja (id INT PRIMARY KEY, s VARCHAR(20), c CHAR(5), n BIGINT, g INT, t TEXT, r DOUBLE, d DECIMAL(5,2), j JSON)",
        "INSERT INTO ja VALUES (1, 'a\"b', 'x', 9223372036854775807, 1, 'long', 1.5, 1, '[1]'), (2, 'c\\\\d', 'yy ', -1, 1, NULL, 2, 2, NULL), (3, 'e\\nf\\tg', NULL, NULL, 2, '', 3, 3, NULL), (4, 'h', 'z', 0, 2, 'é/日', 4, 4, NULL), (5, '日本/</', '', 5, 3, 'x', 5, 5, NULL)",
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

fn texts(row: &[Option<Vec<u8>>]) -> Vec<Option<String>> {
    row.iter()
        .map(|value| value.clone().map(|value| String::from_utf8(value).unwrap()))
        .collect()
}

#[test]
fn json_arrayagg_collects_words_and_whole_numbers_the_way_mysql_writes_them() {
    let (_directory, mut adapter) = adapter();
    let all = result(
        &mut adapter,
        "SELECT JSON_ARRAYAGG(s), JSON_ARRAYAGG(c), JSON_ARRAYAGG(n), JSON_ARRAYAGG(t) FROM ja",
    );
    assert_eq!(
        texts(&all.rows[0]),
        [
            r#"["a\"b", "c\\d", "e\nf\tg", "h", "日本/</"]"#,
            r#"["x", "yy", null, "z", ""]"#,
            "[9223372036854775807, -1, null, 0, 5]",
            r#"["long", null, "", "é/日", "x"]"#,
        ]
        .map(|document| Some(document.to_owned()))
    );
    // The JSON type at the widest a document can be, with the binary flag.
    for column in &all.columns {
        assert_eq!(
            (
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags
            ),
            (
                MYSQL_TYPE_JSON,
                u32::MAX - 3,
                NOT_FIXED_DECIMALS,
                MYSQL_BINARY_FLAG
            ),
            "{}",
            column.name
        );
    }
    let grouped = result(
        &mut adapter,
        "SELECT g, JSON_ARRAYAGG(n) FROM ja GROUP BY g ORDER BY g",
    );
    assert_eq!(
        grouped
            .rows
            .iter()
            .map(|row| texts(row))
            .collect::<Vec<_>>(),
        [
            ["1", "[9223372036854775807, -1]"],
            ["2", "[null, 0]"],
            ["3", "[5]"],
        ]
        .map(|row| row.map(|value| Some(value.to_owned())).to_vec())
    );
    // No rows at all answer NULL rather than an empty array.
    assert_eq!(
        texts(
            &result(
                &mut adapter,
                "SELECT JSON_ARRAYAGG(id) FROM ja WHERE id > 10"
            )
            .rows[0]
        ),
        [None]
    );
    let prepared = adapter
        .execute_stmt_prepare("SELECT JSON_ARRAYAGG(n) FROM ja WHERE id = 3")
        .unwrap();
    assert_eq!(prepared.columns[0].column_type, MYSQL_TYPE_JSON);
    let Ok(PreparedStatementExecutionResult::ResultSet(rows)) =
        adapter.execute_stmt_execute(prepared.statement_id, &[])
    else {
        panic!("the prepared JSON_ARRAYAGG must answer rows");
    };
    assert_eq!(
        rows.rows,
        [vec![BinaryResultValue::Blob(b"[null]".to_vec())]]
    );
}

#[test]
fn json_arrayagg_refuses_what_mysql_writes_by_a_rule_of_its_own() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // A DOUBLE is written with MySQL's own digits, a DECIMAL as a JSON
        // decimal and a JSON document as itself.
        "SELECT JSON_ARRAYAGG(r) FROM ja",
        "SELECT JSON_ARRAYAGG(d) FROM ja",
        "SELECT JSON_ARRAYAGG(j) FROM ja",
        "SELECT JSON_ARRAYAGG(DISTINCT s) FROM ja",
        // A key that is NULL is MySQL's 3158.
        "SELECT JSON_OBJECTAGG(s, n) FROM ja",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
}

/// A prepared GROUP_CONCAT answers its LONG_BLOB as length-encoded bytes.
#[test]
fn group_concat_crosses_the_binary_protocol() {
    let (_directory, mut adapter) = adapter();
    let prepared = adapter
        .execute_stmt_prepare("SELECT GROUP_CONCAT(c) FROM ja WHERE id < 3")
        .unwrap();
    let Ok(PreparedStatementExecutionResult::ResultSet(rows)) =
        adapter.execute_stmt_execute(prepared.statement_id, &[])
    else {
        panic!("the prepared GROUP_CONCAT must answer rows");
    };
    assert_eq!(rows.rows, [vec![BinaryResultValue::Blob(b"x,yy".to_vec())]]);
}
