//! `DROP DATABASE` seen from the session that drops and from the sessions
//! that still have the database selected.
//!
//! Every expected answer was measured on MySQL 8.4.11 with two sessions.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn catalog() -> (
    tempfile::TempDir,
    Arc<MySqlDatabaseCatalog>,
    Arc<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, _factory) = catalog_factory(Arc::clone(&authorizer));
    catalog.create("shop").unwrap();
    catalog.create("other").unwrap();
    (directory, catalog, authorizer)
}

fn session(
    catalog: &Arc<MySqlDatabaseCatalog>,
    authorizer: &Arc<RecordingAuthorizer>,
    database: &str,
) -> Adapter {
    let factory = AuthorizedDatabaseAdapterFactory::new(
        Arc::clone(catalog),
        binary_context(),
        Arc::clone(authorizer),
    );
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([171; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db(database).unwrap();
    adapter
}

fn run(adapter: &mut Adapter, sql: &str) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(ok)) => ok,
        Ok(other) => panic!("{sql}: expected OK, got {other:?}"),
        Err(error) => panic!("{sql}: {error:?} {:?}", adapter.take_error_message()),
    }
}

fn words(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                    .collect()
            })
            .collect(),
        other => panic!(
            "{sql} must return a result set, got {other:?} {:?}",
            adapter.take_error_message()
        ),
    }
}

fn one(value: &str) -> Vec<Vec<Option<String>>> {
    vec![vec![Some(value.to_owned())]]
}

fn refusal(adapter: &mut Adapter, sql: &str) -> (FrontendErrorKind, String) {
    let error = adapter
        .execute_query(sql)
        .expect_err("the statement must be refused");
    let message = adapter
        .take_error_message()
        .map(|message| String::from_utf8(message).unwrap())
        .unwrap_or_default();
    (error, message)
}

/// `DROP DATABASE IF EXISTS` of a database that is not there answers OK
/// counting one warning that `SHOW WARNINGS` does not list, commits the
/// session's transaction first, and counts nothing under `sql_notes = 0`.
#[test]
fn drop_database_if_exists_answers_a_missing_database_with_ok() {
    let (_directory, catalog, authorizer) = catalog();
    let mut adapter = session(&catalog, &authorizer, "shop");
    run(&mut adapter, "CREATE TABLE kept (id INT PRIMARY KEY)");
    run(&mut adapter, "DROP TABLE IF EXISTS nothing_here");
    run(&mut adapter, "BEGIN");
    run(&mut adapter, "INSERT INTO kept VALUES (1)");

    let answer = run(&mut adapter, "DROP DATABASE IF EXISTS nowhere");
    assert_eq!((answer.affected_rows, answer.warnings), (0, 1));
    assert!(words(&mut adapter, "SHOW WARNINGS").is_empty());
    assert_eq!(words(&mut adapter, "SHOW COUNT(*) WARNINGS"), one("0"));
    run(&mut adapter, "ROLLBACK");
    assert_eq!(words(&mut adapter, "SELECT COUNT(*) FROM kept"), one("1"));

    assert_eq!(
        run(&mut adapter, "drop schema if exists `nowhere`;").warnings,
        1
    );
    run(&mut adapter, "SET sql_notes = 0");
    assert_eq!(
        run(&mut adapter, "DROP DATABASE IF EXISTS nowhere").warnings,
        0
    );

    run(&mut adapter, "SET sql_notes = 1");
    assert_eq!(
        run(&mut adapter, "DROP DATABASE IF EXISTS other").warnings,
        0
    );
    assert!(!catalog.list().unwrap().contains(&"other".to_owned()));
    assert_eq!(
        refusal(&mut adapter, "DROP DATABASE other"),
        (
            FrontendErrorKind::NoDatabaseToDrop,
            "Can't drop database 'other'; database doesn't exist".to_owned()
        )
    );
}

/// A session whose database another session dropped keeps its name through a
/// plain `DROP DATABASE` of it, which answers 1008, and is left in none by
/// `DROP DATABASE IF EXISTS`, as though it had dropped it itself.
#[test]
fn drop_database_if_exists_leaves_a_session_whose_database_went_in_none() {
    let (_directory, catalog, authorizer) = catalog();
    let mut left_behind = session(&catalog, &authorizer, "shop");
    let mut dropper = session(&catalog, &authorizer, "other");
    run(&mut dropper, "DROP DATABASE shop");

    assert_eq!(
        refusal(&mut left_behind, "DROP DATABASE shop").0,
        FrontendErrorKind::NoDatabaseToDrop
    );
    assert_eq!(words(&mut left_behind, "SELECT DATABASE()"), one("shop"));
    assert_eq!(
        run(&mut left_behind, "DROP DATABASE IF EXISTS shop").warnings,
        1
    );
    assert_eq!(
        words(&mut left_behind, "SELECT DATABASE()"),
        vec![vec![None]]
    );
}
