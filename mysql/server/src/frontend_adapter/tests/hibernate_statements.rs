//! Statements Hibernate 6.6 writes for Spring Data JPA through Connector/J
//! 9.7, as the framework harness logged them.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([155; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE posts (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(200) NOT NULL, views INT NOT NULL DEFAULT 0)",
        "INSERT INTO posts (title, views) VALUES ('Hello', 10), ('Draft', 0), ('Bob writes', 3)",
    ] {
        changed(&mut adapter, sql);
    }
    (directory, adapter)
}

fn changed(adapter: &mut Adapter, sql: &str) -> u64 {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql}: {other:?}"),
    }
}

fn views(adapter: &mut Adapter) -> Vec<Vec<Option<Vec<u8>>>> {
    let Ok(CommandExecutionResult::ResultSet(read)) =
        adapter.execute_query("SELECT id, views FROM posts ORDER BY id")
    else {
        panic!("the posts must read back");
    };
    read.rows
}

fn row(id: &str, views: &str) -> Vec<Option<Vec<u8>>> {
    vec![
        Some(id.as_bytes().to_vec()),
        Some(views.as_bytes().to_vec()),
    ]
}

/// Every bulk JPQL update Hibernate writes gives its table an alias and names
/// each column through it. This answered 1235.
#[test]
fn a_bulk_update_names_its_table_through_an_alias() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        changed(
            &mut adapter,
            "update posts p1_0 set views=(p1_0.views+1) where p1_0.views<5"
        ),
        2
    );
    assert_eq!(
        views(&mut adapter),
        [row("1", "10"), row("2", "1"), row("3", "4")]
    );

    let statement = adapter
        .execute_stmt_prepare("update posts p1_0 set views=(p1_0.views+1) where p1_0.views<?")
        .unwrap();
    let mut five = vec![0, 1, MYSQL_TYPE_LONG, 0];
    five.extend(5i32.to_le_bytes());
    let PreparedStatementExecutionResult::Ok(result) = adapter
        .execute_stmt_execute(statement.statement_id, &five)
        .unwrap()
    else {
        panic!("an UPDATE answers OK");
    };
    assert_eq!(result.affected_rows, 2);
    assert_eq!(
        views(&mut adapter),
        [row("1", "10"), row("2", "2"), row("3", "5")]
    );

    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE posts AS p SET p.title = 'x' WHERE p.id = 1"
        ),
        1
    );
    // Measured: 1054, the table being known by its alias alone.
    assert!(adapter
        .execute_query("update posts p set posts.views = 1")
        .is_err());
    assert_eq!(
        views(&mut adapter),
        [row("1", "10"), row("2", "2"), row("3", "5")]
    );
}
