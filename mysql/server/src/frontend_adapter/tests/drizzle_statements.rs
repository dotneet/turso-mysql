//! Statements Drizzle ORM 0.45 and drizzle-kit 0.31 send over mysql2,
//! replayed as the framework harness's sixth run captured them.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([191; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn result_set(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    result_set(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn refused(adapter: &mut Adapter, sql: &str) {
    assert!(
        matches!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported)
        ),
        "{sql} must be refused"
    );
}

/// Drizzle checks its connection with `select 1 from dual`, which answers
/// what `select 1` answers, written or prepared.
#[test]
fn drizzles_connection_check_reads_from_dual() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "select 1 from dual",
        "SELECT 1 FROM DUAL",
        "select 1 from dual where 1=0",
        "select 1 from dual order by 1",
    ] {
        let result = result_set(&mut adapter, sql);
        let [column] = result.columns.as_slice() else {
            panic!("{sql} must answer one column");
        };
        assert_eq!(column.name, "1", "{sql}");
        assert_eq!(column.column_type, MYSQL_TYPE_LONGLONG, "{sql}");
        assert_eq!(column.column_length, 2, "{sql}");
        // mysql2 reads 0x81 off the wire; the `NUM` the mysql client prints
        // beside these is its own, added for a numeric type.
        assert_eq!(column.flags, 0x81, "{sql}: NOT_NULL BINARY");
    }
    assert_eq!(
        rows(&mut adapter, "select 1 from dual"),
        [[Some("1".to_owned())]]
    );
    assert!(rows(&mut adapter, "select 1 from dual where 1=0").is_empty());
    assert_eq!(
        rows(&mut adapter, "SELECT 'a' as x, 2+3 FROM DUAL"),
        [[Some("a".to_owned()), Some("5".to_owned())]]
    );

    let statement = adapter.execute_stmt_prepare("select 1 from dual").unwrap();
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(statement.statement_id, &[])
        .unwrap()
    else {
        panic!("the prepared read must answer rows");
    };
    assert_eq!(result.rows, [[BinaryResultValue::Integer(1)]]);

    // A quoted `dual` is a table like any other, which this one is not.
    refused(&mut adapter, "select 1 from `dual`");
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn created(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE `{table}`"))[0][1]
        .clone()
        .unwrap()
}

/// The table drizzle-kit keeps its migrations in, as it creates it before
/// every `migrate`.
const MIGRATIONS_TABLE: &str = "\n\t\t\tcreate table if not exists `__drizzle_migrations` (\n\t\t\t\tid serial primary key,\n\t\t\t\thash text not null,\n\t\t\t\tcreated_at bigint\n\t\t\t)\n\t\t";

/// drizzle-kit's first migration, one statement per breakpoint as it sends
/// them.
const FIRST_MIGRATION: [&str; 8] = [
    "CREATE TABLE `users` (\n\t`id` bigint unsigned AUTO_INCREMENT NOT NULL,\n\t`email` varchar(191) NOT NULL,\n\t`name` varchar(100) NOT NULL,\n\t`balance` decimal(10,2) NOT NULL DEFAULT '0.00',\n\t`is_active` boolean NOT NULL DEFAULT true,\n\t`profile` json,\n\t`created_at` timestamp NOT NULL DEFAULT (now()),\n\t`updated_at` timestamp NOT NULL DEFAULT (now()) ON UPDATE CURRENT_TIMESTAMP,\n\tCONSTRAINT `users_id` PRIMARY KEY(`id`),\n\tCONSTRAINT `users_email_unique` UNIQUE(`email`)\n);\n",
    "\nCREATE TABLE `posts` (\n\t`id` bigint unsigned AUTO_INCREMENT NOT NULL,\n\t`user_id` bigint unsigned NOT NULL,\n\t`title` varchar(200) NOT NULL,\n\t`body` text,\n\t`published_at` datetime,\n\t`views` int NOT NULL DEFAULT 0,\n\tCONSTRAINT `posts_id` PRIMARY KEY(`id`)\n);\n",
    "\nCREATE TABLE `tags` (\n\t`id` bigint unsigned AUTO_INCREMENT NOT NULL,\n\t`name` varchar(100) NOT NULL,\n\tCONSTRAINT `tags_id` PRIMARY KEY(`id`),\n\tCONSTRAINT `tags_name_unique` UNIQUE(`name`)\n);\n",
    "\nCREATE TABLE `post_tags` (\n\t`post_id` bigint unsigned NOT NULL,\n\t`tag_id` bigint unsigned NOT NULL,\n\tCONSTRAINT `post_tags_post_id_tag_id` PRIMARY KEY(`post_id`,`tag_id`)\n);\n",
    "\nALTER TABLE `posts` ADD CONSTRAINT `posts_user_id_users_id_fk` FOREIGN KEY (`user_id`) REFERENCES `users`(`id`) ON DELETE cascade ON UPDATE no action;",
    "\nALTER TABLE `post_tags` ADD CONSTRAINT `post_tags_post_id_posts_id_fk` FOREIGN KEY (`post_id`) REFERENCES `posts`(`id`) ON DELETE cascade ON UPDATE no action;",
    "\nALTER TABLE `post_tags` ADD CONSTRAINT `post_tags_tag_id_tags_id_fk` FOREIGN KEY (`tag_id`) REFERENCES `tags`(`id`) ON DELETE cascade ON UPDATE no action;",
    "\nCREATE INDEX `posts_user_published` ON `posts` (`user_id`,`published_at`);",
];

/// `drizzle-kit migrate` creates its own table with `id serial primary key`
/// and then runs the first migration in one transaction, naming every key.
/// MySQL keeps no `CONSTRAINT` name for a primary key, and makes `SERIAL` a
/// `BIGINT UNSIGNED` counter with a unique key of its own beside the primary
/// one.
#[test]
fn drizzle_kit_migrates_its_first_schema() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, MIGRATIONS_TABLE);
    assert!(rows(
        &mut adapter,
        "select id, hash, created_at from `__drizzle_migrations` order by created_at desc limit 1"
    )
    .is_empty());
    run(&mut adapter, "begin");
    for sql in FIRST_MIGRATION {
        run(&mut adapter, sql);
    }
    run(
        &mut adapter,
        "insert into `__drizzle_migrations` (`hash`, `created_at`) values('e734eba371a03050afb19b3f332c9e3f0684eb8623264dfc8832e183379c61ed', 1790583068207)",
    );
    run(&mut adapter, "commit");
    // Run again, the table is there and is left as it is.
    run(&mut adapter, MIGRATIONS_TABLE);

    assert_eq!(
        created(&mut adapter, "__drizzle_migrations"),
        "CREATE TABLE `__drizzle_migrations` (\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n  `hash` text NOT NULL,\n  `created_at` bigint DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `id` (`id`)\n) ENGINE=InnoDB AUTO_INCREMENT=2 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        created(&mut adapter, "post_tags"),
        "CREATE TABLE `post_tags` (\n  `post_id` bigint unsigned NOT NULL,\n  `tag_id` bigint unsigned NOT NULL,\n  PRIMARY KEY (`post_id`,`tag_id`),\n  KEY `post_tags_tag_id_tags_id_fk` (`tag_id`),\n  CONSTRAINT `post_tags_post_id_posts_id_fk` FOREIGN KEY (`post_id`) REFERENCES `posts` (`id`) ON DELETE CASCADE,\n  CONSTRAINT `post_tags_tag_id_tags_id_fk` FOREIGN KEY (`tag_id`) REFERENCES `tags` (`id`) ON DELETE CASCADE\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        created(&mut adapter, "users"),
        "CREATE TABLE `users` (\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n  `email` varchar(191) NOT NULL,\n  `name` varchar(100) NOT NULL,\n  `balance` decimal(10,2) NOT NULL DEFAULT '0.00',\n  `is_active` tinyint(1) NOT NULL DEFAULT '1',\n  `profile` json DEFAULT NULL,\n  `created_at` timestamp NOT NULL DEFAULT (now()),\n  `updated_at` timestamp NOT NULL DEFAULT (now()) ON UPDATE CURRENT_TIMESTAMP,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `users_email_unique` (`email`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, CONSTRAINT_NAME, CONSTRAINT_TYPE FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_SCHEMA='reports' AND TABLE_NAME IN ('__drizzle_migrations', 'post_tags') ORDER BY TABLE_NAME, CONSTRAINT_NAME"
        ),
        [
            ["__drizzle_migrations", "id", "UNIQUE"],
            ["__drizzle_migrations", "PRIMARY", "PRIMARY KEY"],
            ["post_tags", "post_tags_post_id_posts_id_fk", "FOREIGN KEY"],
            ["post_tags", "post_tags_tag_id_tags_id_fk", "FOREIGN KEY"],
            ["post_tags", "PRIMARY", "PRIMARY KEY"],
        ]
        .map(|row| row.map(|value| Some(value.to_owned())))
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(*) AS n FROM __drizzle_migrations"
        ),
        [[Some("1".to_owned())]]
    );
}

/// `SERIAL` is written out only where a column is declared with it: a column
/// named `serial` keeps its name, and a counter with no primary key beside it
/// is refused as the long spelling is.
#[test]
fn serial_is_only_a_column_type() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "create table s (`serial` int, id SERIAL primary key)",
    );
    assert_eq!(
        created(&mut adapter, "s"),
        "CREATE TABLE `s` (\n  `serial` int DEFAULT NULL,\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `id` (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    refused(&mut adapter, "create table t (id serial, n int)");
}

/// The affected rows and the id one write reports.
fn written(adapter: &mut Adapter, sql: &str) -> (u64, u64) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => (result.affected_rows, result.last_insert_id),
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

/// Drizzle writes `default` for each column a row leaves to its default, so
/// one statement of several rows gives a column `DEFAULT` in some rows and a
/// value in others. Measured on MySQL 8.4.11: each row takes the column's
/// default where it asks for it, the ids count on from the statement's first,
/// which is the id it reports, and a row that fails leaves none of the others
/// written while the numbers they asked for are spent.
#[test]
fn drizzles_rows_give_default_to_different_columns() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `posts` (`id` bigint unsigned AUTO_INCREMENT NOT NULL, `user_id` bigint unsigned NOT NULL, `title` varchar(200) NOT NULL, `body` text, `published_at` datetime, `views` int NOT NULL DEFAULT 0, CONSTRAINT `posts_id` PRIMARY KEY(`id`), UNIQUE KEY (title))",
    );
    assert_eq!(
        written(
            &mut adapter,
            "insert into `posts` (`id`, `user_id`, `title`, `body`, `published_at`, `views`) values (default, 1, 'Hello', 'First post', '2024-01-02 03:04:05.000', 10), (default, 1, 'Draft', 'Not yet', default, 0), (default, 2, 'Bob writes', default, default, 3)",
        ),
        (3, 1)
    );
    assert!(matches!(
        adapter.execute_query(
            "insert into `posts` (`id`, `user_id`, `title`, `body`, `published_at`, `views`) values (default, 1, 'A', default, default, 7), (default, 1, 'Hello', 'x', default, default)",
        ),
        Err(FrontendErrorKind::ConstraintViolation)
    ));
    assert_eq!(
        written(
            &mut adapter,
            "insert into `posts` (`id`, `user_id`, `title`, `body`, `published_at`, `views`) values (default, 1, 'B', default, default, 7), (default, 1, 'C', 'x', default, default)",
        ),
        (2, 6)
    );
    let text = |value: &str| Some(value.to_owned());
    assert_eq!(
        rows(&mut adapter, "select * from posts order by id"),
        [
            [
                text("1"),
                text("1"),
                text("Hello"),
                text("First post"),
                text("2024-01-02 03:04:05"),
                text("10")
            ],
            [
                text("2"),
                text("1"),
                text("Draft"),
                text("Not yet"),
                None,
                text("0")
            ],
            [
                text("3"),
                text("2"),
                text("Bob writes"),
                None,
                None,
                text("3")
            ],
            [text("6"), text("1"), text("B"), None, None, text("7")],
            [text("7"), text("1"), text("C"), text("x"), None, text("0")],
        ]
    );
}

/// Drizzle writes a moment as `'2024-01-02 03:04:05.000'`, which a
/// `DATETIME` keeps as `'2024-01-02 03:04:05'`. The index over the column
/// has to hold what the row holds: before, it held the value as written, so
/// the row could not be deleted (the engine found the index out of step with
/// the table) and a unique key let the same moment in twice. Measured on
/// MySQL 8.4.11, each statement here answers as it does below.
#[test]
fn an_index_holds_the_value_its_row_stores() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE p1 (id INT PRIMARY KEY, user_id INT, published_at DATETIME)",
    );
    run(
        &mut adapter,
        "CREATE INDEX p1_pub ON p1 (user_id, published_at)",
    );
    run(
        &mut adapter,
        "insert into p1 values (1, 1, '2024-01-02 03:04:05.000')",
    );
    run(
        &mut adapter,
        "update p1 set published_at = '2024-01-02 03:04:06.000' where id = 1",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "select * from p1 where user_id = 1 and published_at = '2024-01-02 03:04:06'"
        ),
        [["1", "1", "2024-01-02 03:04:06"].map(|value| Some(value.to_owned()))]
    );
    assert_eq!(written(&mut adapter, "delete from p1"), (1, 0));

    run(
        &mut adapter,
        "CREATE TABLE u1 (id INT PRIMARY KEY, at DATETIME, c CHAR(4), UNIQUE KEY (at), UNIQUE KEY (c))",
    );
    run(
        &mut adapter,
        "insert into u1 values (1, '2024-01-02 03:04:05', 'ab')",
    );
    // 1062: the same moment, and the same word once a CHAR drops its spaces.
    for sql in [
        "insert into u1 values (2, '2024-01-02 03:04:05.000', 'x')",
        "insert into u1 values (3, '2024-01-02 03:04:07', 'ab  ')",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::ConstraintViolation)
            ),
            "{sql}"
        );
    }
    run(
        &mut adapter,
        "insert into u1 values (4, '2024-01-02 03:04:08', 'cd  ')",
    );
    assert_eq!(
        written(
            &mut adapter,
            "insert into u1 values (1, '2024-01-02 03:04:09.000', 'q') on duplicate key update at = '2024-01-02 03:04:09.000'",
        ),
        (2, 0)
    );
    assert_eq!(
        rows(&mut adapter, "select id, at from u1 order by id"),
        [
            [Some("1".to_owned()), Some("2024-01-02 03:04:09".to_owned())],
            [Some("4".to_owned()), Some("2024-01-02 03:04:08".to_owned())],
        ]
    );
    assert_eq!(written(&mut adapter, "delete from u1"), (2, 0));
}

/// The affected rows, the id and the warnings one write reports.
fn written_with_warnings(adapter: &mut Adapter, sql: &str) -> (u64, u64, u16) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => {
            (result.affected_rows, result.last_insert_id, result.warnings)
        }
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

/// Drizzle qualifies every column it writes, inside `VALUES()` of an upsert
/// and inside a call in an `UPDATE` alike. Measured on MySQL 8.4.11, each
/// names what the bare column names there, and each `VALUES()` still warns
/// 1287.
#[test]
fn drizzles_qualified_columns_name_the_table_written() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `tags` (`id` bigint unsigned AUTO_INCREMENT NOT NULL, `name` varchar(100) NOT NULL, CONSTRAINT `tags_id` PRIMARY KEY(`id`), CONSTRAINT `tags_name_unique` UNIQUE(`name`))",
    );
    run(
        &mut adapter,
        "insert into `tags` (`id`, `name`) values (default, 'news'), (default, 'rust'), (default, 'sql')",
    );
    assert_eq!(
        written_with_warnings(
            &mut adapter,
            "insert into `tags` (`id`, `name`) values (default, 'go'), (default, 'news') on duplicate key update `name` = values(`tags`.`name`)",
        ),
        (1, 4, 1)
    );
    assert_eq!(
        rows(&mut adapter, "select id, name from tags order by id"),
        [["1", "news"], ["2", "rust"], ["3", "sql"], ["4", "go"]]
            .map(|row| row.map(|value| Some(value.to_owned())))
    );

    run(
        &mut adapter,
        "CREATE TABLE `users` (`id` bigint unsigned AUTO_INCREMENT NOT NULL, `email` varchar(191) NOT NULL, `name` varchar(100) NOT NULL, `balance` decimal(10,2) NOT NULL DEFAULT '0.00', `is_active` boolean NOT NULL DEFAULT true, `profile` json, CONSTRAINT `users_id` PRIMARY KEY(`id`), CONSTRAINT `users_email_unique` UNIQUE(`email`))",
    );
    run(
        &mut adapter,
        "insert into `users` (`id`, `email`, `name`, `balance`, `is_active`, `profile`) values (default, 'alice@example.com', 'Alice', '100.50', default, '{\"city\":\"Tokyo\",\"tags\":[\"a\",\"b\"]}'), (default, 'bob@example.com', 'Bob', '20.25', false, '{\"city\":\"Osaka\",\"tags\":[\"c\"]}')",
    );
    assert_eq!(
        written(
            &mut adapter,
            "update `users` set `profile` = JSON_SET(`users`.`profile`, '$.city', 'Kyoto') where `users`.`id` = 2",
        ),
        (1, 0)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "select `profile`->>'$.city' from `users` where `users`.`id` = 2"
        ),
        [[Some("Kyoto".to_owned())]]
    );
    assert_eq!(
        written_with_warnings(
            &mut adapter,
            "insert into `users` (`id`, `email`, `name`, `balance`, `is_active`, `profile`) values (default, 'alice@example.com', 'Alice Upserted', '100.00', default, default), (default, 'erin@example.com', 'Erin', default, default, default) on duplicate key update `name` = values(`users`.`name`), `balance` = values(`users`.`balance`)",
        )
        .0,
        3
    );
    let text = |value: &str| Some(value.to_owned());
    assert_eq!(
        rows(
            &mut adapter,
            "select id, email, name, balance, is_active, profile from users order by id"
        ),
        [
            [
                text("1"),
                text("alice@example.com"),
                text("Alice Upserted"),
                text("100.00"),
                text("1"),
                text("{\"city\": \"Tokyo\", \"tags\": [\"a\", \"b\"]}")
            ],
            [
                text("2"),
                text("bob@example.com"),
                text("Bob"),
                text("20.25"),
                text("0"),
                text("{\"city\": \"Kyoto\", \"tags\": [\"c\"]}")
            ],
            [
                text("3"),
                text("erin@example.com"),
                text("Erin"),
                text("0.00"),
                text("1"),
                None
            ],
        ]
    );
    // Another table's name is 1054 in MySQL, and refused here.
    assert!(adapter
        .execute_query(
            "insert into `tags` (`id`, `name`) values (default, 'go') on duplicate key update `name` = values(`other`.`name`)"
        )
        .is_err());
}

/// drizzle-kit reads a database's indexes naming each column with the
/// schema's and the table's names in front. Measured on MySQL 8.4.11 with
/// `lower_case_table_names=1`, as this server reports, the names match
/// whatever their case, written or prepared.
#[test]
fn drizzle_kit_reads_indexes_with_qualified_columns() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `tags` (`id` int NOT NULL, `name` varchar(10), CONSTRAINT `tags_id` PRIMARY KEY(`id`), CONSTRAINT `tags_name_unique` UNIQUE(`name`))",
    );
    let unique_key =
        ["tags", "tags_name_unique", "0", "1", "name"].map(|value| Some(value.to_owned()));
    for sql in [
        "select TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME from INFORMATION_SCHEMA.STATISTICS WHERE INFORMATION_SCHEMA.STATISTICS.TABLE_SCHEMA = 'reports' and INFORMATION_SCHEMA.STATISTICS.INDEX_NAME != 'PRIMARY'",
        "select TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME from INFORMATION_SCHEMA.STATISTICS WHERE STATISTICS.TABLE_SCHEMA = 'reports' and statistics.INDEX_NAME != 'PRIMARY'",
        "select TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME from INFORMATION_SCHEMA.STATISTICS WHERE TABLE_SCHEMA = 'reports' and INDEX_NAME != 'PRIMARY' ORDER BY STATISTICS.INDEX_NAME",
    ] {
        assert_eq!(rows(&mut adapter, sql), [unique_key.clone()], "{sql}");
    }
    let drizzle_kit_read = "select * from INFORMATION_SCHEMA.STATISTICS WHERE INFORMATION_SCHEMA.STATISTICS.TABLE_SCHEMA = 'reports' and INFORMATION_SCHEMA.STATISTICS.INDEX_NAME != 'PRIMARY'";
    let result = result_set(&mut adapter, drizzle_kit_read);
    assert_eq!(result.columns.len(), 18);
    assert_eq!(result.rows.len(), 1);

    let statement = adapter.execute_stmt_prepare(drizzle_kit_read).unwrap();
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(statement.statement_id, &[])
        .unwrap()
    else {
        panic!("the prepared read must answer rows");
    };
    assert_eq!(result.rows.len(), 1);
}
