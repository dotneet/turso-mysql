//! Reads the ORMs write that were refused: TypeORM's counts and query-builder
//! reads, Django's and Rails' aggregates, each replayed as the framework
//! harness saw it on the wire.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([143; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, email VARCHAR(191) NOT NULL, name VARCHAR(100) NOT NULL, balance DECIMAL(10,2) NOT NULL DEFAULT '0.00', is_active TINYINT(1) NOT NULL DEFAULT 1, profile JSON NULL, created_at DATETIME NOT NULL, PRIMARY KEY (id), UNIQUE KEY users_email_unique (email))",
        "CREATE TABLE posts (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, user_id BIGINT UNSIGNED NOT NULL, title VARCHAR(200) NOT NULL, PRIMARY KEY (id))",
        "INSERT INTO users (email, name, balance, is_active, profile, created_at) VALUES ('alice@example.com', 'Alice', 120.50, 1, '{\"city\": \"Oslo\"}', '2026-01-02 03:04:05')",
        "INSERT INTO users (email, name, balance, is_active, profile, created_at) VALUES ('bob@example.com', 'Bob', 0.00, 0, NULL, '2026-02-03 04:05:06')",
        "INSERT INTO users (email, name, balance, is_active, profile, created_at) VALUES ('carol@example.com', 'Carol', 99999.99, 1, '{\"city\": \"Lima\"}', '2026-09-20 00:00:00')",
        "INSERT INTO posts (user_id, title) VALUES (1, 'Hello'), (1, 'Second'), (3, 'Carols post')",
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

fn text_rows(result: &TextResultSet) -> Vec<Vec<Option<String>>> {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| {
                    value
                        .as_ref()
                        .map(|value| String::from_utf8(value.clone()).unwrap())
                })
                .collect()
        })
        .collect()
}

fn shapes(result: &TextResultSet) -> Vec<(&str, u8, u32, u8, u16)> {
    result
        .columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
            )
        })
        .collect()
}

/// TypeORM's `Repository.count` and `findAndCount`. Measured on MySQL 8.4.11:
/// `COUNT(1)` and `COUNT(0)` count every row and answer the shape `COUNT(*)`
/// answers, a NOT NULL `LONGLONG` of 21 with the binary and numeric flags.
#[test]
fn typeorm_counts_every_row_with_a_written_number() {
    let (_directory, mut adapter) = adapter();
    let counted = result_set(&mut adapter, "SELECT COUNT(1) AS `cnt` FROM `posts` `Post`");
    assert_eq!(text_rows(&counted), [[Some("3".to_owned())]]);
    assert_eq!(
        shapes(&counted),
        [(
            "cnt",
            MYSQL_TYPE_LONGLONG,
            21,
            0,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        )]
    );
    assert_eq!(
        text_rows(&result_set(
            &mut adapter,
            "SELECT COUNT(1) AS `cnt` FROM `posts` `Post` WHERE ((`Post`.`user_id` = '1'))"
        )),
        [[Some("2".to_owned())]]
    );
    let named = result_set(
        &mut adapter,
        "SELECT COUNT(1), COUNT(0) FROM posts WHERE id > 100",
    );
    assert_eq!(
        text_rows(&named),
        [[Some("0".to_owned()), Some("0".to_owned())]]
    );
    assert_eq!(
        named
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["COUNT(1)", "COUNT(0)"]
    );
    let statement = adapter
        .execute_stmt_prepare("SELECT COUNT(1) AS `cnt` FROM `posts` `Post`")
        .unwrap();
    let PreparedStatementExecutionResult::ResultSet(prepared) = adapter
        .execute_stmt_execute(statement.statement_id, &[])
        .unwrap()
    else {
        panic!("the count must answer a row");
    };
    assert_eq!(prepared.rows, [vec![BinaryResultValue::Integer(3)]]);
    assert_eq!(prepared.columns, counted.columns);
    // A written NULL counts nothing, and has not been measured beside a
    // grouping; a word or a fraction has not been needed.
    for sql in [
        "SELECT COUNT(NULL) FROM posts",
        "SELECT COUNT('x') FROM posts",
        "SELECT COUNT(1.5) FROM posts",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// TypeORM's `getRawMany` over a grouped query-builder read and Django's
/// aggregate, each column written with its table or its alias. Measured on
/// MySQL 8.4.11: each answers what the bare column answers, value and shape,
/// under the name written.
#[test]
fn an_aggregate_over_the_one_tables_qualified_column_reads_the_bare_column() {
    let (_directory, mut adapter) = adapter();
    for (qualified, bare) in [
        (
            "SELECT `user`.`is_active` AS `active`, COUNT(*) AS `count`, SUM(`user`.`balance`) AS `total`, AVG(`user`.`balance`) AS `average`, MAX(`user`.`created_at`) AS `latest` FROM `users` `user` GROUP BY `user`.`is_active` HAVING COUNT(*) > 0 ORDER BY `user`.`is_active` ASC",
            "SELECT `is_active` AS `active`, COUNT(*) AS `count`, SUM(`balance`) AS `total`, AVG(`balance`) AS `average`, MAX(`created_at`) AS `latest` FROM `users` `user` GROUP BY `is_active` HAVING COUNT(*) > 0 ORDER BY `is_active` ASC",
        ),
        (
            "SELECT COUNT(`users`.`id`) AS `n`, SUM(`users`.`balance`) AS `total`, AVG(`users`.`balance`) AS `avg` FROM `users`",
            "SELECT COUNT(`id`) AS `n`, SUM(`balance`) AS `total`, AVG(`balance`) AS `avg` FROM `users`",
        ),
    ] {
        let (qualified, bare) = (
            result_set(&mut adapter, qualified),
            result_set(&mut adapter, bare),
        );
        assert_eq!(qualified.rows, bare.rows);
        assert_eq!(shapes(&qualified), shapes(&bare));
    }
    let grouped = result_set(
        &mut adapter,
        "SELECT `user`.`is_active` AS `active`, COUNT(*) AS `count`, SUM(`user`.`balance`) AS `total`, AVG(`user`.`balance`) AS `average`, MAX(`user`.`created_at`) AS `latest` FROM `users` `user` GROUP BY `user`.`is_active` HAVING COUNT(*) > 0 ORDER BY `user`.`is_active` ASC",
    );
    assert_eq!(
        text_rows(&grouped),
        [
            ["0", "1", "0.00", "0.000000", "2026-02-03 04:05:06"].map(|v| Some(v.to_owned())),
            ["1", "2", "100120.49", "50060.245000", "2026-09-20 00:00:00"]
                .map(|v| Some(v.to_owned())),
        ]
    );
    let named = result_set(
        &mut adapter,
        "SELECT SUM(u.balance), AVG(u.balance), MIN(u.name), MAX(u.created_at), MIN(u.id) FROM users u",
    );
    assert_eq!(
        shapes(&named)
            .into_iter()
            .map(|(name, column_type, length, decimals, _)| (name, column_type, length, decimals))
            .collect::<Vec<_>>(),
        [
            ("SUM(u.balance)", MYSQL_TYPE_NEWDECIMAL, 34, 2),
            ("AVG(u.balance)", MYSQL_TYPE_NEWDECIMAL, 16, 6),
            ("MIN(u.name)", MYSQL_TYPE_VAR_STRING, 400, 0),
            ("MAX(u.created_at)", MYSQL_TYPE_DATETIME, 19, 0),
            ("MIN(u.id)", MYSQL_TYPE_LONGLONG, 20, 0),
        ]
    );
    assert_eq!(
        text_rows(&named),
        [[
            "100120.49",
            "33373.496667",
            "Alice",
            "2026-09-20 00:00:00",
            "1"
        ]
        .map(|v| Some(v.to_owned()))]
    );
}

/// TypeORM's query builder filtering on a JSON member, the column written with
/// its alias.
#[test]
fn a_json_reading_of_the_one_tables_qualified_column_reads_the_bare_column() {
    let (_directory, mut adapter) = adapter();
    let found = result_set(
        &mut adapter,
        "SELECT `user`.`id` AS `user_id`, `user`.`email` AS `user_email` FROM `users` `user` WHERE JSON_UNQUOTE(JSON_EXTRACT(`user`.`profile`, '$.city')) = 'Oslo'",
    );
    assert_eq!(
        text_rows(&found),
        [[Some("1".to_owned()), Some("alice@example.com".to_owned())]]
    );
    assert_eq!(found.columns[0].table, "user");
}

/// What a qualifier could make read differently stays refused.
#[test]
fn a_qualified_aggregate_that_could_name_another_table_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // A join: the qualifier says which table's column is read.
        "SELECT SUM(u.balance) FROM users u JOIN posts p ON p.user_id = u.id",
        // A name that is no column of the table, which MySQL answers 1054
        // for, and the engine would read as the projection's alias.
        "SELECT balance AS spent, SUM(u.spent) FROM users u GROUP BY balance",
        "SELECT id AS n FROM users u WHERE JSON_EXTRACT(u.n, '$.a') = 1",
        // A name that is not the table's.
        "SELECT SUM(p.balance) FROM users u",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
