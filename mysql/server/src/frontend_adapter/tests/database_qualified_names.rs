//! Names written with the selected database before them, as Prisma writes
//! every table and column (`prisma.users.id`) and TypeORM writes its
//! migrations table (`typeorm.migrations`).

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("prisma").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([142; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("prisma").unwrap();
    for sql in [
        "CREATE TABLE `users` (`id` INTEGER NOT NULL AUTO_INCREMENT, `email` VARCHAR(191) NOT NULL, `balance` DECIMAL(10, 2) NOT NULL DEFAULT 0.00, UNIQUE INDEX `users_email_key`(`email`), PRIMARY KEY (`id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        "CREATE TABLE `tags` (`id` INTEGER NOT NULL AUTO_INCREMENT, `name` VARCHAR(100) NOT NULL, UNIQUE INDEX `tags_name_key`(`name`), PRIMARY KEY (`id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        "CREATE TABLE `migrations` (`id` int NOT NULL AUTO_INCREMENT, `timestamp` bigint NOT NULL, `name` varchar(255) NOT NULL, PRIMARY KEY (`id`))",
        "INSERT INTO `users` (`email`, `balance`) VALUES ('alice@example.com', 1.50), ('bob@example.com', 2.00)",
        "INSERT INTO `migrations` (`timestamp`, `name`) VALUES (1700000000000, 'AddPostSlug1700000000000')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn result_set(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql}: {other:?}"),
    }
}

fn prepared(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
    parameters: &[Bound],
) -> PreparedStatementExecutionResult {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = adapter
        .execute_stmt_execute(statement.statement_id, &payload(parameters))
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"));
    adapter.execute_stmt_close(statement.statement_id);
    result
}

enum Bound<'a> {
    Word(&'a str),
    Number(i64),
}

/// The null bitmap, the new-parameters flag, each parameter's type and then
/// each value, as a client sends them.
fn payload(parameters: &[Bound]) -> Vec<u8> {
    let mut payload = vec![0; parameters.len().div_ceil(8)];
    payload.push(1);
    for parameter in parameters {
        payload.extend_from_slice(match parameter {
            Bound::Word(_) => &[MYSQL_TYPE_VAR_STRING, 0],
            Bound::Number(_) => &[MYSQL_TYPE_LONGLONG, 0],
        });
    }
    for parameter in parameters {
        match parameter {
            Bound::Word(word) => {
                payload.push(u8::try_from(word.len()).unwrap());
                payload.extend_from_slice(word.as_bytes());
            }
            Bound::Number(number) => payload.extend_from_slice(&number.to_le_bytes()),
        }
    }
    payload
}

fn affected(result: PreparedStatementExecutionResult) -> u64 {
    match result {
        PreparedStatementExecutionResult::Ok(result) => result.affected_rows,
        other => panic!("expected OK, got {other:?}"),
    }
}

fn rows(result: PreparedStatementExecutionResult) -> Vec<Vec<BinaryResultValue>> {
    match result {
        PreparedStatementExecutionResult::ResultSet(result) => result.rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Measured on MySQL 8.4.11: a table or a column named with the selected
/// database answers the rows and the column metadata the bare name does —
/// `prisma.users.id` is a column named `id` of table `users` in database
/// `prisma`. The database name is matched without regard to case, as
/// `lower_case_table_names=1`, which this server reports, has MySQL do.
#[test]
fn a_name_written_with_the_selected_database_reads_what_the_bare_name_does() {
    let (_directory, mut adapter) = adapter();
    for (qualified, bare) in [
        (
            "SELECT * FROM `prisma`.`migrations` `migrations` ORDER BY `id` DESC",
            "SELECT * FROM `migrations` `migrations` ORDER BY `id` DESC",
        ),
        (
            "SELECT `prisma`.`users`.`id`, `prisma`.`users`.`email`, `prisma`.`users`.`balance` FROM `prisma`.`users` WHERE (`prisma`.`users`.`email` = 'bob@example.com' AND 1=1)",
            "SELECT `users`.`id`, `users`.`email`, `users`.`balance` FROM `users` WHERE (`users`.`email` = 'bob@example.com' AND 1=1)",
        ),
        (
            "SELECT Prisma.users.id FROM PRISMA.users ORDER BY prisma . users . id",
            "SELECT users.id FROM users ORDER BY users.id",
        ),
        (
            "SELECT prisma.users.* FROM prisma.users WHERE prisma.users.id = 2",
            "SELECT users.* FROM users WHERE users.id = 2",
        ),
        (
            "SELECT COUNT(*) AS `_count$_all`, SUM(`balance`) AS `_sum$balance` FROM (SELECT `prisma`.`users`.`id`, `prisma`.`users`.`balance` FROM `prisma`.`users` WHERE 1=1) AS `sub`",
            "SELECT COUNT(*) AS `_count$_all`, SUM(`balance`) AS `_sum$balance` FROM (SELECT `users`.`id`, `users`.`balance` FROM `users` WHERE 1=1) AS `sub`",
        ),
        (
            "SELECT COUNT(prisma.users.id) AS n FROM users",
            "SELECT COUNT(users.id) AS n FROM users",
        ),
    ] {
        assert_eq!(
            result_set(&mut adapter, qualified),
            result_set(&mut adapter, bare),
            "{qualified}"
        );
    }
    let migrations = result_set(
        &mut adapter,
        "SELECT * FROM `prisma`.`migrations` `migrations` ORDER BY `id` DESC",
    );
    assert_eq!(
        migrations.rows,
        [vec![
            Some(b"1".to_vec()),
            Some(b"1700000000000".to_vec()),
            Some(b"AddPostSlug1700000000000".to_vec()),
        ]]
    );
    assert_eq!(migrations.columns[0].schema, "prisma");
    assert_eq!(migrations.columns[0].table, "migrations");
}

/// Prisma prepares every statement, each name written with the database.
#[test]
fn prisma_writes_and_reads_through_prepared_statements() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        affected(prepared(
            &mut adapter,
            "INSERT INTO `prisma`.`tags` (`name`) VALUES (?)",
            &[Bound::Word("news")],
        )),
        1
    );
    assert_eq!(
        affected(prepared(
            &mut adapter,
            "INSERT IGNORE INTO `prisma`.`tags` (`name`) VALUES (?), (?), (?)",
            &[Bound::Word("news"), Bound::Word("rust"), Bound::Word("sql")],
        )),
        2
    );
    assert_eq!(
        rows(prepared(
            &mut adapter,
            "SELECT `prisma`.`tags`.`id` FROM `prisma`.`tags` WHERE (`prisma`.`tags`.`name` = ? AND 1=1)",
            &[Bound::Word("rust")],
        )),
        [vec![BinaryResultValue::Integer(2)]]
    );
    assert_eq!(
        rows(prepared(
            &mut adapter,
            "SELECT `prisma`.`users`.`id`, `prisma`.`users`.`email` FROM `prisma`.`users` WHERE (`prisma`.`users`.`email` = ? AND 1=1) LIMIT ? OFFSET ?",
            &[
                Bound::Word("bob@example.com"),
                Bound::Number(1),
                Bound::Number(0),
            ],
        )),
        [vec![
            BinaryResultValue::Integer(2),
            BinaryResultValue::Text("bob@example.com".to_owned()),
        ]]
    );
    assert_eq!(
        affected(prepared(
            &mut adapter,
            "UPDATE `prisma`.`tags` SET `name` = ? WHERE (`prisma`.`tags`.`id` = ? AND 1=1)",
            &[Bound::Word("go"), Bound::Number(3)],
        )),
        1
    );
    assert_eq!(
        affected(prepared(
            &mut adapter,
            "DELETE FROM `prisma`.`tags` WHERE (`prisma`.`tags`.`id` = ? AND 1=1)",
            &[Bound::Number(1)],
        )),
        1
    );
    assert_eq!(
        result_set(&mut adapter, "SELECT `name` FROM `tags` ORDER BY `id`").rows,
        [vec![Some(b"rust".to_vec())], vec![Some(b"go".to_vec())]]
    );
}

/// What leaving the database out would change is refused rather than read.
#[test]
fn a_database_name_that_would_change_the_answer_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // Measured on MySQL 8.4.11: the result column is named
        // `COUNT(prisma.users.id)`, after the text as written.
        "SELECT COUNT(prisma.users.id) FROM users",
        "SELECT prisma.users.id + 1 FROM users",
        // The engine reads the selected database alone.
        "SELECT id FROM reports.records",
        "SELECT reports.users.id FROM users",
        // A qualified name never reads a `WITH` name.
        "WITH users AS (SELECT 1 AS id) SELECT id FROM prisma.users",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}
