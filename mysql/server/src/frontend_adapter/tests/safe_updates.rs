//! `SET SQL_SAFE_UPDATES=1`, which MySQL Workbench opens every session with,
//! and the refusal it asks for.
//!
//! Every expectation here was measured on MySQL 8.4.11 over a table with a
//! primary key `id`, an index on `k` and an unindexed `v`.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn session() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("shop").unwrap();
    drop(catalog);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([0x53; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("shop").unwrap();
    for sql in [
        "CREATE TABLE s (id INT NOT NULL PRIMARY KEY, k INT, v INT, KEY (k))",
        "INSERT INTO s (id, k, v) VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3)",
        "SET SQL_SAFE_UPDATES=1",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

#[test]
fn a_where_an_index_serves_runs_and_one_no_index_can_serve_answers_1175() {
    let (_directory, mut adapter) = session();
    let Ok(CommandExecutionResult::ResultSet(read)) =
        adapter.execute_query("SELECT @@sql_safe_updates")
    else {
        panic!("the setting must read back");
    };
    assert_eq!(read.rows, [[Some(b"1".to_vec())]]);
    for sql in [
        "UPDATE s SET v = 0 WHERE id = 1",
        "UPDATE s SET v = 0 WHERE id > 0",
        "UPDATE s SET v = 0 WHERE id <> 1",
        "UPDATE s SET v = 0 WHERE id IN (1, 2)",
        "UPDATE s SET v = 0 WHERE id BETWEEN 1 AND 2",
        "UPDATE s SET v = 0 WHERE id = '1'",
        "UPDATE s SET v = 0 WHERE k > 0",
        "UPDATE s SET v = 0 WHERE id = 1 OR id = 2",
        "UPDATE s SET v = 0 WHERE v = 9 AND id = 1",
        "DELETE FROM s WHERE id = 3",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Ok(CommandExecutionResult::Ok(_))
            ),
            "{sql}"
        );
    }
    for sql in [
        "UPDATE s SET v = 0",
        "UPDATE s SET v = 0 WHERE v = 1",
        "UPDATE s SET v = 0 WHERE 1 = 1",
        "DELETE FROM s",
        "DELETE FROM s WHERE v = 3",
    ] {
        assert_eq!(
            adapter.execute_query(sql).err(),
            Some(FrontendErrorKind::UpdateWithoutKey),
            "{sql}"
        );
    }
    // MySQL answers 1175 for these too; which index its optimizer could use
    // is not read here, so they are refused as unsupported rather than run.
    for sql in [
        "UPDATE s SET v = 0 WHERE id = id",
        "UPDATE s SET v = 0 WHERE id + 0 = 1",
        "UPDATE s SET v = 0 WHERE id = 1 OR v = 2",
    ] {
        assert_eq!(
            adapter.execute_query(sql).err(),
            Some(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    let Ok(CommandExecutionResult::ResultSet(rows)) =
        adapter.execute_query("SELECT COUNT(*) FROM s")
    else {
        panic!("the table must be read");
    };
    assert_eq!(rows.rows, [[Some(b"2".to_vec())]]);
}

/// Measured: a prepared `id = ?` runs, and `v = ?` answers 1175 when it is
/// executed.
#[test]
fn a_prepared_update_is_held_to_the_same_when_it_runs() {
    let (_directory, mut adapter) = session();
    let by_key = adapter
        .execute_stmt_prepare("UPDATE s SET v = 0 WHERE id = ?")
        .unwrap();
    let by_value = adapter
        .execute_stmt_prepare("UPDATE s SET v = 0 WHERE v = ?")
        .unwrap();
    let one = [0, 1, MYSQL_TYPE_LONGLONG, 0, 1, 0, 0, 0, 0, 0, 0, 0];
    assert!(adapter
        .execute_stmt_execute(by_key.statement_id, &one)
        .is_ok());
    assert_eq!(
        adapter
            .execute_stmt_execute(by_value.statement_id, &one)
            .err(),
        Some(FrontendErrorKind::UpdateWithoutKey)
    );
}

#[test]
fn turning_it_off_lets_every_update_run() {
    let (_directory, mut adapter) = session();
    adapter.execute_query("SET sql_safe_updates = 0").unwrap();
    assert!(adapter.execute_query("UPDATE s SET v = 0").is_ok());
    adapter.execute_query("SET sql_safe_updates = 1").unwrap();
    adapter.execute_reset_connection().unwrap();
    adapter.execute_init_db("shop").unwrap();
    assert!(adapter.execute_query("DELETE FROM s WHERE v = 0").is_ok());
}
