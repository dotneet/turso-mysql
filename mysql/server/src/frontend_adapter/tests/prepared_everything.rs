//! A client that prepares every statement, as Laravel does through PDO with
//! emulated prepares off: `CREATE TABLE`, the `information_schema` read that
//! comes before it, and every write, each prepared and then executed.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([111; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn prepared(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
    payload: &[u8],
) -> PreparedStatementExecutionResult {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = adapter
        .execute_stmt_execute(statement.statement_id, payload)
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"));
    adapter.execute_stmt_close(statement.statement_id);
    result
}

/// The null bitmap, the new-parameters flag and one VAR_STRING parameter.
fn one_word(word: &str) -> Vec<u8> {
    let mut payload = vec![
        0,
        1,
        MYSQL_TYPE_VAR_STRING,
        0,
        u8::try_from(word.len()).unwrap(),
    ];
    payload.extend_from_slice(word.as_bytes());
    payload
}

fn exists(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    table: &str,
) -> bool {
    let sql = format!(
        "select exists (select 1 from information_schema.tables where table_schema = schema() and table_name = '{table}' and table_type in ('BASE TABLE', 'SYSTEM VERSIONED')) as `exists`"
    );
    let PreparedStatementExecutionResult::ResultSet(result) = prepared(adapter, &sql, &[]) else {
        panic!("hasTable must answer a row");
    };
    match result.rows.as_slice() {
        [row] => match row.as_slice() {
            [BinaryResultValue::Integer(found)] => *found == 1,
            other => panic!("hasTable answered {other:?}"),
        },
        other => panic!("hasTable answered {other:?}"),
    }
}

#[test]
fn laravel_migrates_with_every_statement_prepared() {
    let (_directory, mut adapter) = adapter();
    assert!(!exists(&mut adapter, "migrations"));
    for sql in [
        "create table `migrations` (`id` int unsigned not null auto_increment primary key, `migration` varchar(255) not null, `batch` int not null) default character set utf8mb4 collate 'utf8mb4_0900_ai_ci'",
        "create table `users` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null, `email` varchar(255) not null, `created_at` timestamp null, `updated_at` timestamp null)",
        "alter table `users` add unique `users_email_unique`(`email`)",
    ] {
        assert!(
            matches!(
                prepared(&mut adapter, sql, &[]),
                PreparedStatementExecutionResult::Ok(_)
            ),
            "{sql}"
        );
    }
    assert!(exists(&mut adapter, "migrations"));

    let PreparedStatementExecutionResult::Ok(inserted) = prepared(
        &mut adapter,
        "insert into `migrations` (`migration`, `batch`) values (?, 1)",
        &one_word("0001_01_01_000000_create_users_table"),
    ) else {
        panic!("the insert must answer OK");
    };
    assert_eq!(inserted.affected_rows, 1);

    // A statement prepared this way does its work when it is executed, not
    // when it is prepared.
    let dropping = adapter
        .execute_stmt_prepare("drop table if exists `users`")
        .unwrap();
    assert!(exists(&mut adapter, "users"));
    adapter
        .execute_stmt_execute(dropping.statement_id, &[])
        .unwrap();
    assert!(!exists(&mut adapter, "users"));
}

/// A statement with a parameter, or one answering rows, is left to the checked
/// prepared path, which refuses what it does not take.
#[test]
fn only_a_statement_without_parameters_or_rows_is_run_as_text() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "create table t (id int primary key, n int) comment = ?",
        "SELECT GET_LOCK('x', 0)",
    ] {
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
}

/// Laravel's `migrate:fresh` lists every table and drops them in one
/// statement. Measured on MySQL 8.4.11: a parent and its child go together
/// whatever order they are named in; a statement naming a table that is not
/// there drops none of the others, answering 1051; `IF EXISTS` drops the rest
/// and notes each missing one; and a table named twice is 1066.
#[test]
fn laravel_drops_every_table_in_one_statement() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "create table `users` (`id` bigint not null auto_increment primary key)",
        "create table `posts` (`id` bigint not null auto_increment primary key, `user_id` bigint not null)",
        "alter table `posts` add constraint `posts_user_id_foreign` foreign key (`user_id`) references `users` (`id`)",
        "insert into `users` () values ()",
        "insert into `posts` (`user_id`) values (1)",
        "create table `cache` (`key` varchar(255) not null, primary key (`key`))",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    assert_eq!(
        adapter.execute_query("drop table `users`, `missing`, `cache`"),
        Err(FrontendErrorKind::UnknownTable)
    );
    assert!(exists(&mut adapter, "users"));
    assert!(exists(&mut adapter, "cache"));
    assert_eq!(
        adapter.execute_query("drop table `cache`, `cache`"),
        Err(FrontendErrorKind::NotUniqueTable)
    );

    let Ok(CommandExecutionResult::Ok(dropped)) =
        adapter.execute_query("drop table if exists `users`, `missing`, `posts`")
    else {
        panic!("the tables must be dropped");
    };
    assert_eq!(dropped.warnings, 1);
    assert!(!exists(&mut adapter, "users"));
    assert!(!exists(&mut adapter, "posts"));
    assert!(matches!(
        prepared(&mut adapter, "drop table `cache`", &[]),
        PreparedStatementExecutionResult::Ok(_)
    ));
    assert!(!exists(&mut adapter, "cache"));
}
