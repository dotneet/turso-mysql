//! `NOT col`, `col IS TRUE` and its kin, and a comparison standing as a
//! result column, each of which MySQL answers as the whole number 1, 0 or
//! NULL.
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
            AccountId::from_bytes([144; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE items (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100), price DECIMAL(10,2), qty INT, ratio DOUBLE, flag TINYINT(1))",
        "INSERT INTO items (name, price, qty, ratio, flag) VALUES ('apple', 1.25, 3, 2.5, 1), ('Bänana', 2.35, -4, 0.15, 0), (NULL, NULL, NULL, NULL, NULL)",
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

/// Each row of the result, its NULLs written as `NULL`.
fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    result(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| {
                    value.map_or("NULL".to_owned(), |value| String::from_utf8(value).unwrap())
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// A LONGLONG of length 1, named as written, and NOT NULL where nothing it
/// reads can be null. The truth tests never are.
#[test]
fn truth_values_report_the_column_mysql_reports() {
    let (_directory, mut adapter) = adapter();
    let nullable = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let not_null = nullable | MYSQL_NOT_NULL_FLAG;
    let text = result(
        &mut adapter,
        "SELECT NOT flag, NOT ratio, NOT id, flag IS TRUE, flag IS FALSE, flag IS NOT TRUE, flag IS NOT FALSE, qty > 1, name = 'apple', price > 2, ratio < 1, id > 1, 1 < qty FROM items",
    );
    let shapes = text
        .columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shapes,
        [
            ("NOT flag", MYSQL_TYPE_LONGLONG, 1, nullable),
            ("NOT ratio", MYSQL_TYPE_LONGLONG, 1, nullable),
            ("NOT id", MYSQL_TYPE_LONGLONG, 1, not_null),
            ("flag IS TRUE", MYSQL_TYPE_LONGLONG, 1, not_null),
            ("flag IS FALSE", MYSQL_TYPE_LONGLONG, 1, not_null),
            ("flag IS NOT TRUE", MYSQL_TYPE_LONGLONG, 1, not_null),
            ("flag IS NOT FALSE", MYSQL_TYPE_LONGLONG, 1, not_null),
            ("qty > 1", MYSQL_TYPE_LONGLONG, 1, nullable),
            ("name = 'apple'", MYSQL_TYPE_LONGLONG, 1, nullable),
            ("price > 2", MYSQL_TYPE_LONGLONG, 1, nullable),
            ("ratio < 1", MYSQL_TYPE_LONGLONG, 1, nullable),
            ("id > 1", MYSQL_TYPE_LONGLONG, 1, not_null),
            ("1 < qty", MYSQL_TYPE_LONGLONG, 1, nullable),
        ]
    );
    let counted = &result(&mut adapter, "SELECT COUNT(*) > 0 FROM items").columns[0];
    assert_eq!(
        (counted.column_type, counted.column_length, counted.flags),
        (MYSQL_TYPE_LONGLONG, 1, not_null)
    );
    let prepared = adapter
        .execute_stmt_prepare("SELECT NOT flag, flag IS TRUE, qty > 1, id > 1 FROM items")
        .unwrap();
    assert_eq!(
        prepared
            .columns
            .iter()
            .map(|column| (column.column_length, column.flags))
            .collect::<Vec<_>>(),
        [(1, nullable), (1, not_null), (1, nullable), (1, not_null)]
    );
}

#[test]
fn truth_values_answer_what_mysql_answers() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT NOT flag, NOT qty, NOT ratio, flag IS TRUE, flag IS FALSE, flag IS NOT TRUE, flag IS NOT FALSE, qty IS TRUE, ratio IS TRUE FROM items ORDER BY id"
        ),
        [
            "0 0 0 1 0 0 1 1 1",
            "1 0 0 0 1 1 0 1 1",
            // NULL is neither true nor false.
            "NULL NULL NULL 0 0 1 1 0 0",
        ]
    );
    // A word is compared under its column's collation, so `APPLE` finds
    // `apple` and `Bänana` sorts after `b`.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT qty > 1, qty = 3, name = 'APPLE', name > 'b', price > 2, ratio < 1, id > 1, qty <> 3, qty > 1.5 FROM items ORDER BY id"
        ),
        [
            "1 1 1 0 0 0 0 0 1",
            "0 0 0 1 1 1 1 1 0",
            "NULL NULL NULL NULL NULL NULL 1 NULL NULL",
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(*) > 0, COUNT(name) = 2 FROM items"
        ),
        ["1 1"]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(*) > 0 FROM items WHERE id > 100"
        ),
        ["0"]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, qty > 1 AS big FROM items ORDER BY big, id"
        ),
        ["3 NULL", "2 0", "1 1"]
    );
    // The truth tests read in a WHERE too.
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM items WHERE flag IS TRUE"),
        ["1"]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id FROM items WHERE flag IS NOT TRUE ORDER BY id"
        ),
        ["2", "3"]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id FROM items WHERE (qty > 0) IS FALSE"
        ),
        ["2"]
    );
}

#[test]
fn truth_values_refuse_what_mysql_reads_by_another_rule() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // MySQL reads a word as the number it begins with — `NOT 'apple'` is
        // 1 — and a DECIMAL by a rule of its own.
        "SELECT NOT name FROM items",
        "SELECT name IS TRUE FROM items",
        "SELECT NOT price FROM items",
        // A word not spelling a whole number against a number, and a column
        // against a column, are coercions; a parameter carries no kind until
        // it binds.
        "SELECT qty > '1.5' FROM items",
        "SELECT qty > name FROM items",
        "SELECT qty > ? FROM items",
        "SELECT qty = NULL FROM items",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
}
