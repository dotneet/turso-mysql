//! A column holding fractional seconds that defaults to, or is rewritten to,
//! the moment a statement runs at — how Prisma and TypeORM declare their
//! timestamp columns.
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
            AccountId::from_bytes([121; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<Option<String>>> {
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

/// The fractional digits of a `YYYY-MM-DD hh:mm:ss.fff...` value.
fn digits_after_the_point(value: &str) -> usize {
    value.split_once('.').map_or(0, |(_, digits)| digits.len())
}

/// Prisma creates its own bookkeeping table this way, and every model with a
/// `@default(now())` the same way.
#[test]
fn prismas_migrations_table_takes_the_moment_to_the_millisecond() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE _prisma_migrations (\n    id                      VARCHAR(36) PRIMARY KEY NOT NULL,\n    checksum                VARCHAR(64) NOT NULL,\n    finished_at             DATETIME(3),\n    migration_name          VARCHAR(255) NOT NULL,\n    logs                    TEXT,\n    rolled_back_at          DATETIME(3),\n    started_at              DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),\n    applied_steps_count     INTEGER UNSIGNED NOT NULL DEFAULT 0\n) DEFAULT CHARACTER SET utf8mb4;\n",
    );
    run(
        &mut adapter,
        "INSERT INTO _prisma_migrations (id, checksum, migration_name) VALUES ('m1', 'c', '0001_init')",
    );
    let started = rows(&mut adapter, "SELECT started_at FROM _prisma_migrations");
    let started = started[0][0].as_deref().unwrap();
    assert_eq!(digits_after_the_point(started), 3, "{started}");

    let created = rows(&mut adapter, "SHOW CREATE TABLE _prisma_migrations");
    assert!(
        created[0][1]
            .as_deref()
            .unwrap()
            .contains("`started_at` datetime(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3)"),
        "{created:?}"
    );
    let columns = rows(
        &mut adapter,
        "SELECT COLUMN_NAME, COLUMN_DEFAULT, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '_prisma_migrations'",
    );
    assert!(
        columns.contains(&vec![
            Some("started_at".to_owned()),
            Some("CURRENT_TIMESTAMP(3)".to_owned()),
            Some("DEFAULT_GENERATED".to_owned())
        ]),
        "{columns:?}"
    );
}

/// TypeORM's `@CreateDateColumn` and `@UpdateDateColumn`.
#[test]
fn typeorms_date_columns_take_and_rewrite_the_moment_to_the_microsecond() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `post` (`id` int NOT NULL AUTO_INCREMENT, `title` varchar(255) NOT NULL, `createdAt` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6), `updatedAt` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6), PRIMARY KEY (`id`)) ENGINE=InnoDB",
    );
    run(&mut adapter, "INSERT INTO `post` (`title`) VALUES ('a')");
    let [created, updated] = rows(&mut adapter, "SELECT `createdAt`, `updatedAt` FROM `post`")
        .remove(0)
        .try_into()
        .unwrap();
    assert_eq!(digits_after_the_point(created.as_deref().unwrap()), 6);
    assert_eq!(created, updated);

    run(
        &mut adapter,
        "UPDATE `post` SET `updatedAt` = '2000-01-01 00:00:00' WHERE `id` = 1",
    );
    run(
        &mut adapter,
        "UPDATE `post` SET `title` = 'b' WHERE `id` = 1",
    );
    let updated = rows(&mut adapter, "SELECT `updatedAt` FROM `post`");
    let updated = updated[0][0].as_deref().unwrap();
    assert!(updated > "2000-01-01 00:00:00.000000", "{updated}");
    assert_eq!(digits_after_the_point(updated), 6);

    assert_eq!(
        rows(&mut adapter, "SHOW COLUMNS FROM `post` LIKE 'updatedAt'"),
        [[
            Some("updatedAt".to_owned()),
            Some("datetime(6)".to_owned()),
            Some("NO".to_owned()),
            Some(String::new()),
            Some("CURRENT_TIMESTAMP(6)".to_owned()),
            Some("DEFAULT_GENERATED on update CURRENT_TIMESTAMP(6)".to_owned()),
        ]]
    );
    let created = rows(&mut adapter, "SHOW CREATE TABLE `post`");
    assert!(
        created[0][1].as_deref().unwrap().contains(
            "`updatedAt` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6)"
        ),
        "{created:?}"
    );
}

/// Measured on MySQL 8.4.11: the moment is read to exactly the digits the
/// column holds, and any other count is 1067, or 1294 for `ON UPDATE`.
#[test]
fn the_moment_at_another_precision_than_the_column_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE c1 (id INT PRIMARY KEY, d DATETIME(3) DEFAULT CURRENT_TIMESTAMP)",
        "CREATE TABLE c2 (id INT PRIMARY KEY, d DATETIME(6) DEFAULT CURRENT_TIMESTAMP(3))",
        "CREATE TABLE c3 (id INT PRIMARY KEY, d DATETIME DEFAULT CURRENT_TIMESTAMP(3))",
        "CREATE TABLE c5 (id INT PRIMARY KEY, d DATETIME(6) DEFAULT NOW(6) ON UPDATE CURRENT_TIMESTAMP(3))",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
