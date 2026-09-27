//! Tables declared `utf8mb4_unicode_ci`, the collation Laravel and Prisma
//! declare every table with, and the connection Laravel opens with it.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

fn connect(
    factory: AuthorizedDatabaseAdapterFactory<RecordingAuthorizer>,
    account: u8,
) -> AuthorizedDatabaseCommandAdapter<RecordingAuthorizer> {
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([account; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
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

fn text(value: &str) -> Option<String> {
    Some(value.to_owned())
}

#[test]
fn laravel_connects_and_migrates_under_utf8mb4_unicode_ci() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = connect(factory, 131);
    assert_eq!(adapter.connection_collation(), 45);
    run(
        &mut adapter,
        "SET NAMES 'utf8mb4' COLLATE 'utf8mb4_unicode_ci', SESSION sql_mode='ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION'",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT @@collation_connection"),
        [[text("utf8mb4_unicode_ci")]]
    );
    // Every text column of a result reports the connection's collation.
    assert_eq!(adapter.connection_collation(), 224);

    for sql in [
        "create table `users` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null, `email` varchar(255) not null, `remember_token` varchar(100) null, `created_at` timestamp null, `updated_at` timestamp null) default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
        "alter table `users` add unique `users_email_unique`(`email`)",
        // A column an ALTER adds takes the table's collation too, and one
        // placed after another rewrites the whole table.
        "alter table `users` add `nick` varchar(20) null after `name`",
        "insert into `users` (`name`, `email`) values ('xₐy', 'ann@example.com')",
        "create table `plain` (`id` int primary key, `name` varchar(10))",
        "insert into `plain` values (1, 'xₐy')",
    ] {
        run(&mut adapter, sql);
    }
    const USERS: &str = "CREATE TABLE `users` (\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n  `name` varchar(255) COLLATE utf8mb4_unicode_ci NOT NULL,\n  `nick` varchar(20) COLLATE utf8mb4_unicode_ci DEFAULT NULL,\n  `email` varchar(255) COLLATE utf8mb4_unicode_ci NOT NULL,\n  `remember_token` varchar(100) COLLATE utf8mb4_unicode_ci DEFAULT NULL,\n  `created_at` timestamp NULL DEFAULT NULL,\n  `updated_at` timestamp NULL DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `users_email_unique` (`email`)\n) ENGINE=InnoDB AUTO_INCREMENT=2 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci";
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TABLE `users`"),
        [[text("users"), text(USERS)]]
    );

    // PAD SPACE, where utf8mb4_0900_ai_ci compares the trailing spaces.
    assert_eq!(
        adapter.execute_query(
            "insert into `users` (`name`, `email`) values ('B', 'ANN@EXAMPLE.COM ')"
        ),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "select `id` from `users` where `email` = 'ann@example.com  '"
        ),
        [[text("1")]]
    );
    // U+2090 weighs the same as `a` under Unicode 9 and has no weight of its
    // own in Unicode 4.0.0, so the two collations disagree about it.
    assert!(rows(
        &mut adapter,
        "select `id` from `users` where `name` like 'xa%'"
    )
    .is_empty());
    assert_eq!(
        rows(
            &mut adapter,
            "select `id` from `users` where `name` like 'X_Y'"
        ),
        [[text("1")]]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "select `id` from `plain` where `name` like 'xa%'"
        ),
        [[text("1")]]
    );
    // These compare under utf8mb4_0900_ai_ci's weights, where MySQL compares
    // a call's answer under the collation of the column it read.
    for sql in [
        "select field(`name`, 'xay') from `users`",
        "select `id` from `users` order by concat(`name`, 'x'), `id`",
        "select `id` from `users` where lower(`email`) = 'ann@example.com'",
        "update `users` set `nick` = 'a' where lower(`email`) = 'ann@example.com'",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert_eq!(
        rows(
            &mut adapter,
            "select `id` from `plain` order by concat(`name`, 'x'), `id`"
        ),
        [[text("1")]]
    );

    assert_eq!(
        rows(
            &mut adapter,
            "select table_name, table_collation from information_schema.tables where table_schema = database() and table_name in ('users', 'plain') order by table_name"
        ),
        [
            [text("plain"), text("utf8mb4_0900_ai_ci")],
            [text("users"), text("utf8mb4_unicode_ci")],
        ]
    );
    let columns = rows(&mut adapter, "SHOW FULL COLUMNS FROM `users`");
    assert_eq!(columns[2][0], text("nick"));
    assert_eq!(columns[2][2], text("utf8mb4_unicode_ci"));
    assert_eq!(columns[5][2], None);

    // Another connection reads the table back the way it was stored. The
    // refused row took a number, as it does in MySQL.
    let mut other = connect(
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer),
        132,
    );
    assert_eq!(
        rows(&mut other, "SHOW CREATE TABLE `users`"),
        [[
            text("users"),
            Some(USERS.replace("AUTO_INCREMENT=2", "AUTO_INCREMENT=3"))
        ]]
    );
    assert_eq!(
        other
            .execute_query("insert into `users` (`name`, `email`) values ('C', 'Ann@Example.com')"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
}

/// Prisma keys its bookkeeping table on a `VARCHAR`, so the key itself is
/// compared under the table's collation.
#[test]
fn prismas_migrations_table_keys_its_rows_under_utf8mb4_unicode_ci() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = connect(factory, 133);
    run(
        &mut adapter,
        "CREATE TABLE _prisma_migrations (\n    id                      VARCHAR(36) PRIMARY KEY NOT NULL,\n    checksum                VARCHAR(64) NOT NULL,\n    finished_at             DATETIME(3),\n    migration_name          VARCHAR(255) NOT NULL,\n    logs                    TEXT,\n    rolled_back_at          DATETIME(3),\n    started_at              DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),\n    applied_steps_count     INTEGER UNSIGNED NOT NULL DEFAULT 0\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;\n",
    );
    run(
        &mut adapter,
        "insert into _prisma_migrations (id, checksum, migration_name) values ('abc', 'x', 'm')",
    );
    assert_eq!(
        adapter.execute_query(
            "insert into _prisma_migrations (id, checksum, migration_name) values ('ABC ', 'x', 'm')"
        ),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TABLE _prisma_migrations"),
        [[
            text("_prisma_migrations"),
            text("CREATE TABLE `_prisma_migrations` (\n  `id` varchar(36) COLLATE utf8mb4_unicode_ci NOT NULL,\n  `checksum` varchar(64) COLLATE utf8mb4_unicode_ci NOT NULL,\n  `finished_at` datetime(3) DEFAULT NULL,\n  `migration_name` varchar(255) COLLATE utf8mb4_unicode_ci NOT NULL,\n  `logs` text COLLATE utf8mb4_unicode_ci,\n  `rolled_back_at` datetime(3) DEFAULT NULL,\n  `started_at` datetime(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),\n  `applied_steps_count` int unsigned NOT NULL DEFAULT '0',\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"),
        ]]
    );
    let status = rows(&mut adapter, "SHOW TABLE STATUS LIKE '_prisma%'");
    assert_eq!(status[0][14], text("utf8mb4_unicode_ci"));
}
