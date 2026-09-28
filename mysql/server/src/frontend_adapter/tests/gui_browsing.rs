//! What database GUIs — DBeaver, MySQL Workbench, TablePlus — send when a
//! user browses a schema, modelled on their general-log output.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("root"));
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("dbtools").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([156; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("dbtools").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, email VARCHAR(191) NOT NULL, is_active TINYINT(1) NOT NULL DEFAULT 1)",
        "CREATE VIEW active_users AS SELECT id, email FROM users WHERE is_active = 1",
        "INSERT INTO users (email) VALUES ('alice@example.com'), ('bob@example.com')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

/// TablePlus reads one table's status by name. Measured: the name is
/// compared by its bytes, so `'USERS'` finds nothing, where `LIKE` would.
#[test]
fn tableplus_reads_one_table_s_status_by_its_name() {
    let (_directory, mut adapter) = adapter();
    let named = rows(
        &mut adapter,
        "SHOW TABLE STATUS FROM `dbtools` WHERE Name = 'users'",
    );
    assert_eq!(
        named,
        rows(
            &mut adapter,
            "SHOW TABLE STATUS FROM `dbtools` LIKE 'users'"
        )
    );
    assert_eq!(named.len(), 1);
    assert!(rows(
        &mut adapter,
        "SHOW TABLE STATUS FROM `dbtools` WHERE Name = 'USERS'"
    )
    .is_empty());
    assert!(adapter
        .execute_query("SHOW TABLE STATUS WHERE Engine = 'InnoDB'")
        .is_err());
}

/// Measured: a view's status is its name and the comment `VIEW`, every other
/// figure NULL. It read as an InnoDB table holding no rows.
#[test]
fn a_view_s_status_is_its_name_and_view() {
    let (_directory, mut adapter) = adapter();
    let mut expected = vec![None; 18];
    expected[0] = Some("active_users".to_owned());
    expected[17] = Some("VIEW".to_owned());
    assert_eq!(
        rows(&mut adapter, "SHOW TABLE STATUS LIKE 'active_users'"),
        [expected]
    );
}
