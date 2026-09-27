//! `FLOOR`, `CEIL` and `CEILING` over a `DECIMAL`, which land on a whole
//! number without passing through a double.
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
            AccountId::from_bytes([161; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE fc (id INT PRIMARY KEY, d DECIMAL(10,2), dn DECIMAL(10,2) NOT NULL, d0 DECIMAL(5,0), d3 DECIMAL(12,3), du DECIMAL(10,2) UNSIGNED, a DECIMAL(19,1), e DECIMAL(18,0), u BIGINT UNSIGNED)",
        "INSERT INTO fc VALUES (1, 1.50, -1.50, 5, 123456789.001, 0.99, 1.5, 999999999999999999, 5), (2, NULL, 0.00, NULL, -0.001, NULL, NULL, NULL, NULL), (3, -0.01, 99999999.99, -99999, -123456789.999, 99999999.99, NULL, -999999999999999999, NULL)",
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

/// Measured: each lands on the whole number below or above, the last of
/// eighteen whole digits kept exactly, and answers a LONGLONG of 21 — NOT
/// NULL over a NOT NULL column, and signed over an unsigned one.
#[test]
fn floor_and_ceil_land_a_decimal_on_a_whole_number() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT FLOOR(d), CEIL(d), FLOOR(dn), CEILING(dn), FLOOR(d0), CEIL(d0), FLOOR(d3), CEIL(d3), FLOOR(du), CEIL(du), FLOOR(e), CEIL(e) FROM fc ORDER BY id";
    let text = result(&mut adapter, sql);
    let rows: Vec<Vec<String>> = text
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
        .collect();
    assert_eq!(
        rows,
        [
            [
                "1",
                "2",
                "-2",
                "-1",
                "5",
                "5",
                "123456789",
                "123456790",
                "0",
                "1",
                "999999999999999999",
                "999999999999999999"
            ],
            ["NULL", "NULL", "0", "0", "NULL", "NULL", "-1", "0", "NULL", "NULL", "NULL", "NULL"],
            [
                "-1",
                "0",
                "99999999",
                "100000000",
                "-99999",
                "-99999",
                "-123456790",
                "-123456789",
                "99999999",
                "100000000",
                "-999999999999999999",
                "-999999999999999999"
            ],
        ]
    );
    let number = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let shapes: Vec<_> = text
        .columns
        .iter()
        .map(|column| (column.column_type, column.column_length, column.flags))
        .collect();
    let mut expected = vec![(MYSQL_TYPE_LONGLONG, 21, number); 12];
    expected[2].2 |= MYSQL_NOT_NULL_FLAG;
    expected[3].2 |= MYSQL_NOT_NULL_FLAG;
    assert_eq!(shapes, expected);
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns);
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert_eq!(binary.rows[0][2], BinaryResultValue::Integer(-2));
}

/// Measured: past eighteen whole digits, counting one for a fraction, MySQL
/// answers a NEWDECIMAL, which is not taken; an unsigned BIGINT is not a
/// DECIMAL.
#[test]
fn a_decimal_too_wide_for_a_whole_number_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in ["SELECT FLOOR(a) FROM fc", "SELECT CEIL(u) FROM fc"] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
