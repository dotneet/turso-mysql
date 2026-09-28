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

const LARAVEL_OPENS_WITH: &str = "SET NAMES 'utf8mb4' COLLATE 'utf8mb4_unicode_ci', SESSION sql_mode='ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION'";

/// The first statements Laravel and Prisma send on a connection read the
/// server and the session, prepared. Measured on MySQL 8.4.11: each answers
/// the same columns over the binary protocol as over the text one —
/// `VERSION()` a NOT NULL `VAR_STRING` of 24, `DATABASE()` a nullable one of
/// 256, each word variable a nullable one of 87380 — and a whole-number
/// variable a `LONGLONG`.
#[test]
fn laravel_reads_the_server_and_the_session_prepared() {
    let (_directory, mut adapter) = adapter();
    adapter.execute_query(LARAVEL_OPENS_WITH).unwrap();
    for (sql, shapes) in [
        (
            "select version() as version, database() as db",
            &[("version", 24, MYSQL_NOT_NULL_FLAG), ("db", 256, 0)][..],
        ),
        (
            "select @@character_set_client as client, @@character_set_connection as conn, @@character_set_results as results, @@collation_connection as collation, @@sql_mode as sql_mode",
            &[
                ("client", 87380, 0),
                ("conn", 87380, 0),
                ("results", 87380, 0),
                ("collation", 87380, 0),
                ("sql_mode", 87380, 0),
            ][..],
        ),
        (
            "SELECT VERSION() AS version, DATABASE() AS db, @@character_set_client AS cs",
            &[
                ("version", 24, MYSQL_NOT_NULL_FLAG),
                ("db", 256, 0),
                ("cs", 87380, 0),
            ][..],
        ),
        (
            "SELECT @@version, @@GLOBAL.version",
            &[("@@version", 87380, 0), ("@@GLOBAL.version", 87380, 0)][..],
        ),
    ] {
        let prepared_columns = adapter.execute_stmt_prepare(sql).unwrap().columns;
        assert_eq!(
            prepared_columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.column_length,
                    column.flags,
                    column.column_type
                ))
                .collect::<Vec<_>>(),
            shapes
                .iter()
                .map(|(name, length, flags)| (*name, *length, *flags, MYSQL_TYPE_VAR_STRING))
                .collect::<Vec<_>>(),
            "{sql}"
        );
        let Ok(CommandExecutionResult::ResultSet(text)) = adapter.execute_query(sql) else {
            panic!("{sql} must answer a row as text");
        };
        let PreparedStatementExecutionResult::ResultSet(binary) = prepared(&mut adapter, sql, &[])
        else {
            panic!("{sql} must answer a row prepared");
        };
        assert_eq!(binary.columns, text.columns, "{sql}");
        assert_eq!(
            binary.rows,
            text.rows
                .iter()
                .map(|row| row
                    .iter()
                    .map(|value| BinaryResultValue::Text(
                        String::from_utf8(value.clone().unwrap()).unwrap()
                    ))
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }
    let PreparedStatementExecutionResult::ResultSet(settings) = prepared(
        &mut adapter,
        "select @@character_set_client as client, @@collation_connection as collation, @@autocommit",
        &[],
    ) else {
        panic!("the settings must answer a row");
    };
    assert_eq!(
        settings.rows,
        [[
            BinaryResultValue::Text("utf8mb4".to_owned()),
            BinaryResultValue::Text("utf8mb4_unicode_ci".to_owned()),
            BinaryResultValue::Integer(1),
        ]]
    );
    assert_eq!(settings.columns[2].column_type, MYSQL_TYPE_LONGLONG);
    // A variable the server does not have is 1193 when it is prepared, as it
    // is when it is run.
    assert_eq!(
        adapter.execute_stmt_prepare("select @@no_such_variable as x"),
        Err(FrontendErrorKind::UnknownSystemVariable)
    );
}

/// A variable read beside a table's column is refused rather than answered
/// 1054: the engine reads the variable as a column it does not have, and
/// MySQL has no such column to miss.
#[test]
fn a_variable_beside_a_column_is_no_unknown_column() {
    let (_directory, mut adapter) = adapter();
    let sql = "select @@sql_mode as sql_mode, `id` from `records`";
    assert_eq!(
        adapter.execute_query(sql),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_stmt_prepare(sql),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("select `nope` from `records`"),
        Err(FrontendErrorKind::UnknownColumn)
    );
}
