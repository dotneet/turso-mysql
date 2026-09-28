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

/// An ORM's batch insert is one long statement with a `?` for each value, and
/// every reading of a statement costs time in proportion to its length. When
/// it is prepared it is tokenized once by each of the two ways the recognizers
/// read it there, parsed once and read by the engine once; when it is executed
/// it is read once more, to number its rows. Before the readings were kept
/// while a statement is answered, preparing it tokenized it 13 times, parsed
/// it 9 times and had the engine read it 4.
#[test]
fn a_long_batch_insert_is_read_whole_once_when_prepared_and_once_when_executed() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query("create table `users` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null, `email` varchar(255) not null)")
        .unwrap();
    let rows = 1000;
    let sql = format!(
        "insert into `users` (`name`, `email`) values {}",
        vec!["(?, ?)"; rows].join(", ")
    );
    let values = (0..rows)
        .flat_map(|row| [format!("user {row}"), format!("user{row}@example.com")])
        .collect::<Vec<_>>();

    let before = turso_mysql_parser::bytes_read();
    let statement = adapter.execute_stmt_prepare(&sql).unwrap();
    let prepared_at = turso_mysql_parser::bytes_read();
    let result = adapter
        .execute_stmt_execute(statement.statement_id, &batch_of_words(&values))
        .unwrap();
    let executed_at = turso_mysql_parser::bytes_read();

    let PreparedStatementExecutionResult::Ok(result) = result else {
        panic!("the insert must answer OK");
    };
    assert_eq!((result.affected_rows, result.last_insert_id), (1000, 1));
    assert_eq!(
        whole_readings(before, prepared_at, sql.len()),
        turso_mysql_parser::BytesRead {
            tokenized: 2,
            parsed: 1,
            parsed_by_the_engine: 1,
            tokenized_as_a_command: 0,
        }
    );
    assert_eq!(
        whole_readings(prepared_at, executed_at, sql.len()),
        turso_mysql_parser::BytesRead {
            tokenized: 1,
            parsed: 1,
            parsed_by_the_engine: 0,
            tokenized_as_a_command: 0,
        }
    );
}

/// The null bitmap, the new-parameters flag and one VAR_STRING parameter for
/// each word.
fn batch_of_words(words: &[String]) -> Vec<u8> {
    let mut payload = vec![0; words.len().div_ceil(8)];
    payload.push(1);
    for _ in words {
        payload.extend_from_slice(&[MYSQL_TYPE_VAR_STRING, 0]);
    }
    for word in words {
        payload.push(u8::try_from(word.len()).unwrap());
        payload.extend_from_slice(word.as_bytes());
    }
    payload
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

/// Laravel 12's `migrate:fresh` qualifies every table it drops by its
/// database, prepared: ``drop table `laravel`.`cache`, `laravel`.`jobs`, ...``.
/// Measured on MySQL 8.4.11, a qualified name drops the same table an
/// unqualified one does, a missing one is 1051 and drops none of the others,
/// and a table named with and without its database is 1066. This server
/// reports `lower_case_table_names=1`, so a database is named without regard
/// to case. A table of another database is refused.
#[test]
fn laravel_drops_every_table_qualified_by_its_database() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "create table `cache` (`key` varchar(255) not null, primary key (`key`))",
        "create table `jobs` (`id` bigint unsigned not null auto_increment primary key)",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let missing = adapter
        .execute_stmt_prepare("drop table `reports`.`cache`, `reports`.`missing`")
        .unwrap();
    assert_eq!(
        adapter.execute_stmt_execute(missing.statement_id, &[]),
        Err(FrontendErrorKind::UnknownTable)
    );
    assert!(exists(&mut adapter, "cache"));
    assert_eq!(
        adapter.execute_query("drop table `reports`.`cache`, `cache`"),
        Err(FrontendErrorKind::NotUniqueTable)
    );
    assert_eq!(
        adapter.execute_query("drop table `other`.`cache`"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("drop table `mysql`.`user`"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert!(exists(&mut adapter, "cache"));
    assert!(matches!(
        prepared(
            &mut adapter,
            "drop table `REPORTS`.`cache`, `reports`.`jobs`",
            &[]
        ),
        PreparedStatementExecutionResult::Ok(_)
    ));
    assert!(!exists(&mut adapter, "cache"));
    assert!(!exists(&mut adapter, "jobs"));
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

/// One value a statement is executed with, bound the way PDO binds it: a PHP
/// integer as a `LONGLONG`, everything else as a `VAR_STRING`.
enum Bound<'a> {
    Word(&'a str),
    Number(i64),
}

fn bound(values: &[Bound<'_>]) -> Vec<u8> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        let kind = match value {
            Bound::Word(_) => MYSQL_TYPE_VAR_STRING,
            Bound::Number(_) => MYSQL_TYPE_LONGLONG,
        };
        payload.extend_from_slice(&[kind, 0]);
    }
    for value in values {
        match value {
            Bound::Word(word) => {
                payload.push(u8::try_from(word.len()).unwrap());
                payload.extend_from_slice(word.as_bytes());
            }
            Bound::Number(number) => payload.extend_from_slice(&number.to_le_bytes()),
        }
    }
    payload
}

fn affected(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
    values: &[Bound<'_>],
) -> u64 {
    match prepared(adapter, sql, &bound(values)) {
        PreparedStatementExecutionResult::Ok(result) => result.affected_rows,
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn words_of(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must answer rows");
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

fn word(value: &str) -> Option<String> {
    Some(value.to_owned())
}

/// Laravel's cache, locks, migrations and Eloquent updates find their row by a
/// column of words compared with a bound word. Measured on MySQL 8.4.11 over
/// `utf8mb4_unicode_ci` columns: the word matches without regard to case or
/// trailing spaces; an `IN` list of bound words does the same; and a `?` in a
/// `SET` is counted before the ones in the `WHERE`.
#[test]
fn laravel_writes_where_a_column_of_words_matches_a_bound_word() {
    let (_directory, mut adapter) = adapter();
    adapter.execute_query(LARAVEL_OPENS_WITH).unwrap();
    for sql in [
        "create table `cache` (`key` varchar(255) not null, `value` mediumtext not null, `expiration` int not null, primary key (`key`)) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "create table `cache_locks` (`key` varchar(255) not null, `owner` varchar(255) not null, `expiration` int not null, primary key (`key`)) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "create table `migrations` (`id` int unsigned not null auto_increment primary key, `migration` varchar(255) not null, `batch` int not null) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "create table `users` (`id` bigint unsigned not null auto_increment primary key, `email` varchar(255) not null, `balance` decimal(10, 2) not null default '0', `updated_at` timestamp null) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "insert into `cache` values ('laravel-cache-counter', 'i:1;', 100), ('laravel-cache-a', 'x', 1), ('laravel-cache-b', 'y', 1), ('Laravel-Cache-C', 'z', 1)",
        "insert into `cache_locks` values ('laravel-cache-report', 'first', 5)",
        "insert into `migrations` (`migration`, `batch`) values ('0001_01_01_000000_create_users_table', 1), ('2026_02_01_000000_add_slug', 2)",
        "insert into `users` (`email`) values ('alice@example.com'), ('bob@example.com')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }

    assert_eq!(
        affected(
            &mut adapter,
            "update `cache` set `value` = ? where `key` = ?",
            &[Bound::Word("i:3;"), Bound::Word("LARAVEL-CACHE-COUNTER  ")],
        ),
        1
    );
    assert_eq!(
        affected(
            &mut adapter,
            "delete from `cache` where `key` in (?, ?)",
            &[
                Bound::Word("laravel-cache-a"),
                Bound::Word("laravel-cache-c")
            ],
        ),
        2
    );
    assert_eq!(
        words_of(
            &mut adapter,
            "select `key`, `value` from `cache` order by `key`"
        ),
        [
            [word("laravel-cache-b"), word("y")],
            [word("laravel-cache-counter"), word("i:3;")],
        ]
    );

    // Laravel takes a lock another owner let lapse, or its own again.
    let take_the_lock = "update `cache_locks` set `owner` = ?, `expiration` = ? where `key` = ? and (`owner` = ? or `expiration` <= ?)";
    assert_eq!(
        affected(
            &mut adapter,
            take_the_lock,
            &[
                Bound::Word("second"),
                Bound::Number(20),
                Bound::Word("laravel-cache-report"),
                Bound::Word("second"),
                Bound::Number(4),
            ],
        ),
        0
    );
    assert_eq!(
        affected(
            &mut adapter,
            take_the_lock,
            &[
                Bound::Word("second"),
                Bound::Number(20),
                Bound::Word("laravel-cache-report"),
                Bound::Word("second"),
                Bound::Number(5),
            ],
        ),
        1
    );
    assert_eq!(
        affected(
            &mut adapter,
            "delete from `cache_locks` where `key` = ? and `owner` = ?",
            &[Bound::Word("laravel-cache-report"), Bound::Word("second")],
        ),
        1
    );

    assert_eq!(
        affected(
            &mut adapter,
            "delete from `migrations` where `migration` = ?",
            &[Bound::Word("2026_02_01_000000_add_slug")],
        ),
        1
    );
    assert_eq!(
        affected(
            &mut adapter,
            "update `users` set `balance` = ?, `users`.`updated_at` = ? where `email` = ?",
            &[
                Bound::Word("0"),
                Bound::Word("2026-09-28 01:45:19"),
                Bound::Word("Alice@Example.com"),
            ],
        ),
        1
    );
    assert_eq!(
        words_of(
            &mut adapter,
            "select `email`, `balance`, `updated_at` from `users` order by `id`"
        ),
        [
            [
                word("alice@example.com"),
                word("0.00"),
                word("2026-09-28 01:45:19")
            ],
            [word("bob@example.com"), word("0.00"), None],
        ]
    );
}

/// A number bound against a column of words is refused, prepared, in a
/// `SELECT` as in a write, and inside a subquery of either. Measured on MySQL 8.4.11: MySQL compares it with the
/// number each word begins with — `name = 0` finds `'abc'` and `name = 5`
/// finds `'5x'` — where the engine would compare it as the word it spells and
/// find neither.
#[test]
fn a_number_bound_against_a_column_of_words_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "create table `w` (`id` int primary key, `name` varchar(20))",
        "create table `v` (`id` int primary key, `name` varchar(20))",
        "insert into `w` values (1, 'abc'), (2, '5x')",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    for sql in [
        "select `id` from `w` where `name` = ?",
        "select exists(select * from `w` where `name` = ?) as `exists`",
        "update `v` set `id` = `id` where exists (select 1 from `w` where `w`.`name` = ?)",
        "update `w` set `id` = `id` where `name` = ?",
        "delete from `w` where `name` = ?",
    ] {
        let statement = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(
            adapter.execute_stmt_execute(statement.statement_id, &bound(&[Bound::Number(0)])),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
        assert!(
            adapter
                .execute_stmt_execute(statement.statement_id, &bound(&[Bound::Word("abc")]))
                .is_ok(),
            "{sql}"
        );
    }
    assert_eq!(
        words_of(&mut adapter, "select `id` from `w` order by `id`"),
        [[word("2")]]
    );
}

/// An `UPDATE` of a table with an `ON UPDATE CURRENT_TIMESTAMP` column
/// rewrites it when any assigned column changes. Each `?` of the `SET` is the
/// one compared with its own column's value: measured on MySQL 8.4.11, `SET a
/// = ?, b = ?` bound 1 and 5 over a row holding 1 and 1 moves the moment.
#[test]
fn every_bound_value_of_a_set_is_compared_with_its_own_column() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "create table `t` (`id` int primary key, `a` int, `b` int, `updated_at` timestamp not null default current_timestamp on update current_timestamp)",
        "insert into `t` values (1, 1, 1, '2000-01-01 00:00:00')",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    assert_eq!(
        affected(
            &mut adapter,
            "update `t` set `a` = ?, `b` = ? where `id` = ?",
            &[Bound::Number(1), Bound::Number(5), Bound::Number(1)],
        ),
        1
    );
    assert_eq!(
        words_of(
            &mut adapter,
            "select `a`, `b`, `updated_at` > '2000-01-01 00:00:00' from `t`"
        ),
        [[word("1"), word("5"), word("1")]]
    );
}

/// Laravel's `->exists()`, `firstOrCreate`, `updateOrCreate` and the `unique`
/// validation rule ask whether a row is there with a subquery comparing a
/// column of words with a bound word. Measured on MySQL 8.4.11 over
/// `utf8mb4_unicode_ci`: the answer is a NOT NULL `LONGLONG` of 1, binary,
/// and `'third '` finds `Third`.
#[test]
fn laravel_asks_whether_a_row_exists_by_a_bound_word() {
    let (_directory, mut adapter) = adapter();
    adapter.execute_query(LARAVEL_OPENS_WITH).unwrap();
    for sql in [
        "create table `posts` (`id` bigint unsigned not null auto_increment primary key, `title` varchar(255) not null, `views` int not null default '0') default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "insert into `posts` (`title`) values ('Hello'), ('Third')",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let exists = "select exists(select * from `posts` where `title` = ?) as `exists`";
    for (title, found) in [("third ", 1), ("nope", 0)] {
        let PreparedStatementExecutionResult::ResultSet(result) =
            prepared(&mut adapter, exists, &bound(&[Bound::Word(title)]))
        else {
            panic!("{exists} must answer a row");
        };
        let [column] = result.columns.as_slice() else {
            panic!("{exists} must answer one column");
        };
        assert_eq!(
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.character_set,
                column.flags
            ),
            (
                "exists",
                MYSQL_TYPE_LONGLONG,
                1,
                63,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            )
        );
        assert_eq!(
            result.rows,
            [[BinaryResultValue::Integer(found)]],
            "{title}"
        );
    }
    let statement = adapter.execute_stmt_prepare(exists).unwrap();
    assert_eq!(
        adapter.execute_stmt_execute(statement.statement_id, &bound(&[Bound::Number(0)])),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// Laravel's database queue takes its next job with `FOR UPDATE SKIP LOCKED`,
/// prepared, inside a transaction. Measured on MySQL 8.4.11 with no other
/// session holding a row, it answers the job as `FOR UPDATE` does.
#[test]
fn laravel_takes_its_next_job_skipping_locked_rows() {
    let (_directory, mut adapter) = adapter();
    adapter.execute_query(LARAVEL_OPENS_WITH).unwrap();
    for sql in [
        "create table `jobs` (`id` bigint unsigned not null auto_increment primary key, `queue` varchar(255) not null, `attempts` tinyint unsigned not null, `reserved_at` int unsigned null, `available_at` int unsigned not null) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "insert into `jobs` (`queue`, `attempts`, `available_at`) values ('default', 0, 1790561610)",
        "START TRANSACTION",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let PreparedStatementExecutionResult::ResultSet(job) = prepared(
        &mut adapter,
        "select * from `jobs` where `queue` = ? and ((`reserved_at` is null and `available_at` <= ?) or (`reserved_at` <= ?)) order by `id` asc limit 1 FOR UPDATE SKIP LOCKED",
        &bound(&[
            Bound::Word("default"),
            Bound::Number(1790561610),
            Bound::Number(1790561520),
        ]),
    ) else {
        panic!("the queue must answer rows");
    };
    assert_eq!(
        job.rows,
        [[
            BinaryResultValue::UnsignedInteger(1),
            BinaryResultValue::Text("default".to_owned()),
            BinaryResultValue::Integer(0),
            BinaryResultValue::Null,
            BinaryResultValue::Integer(1790561610),
        ]]
    );
    adapter.execute_query("COMMIT").unwrap();
}

/// Every key Laravel makes is a `bigint unsigned`, and Eloquent updates and
/// deletes a row by it, binding the id as a number: `update ... where id =
/// ?`, `delete from posts where id = ?`, and a pivot's detach, `delete from
/// post_tag where post_tag.post_id = ? and post_tag.tag_id in (?)`. Each
/// matched no row, the engine comparing the number with the unsigned column's
/// own stored form; MySQL finds the row.
#[test]
fn laravel_writes_a_row_found_by_its_unsigned_id() {
    let (_directory, mut adapter) = adapter();
    adapter.execute_query(LARAVEL_OPENS_WITH).unwrap();
    for sql in [
        "create table `jobs` (`id` bigint unsigned not null auto_increment primary key, `queue` varchar(255) not null, `attempts` tinyint unsigned not null, `reserved_at` int unsigned null) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "create table `post_tag` (`post_id` bigint unsigned not null, `tag_id` bigint unsigned not null, primary key (`post_id`, `tag_id`)) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "insert into `jobs` (`queue`, `attempts`) values ('default', 0), ('default', 0)",
        "insert into `post_tag` values (2, 2), (2, 3), (3, 3)",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    assert_eq!(
        affected(
            &mut adapter,
            "update `jobs` set `reserved_at` = ?, `attempts` = ? where `id` = ?",
            &[
                Bound::Number(1790561610),
                Bound::Number(1),
                Bound::Number(2)
            ],
        ),
        1
    );
    assert_eq!(
        words_of(
            &mut adapter,
            "select `id`, `attempts`, `reserved_at` from `jobs` order by `id`"
        ),
        [
            [word("1"), word("0"), None],
            [word("2"), word("1"), word("1790561610")]
        ]
    );
    assert_eq!(
        affected(
            &mut adapter,
            "delete from `jobs` where `id` = ?",
            &[Bound::Number(1)],
        ),
        1
    );
    assert_eq!(
        affected(
            &mut adapter,
            "delete from `post_tag` where `post_tag`.`post_id` = ? and `post_tag`.`tag_id` in (?)",
            &[Bound::Number(2), Bound::Number(2)],
        ),
        1
    );
    assert_eq!(
        words_of(
            &mut adapter,
            "select `post_id`, `tag_id` from `post_tag` order by `post_id`, `tag_id`"
        ),
        [[word("2"), word("3")], [word("3"), word("3")]]
    );
}

/// Laravel writes every member of a JSON path quoted — `select('profile->city
/// as city')` is `json_unquote(json_extract(profile, '$."city"'))` and a JSON
/// update `json_set(profile, '$."city"', ?)` — which names what `$.city`
/// names. Measured on MySQL 8.4.11, over both protocols: the reading answers
/// what the bare path answers, in the same column, a `LONG_BLOB` of
/// 4294967295 unquoted and a `JSON` of 4294967292 not; and the update changes
/// the member the bare path changes.
#[test]
fn laravel_reads_and_changes_a_json_member_named_in_quotes() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "create table `users` (`id` bigint unsigned not null auto_increment primary key, `email` varchar(255) not null, `profile` json null)",
        "insert into `users` (`email`, `profile`) values ('a', '{\"city\": \"Tokyo\", \"tags\": [\"a\", \"b\"], \"n\": null}'), ('b', null)",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let city = "select `email`, json_unquote(json_extract(`profile`, '$.\"city\"')) as `city` from `users` where `profile` is not null";
    let Ok(CommandExecutionResult::ResultSet(text)) = adapter.execute_query(city) else {
        panic!("{city} must answer rows");
    };
    assert_eq!(text.rows, [[Some(b"a".to_vec()), Some(b"Tokyo".to_vec())]]);
    assert_eq!(
        (
            text.columns[1].column_type,
            text.columns[1].column_length,
            text.columns[1].flags,
            text.columns[1].decimals
        ),
        (
            MYSQL_TYPE_LONG_BLOB,
            4_294_967_295,
            MYSQL_BINARY_FLAG,
            NOT_FIXED_DECIMALS
        )
    );
    let PreparedStatementExecutionResult::ResultSet(binary) = prepared(&mut adapter, city, &[])
    else {
        panic!("{city} must answer rows prepared");
    };
    assert_eq!(binary.columns, text.columns);
    assert_eq!(
        binary.rows[0][1],
        BinaryResultValue::Blob(b"Tokyo".to_vec())
    );
    for (sql, answer, length) in [
        (
            "select json_extract(`profile`, '$.\"tags\"[1]') from `users` order by `id`",
            ["\"b\"", "NULL"],
            4_294_967_292,
        ),
        (
            "select `profile`->'$.\"n\"' from `users` order by `id`",
            ["null", "NULL"],
            4_294_967_292,
        ),
        (
            "select `profile`->>'$.\"city\"' from `users` order by `id`",
            ["Tokyo", "NULL"],
            4_294_967_295,
        ),
    ] {
        let Ok(CommandExecutionResult::ResultSet(read)) = adapter.execute_query(sql) else {
            panic!("{sql} must answer rows");
        };
        assert_eq!(
            read.rows
                .iter()
                .map(|row| row[0]
                    .as_ref()
                    .map_or("NULL".to_owned(), |value| String::from_utf8(value.clone())
                        .unwrap()))
                .collect::<Vec<_>>(),
            answer,
            "{sql}"
        );
        assert_eq!(read.columns[0].column_length, length, "{sql}");
    }

    assert_eq!(
        adapter
            .execute_query(
                "update `users` set `profile` = json_set(`profile`, '$.\"city\"', 'Kyoto') where `email` = 'a'"
            )
            .map(|result| match result {
                CommandExecutionResult::Ok(ok) => ok.affected_rows,
                CommandExecutionResult::ResultSet(_) => panic!("an update answers OK"),
            }),
        Ok(1)
    );
    assert_eq!(
        words_of(
            &mut adapter,
            "select `profile` from `users` where `email` = 'a'"
        ),
        [[word(
            "{\"n\": null, \"city\": \"Kyoto\", \"tags\": [\"a\", \"b\"]}"
        )]]
    );
    // A quoted name holding anything but letters, digits and underscores, and
    // a wildcard, are not paths the engine and MySQL were measured to agree
    // on: the arrow over one answered a column of no type.
    for sql in [
        "select json_extract(`profile`, '$.\"first name\"') from `users`",
        "select `profile`->>'$.\"first name\"' as `x` from `users`",
        "select `profile`->'$.*' as `x` from `users`",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
