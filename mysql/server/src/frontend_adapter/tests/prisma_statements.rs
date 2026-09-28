//! What Prisma's schema engine and client send, as prisma-engines 7.1 (the
//! engines of Prisma 6.19) writes it through `mysql_async`.
//!
//! Every expected answer was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn catalog() -> (
    tempfile::TempDir,
    Arc<MySqlDatabaseCatalog>,
    Arc<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, _factory) = catalog_factory(Arc::clone(&authorizer));
    catalog.create("prisma").unwrap();
    catalog.create("prisma_shadow").unwrap();
    (directory, catalog, authorizer)
}

/// A session connected with `database` selected, as a Prisma URL names it.
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
            AccountId::from_bytes([167; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db(database).unwrap();
    adapter
}

fn run(adapter: &mut Adapter, sql: &str) {
    match adapter.execute_query(sql) {
        Ok(_) => {}
        Err(error) => panic!("{sql}: {error:?} {:?}", adapter.take_error_message()),
    }
}

fn words(adapter: &mut Adapter, sql: &str) -> Vec<Option<String>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row[0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap())
        })
        .collect()
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

/// `prisma migrate reset` drops and makes again the database the app's own
/// pooled connections still have selected. MySQL drops it at once: an idle
/// session holds nothing up, keeps the name, answers 1049 for a table of it,
/// and reads the new database once there is one.
#[test]
fn migrate_reset_drops_a_database_the_pool_still_has_selected() {
    let (_directory, catalog, authorizer) = catalog();
    let mut pooled = session(&catalog, &authorizer, "prisma");
    run(
        &mut pooled,
        "CREATE TABLE `users` (`id` BIGINT NOT NULL AUTO_INCREMENT, PRIMARY KEY (`id`))",
    );
    run(&mut pooled, "INSERT INTO `users` () VALUES ()");
    let prepared = pooled
        .execute_stmt_prepare("SELECT `prisma`.`users`.`id` FROM `prisma`.`users`")
        .unwrap();

    let mut reset = session(&catalog, &authorizer, "prisma");
    run(&mut reset, "DROP DATABASE `prisma`");
    assert_eq!(words(&mut reset, "SELECT DATABASE()"), [None]);

    assert_eq!(
        words(&mut pooled, "SELECT DATABASE()"),
        [Some("prisma".to_owned())]
    );
    assert_eq!(
        refusal(&mut pooled, "SELECT `id` FROM `users`").0,
        FrontendErrorKind::UnknownDatabase
    );
    assert_eq!(
        pooled.execute_stmt_execute(prepared.statement_id, &[]),
        Err(FrontendErrorKind::UnknownDatabase)
    );
    assert_eq!(
        pooled.take_error_message(),
        Some(b"Unknown database 'prisma'".to_vec())
    );

    run(&mut reset, "CREATE DATABASE `prisma`");
    run(&mut reset, "USE `prisma`");
    run(
        &mut reset,
        "CREATE TABLE `users` (`id` BIGINT NOT NULL AUTO_INCREMENT, PRIMARY KEY (`id`))",
    );
    assert_eq!(
        words(&mut pooled, "SELECT COUNT(*) FROM `users`"),
        [Some("0".to_owned())]
    );
}

/// The shadow database is dropped by the session connected to it, which is
/// then in no database until it selects the one it makes again.
#[test]
fn migrate_dev_drops_the_shadow_database_it_is_in() {
    let (_directory, catalog, authorizer) = catalog();
    let mut shadow = session(&catalog, &authorizer, "prisma_shadow");
    run(
        &mut shadow,
        "CREATE TABLE `users` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    run(&mut shadow, "DROP DATABASE `prisma_shadow`");
    assert_eq!(words(&mut shadow, "SELECT DATABASE()"), [None]);
    assert_eq!(
        refusal(&mut shadow, "SELECT 1 FROM `users`").0,
        FrontendErrorKind::NoDatabaseSelected
    );
    run(&mut shadow, "CREATE DATABASE `prisma_shadow`");
    run(&mut shadow, "USE `prisma_shadow`");
    run(
        &mut shadow,
        "CREATE TABLE `users` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    assert_eq!(
        words(&mut shadow, "SHOW TABLES"),
        [Some("users".to_owned())]
    );
}

/// `DROP DATABASE` commits the session's transaction first, whichever
/// database it names, and a database that is not there answers 1008.
#[test]
fn drop_database_commits_first_and_names_a_database_that_is_not_there() {
    let (_directory, catalog, authorizer) = catalog();
    let mut adapter = session(&catalog, &authorizer, "prisma");
    run(
        &mut adapter,
        "CREATE TABLE `kept` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    run(&mut adapter, "BEGIN");
    run(&mut adapter, "INSERT INTO `kept` VALUES (1)");
    run(&mut adapter, "DROP DATABASE `prisma_shadow`");
    run(&mut adapter, "ROLLBACK");
    assert_eq!(
        words(&mut adapter, "SELECT COUNT(*) FROM `kept`"),
        [Some("1".to_owned())]
    );

    assert_eq!(
        refusal(&mut adapter, "DROP DATABASE `prisma_shadow`"),
        (
            FrontendErrorKind::NoDatabaseToDrop,
            "Can't drop database 'prisma_shadow'; database doesn't exist".to_owned()
        )
    );
}

/// A transaction another session holds open on the database holds the drop
/// up until it ends, and its rows are the last the database had.
#[test]
fn drop_database_waits_for_a_transaction_on_it() {
    let (_directory, catalog, authorizer) = catalog();
    let mut holder = session(&catalog, &authorizer, "prisma");
    run(
        &mut holder,
        "CREATE TABLE `kept` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    run(&mut holder, "BEGIN");
    run(&mut holder, "INSERT INTO `kept` VALUES (1)");

    let dropper = {
        let mut dropper = session(&catalog, &authorizer, "prisma_shadow");
        std::thread::spawn(move || dropper.execute_query("DROP DATABASE `prisma`").map(|_| ()))
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!dropper.is_finished());
    assert_eq!(
        words(&mut holder, "SELECT COUNT(*) FROM `kept`"),
        [Some("1".to_owned())]
    );
    run(&mut holder, "COMMIT");
    assert_eq!(dropper.join().unwrap(), Ok(()));

    assert_eq!(
        refusal(&mut holder, "SELECT COUNT(*) FROM `kept`").0,
        FrontendErrorKind::UnknownDatabase
    );
    assert_eq!(
        words(&mut holder, "SELECT DATABASE()"),
        [Some("prisma".to_owned())]
    );
}
