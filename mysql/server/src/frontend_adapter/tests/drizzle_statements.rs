//! Statements Drizzle ORM 0.45 and drizzle-kit 0.31 send over mysql2,
//! replayed as the framework harness's sixth run captured them.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([191; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn result_set(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    result_set(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn refused(adapter: &mut Adapter, sql: &str) {
    assert!(
        matches!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported)
        ),
        "{sql} must be refused"
    );
}

/// Drizzle checks its connection with `select 1 from dual`, which answers
/// what `select 1` answers, written or prepared.
#[test]
fn drizzles_connection_check_reads_from_dual() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "select 1 from dual",
        "SELECT 1 FROM DUAL",
        "select 1 from dual where 1=0",
        "select 1 from dual order by 1",
    ] {
        let result = result_set(&mut adapter, sql);
        let [column] = result.columns.as_slice() else {
            panic!("{sql} must answer one column");
        };
        assert_eq!(column.name, "1", "{sql}");
        assert_eq!(column.column_type, MYSQL_TYPE_LONGLONG, "{sql}");
        assert_eq!(column.column_length, 2, "{sql}");
        // mysql2 reads 0x81 off the wire; the `NUM` the mysql client prints
        // beside these is its own, added for a numeric type.
        assert_eq!(column.flags, 0x81, "{sql}: NOT_NULL BINARY");
    }
    assert_eq!(
        rows(&mut adapter, "select 1 from dual"),
        [[Some("1".to_owned())]]
    );
    assert!(rows(&mut adapter, "select 1 from dual where 1=0").is_empty());
    assert_eq!(
        rows(&mut adapter, "SELECT 'a' as x, 2+3 FROM DUAL"),
        [[Some("a".to_owned()), Some("5".to_owned())]]
    );

    let statement = adapter.execute_stmt_prepare("select 1 from dual").unwrap();
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(statement.statement_id, &[])
        .unwrap()
    else {
        panic!("the prepared read must answer rows");
    };
    assert_eq!(result.rows, [[BinaryResultValue::Integer(1)]]);

    // A quoted `dual` is a table like any other, which this one is not.
    refused(&mut adapter, "select 1 from `dual`");
}
