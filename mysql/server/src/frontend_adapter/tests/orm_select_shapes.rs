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

/// Rails' `group(:user_id).having("COUNT(*) > ?", 1).count` writes the bound
/// number as a word. Measured on MySQL 8.4.11: a count against a word
/// compares the two as doubles, so `'1'` is 1 and says nothing, while
/// `'1abc'` warns 1292 and `'1.5'` is not a whole number.
#[test]
fn rails_compares_a_count_with_a_word_naming_a_whole_number() {
    let (_directory, mut adapter) = adapter();
    let grouped = result_set(
        &mut adapter,
        "SELECT COUNT(*) AS `count_all`, `posts`.`user_id` AS `posts_user_id` FROM `posts` GROUP BY `posts`.`user_id` HAVING (COUNT(*) > '1')",
    );
    assert_eq!(
        text_rows(&grouped),
        [[Some("2".to_owned()), Some("1".to_owned())]]
    );
    assert_eq!(grouped.warnings, 0);
    assert_eq!(
        text_rows(&result_set(
            &mut adapter,
            "SELECT user_id FROM posts GROUP BY user_id HAVING COUNT(*) = '02'"
        )),
        [[Some("1".to_owned())]]
    );
    for sql in [
        "SELECT user_id FROM posts GROUP BY user_id HAVING COUNT(*) > '1abc'",
        "SELECT user_id FROM posts GROUP BY user_id HAVING COUNT(*) > '1.5'",
        "SELECT user_id FROM posts GROUP BY user_id HAVING COUNT(*) > ' 1'",
        "SELECT user_id FROM posts GROUP BY user_id HAVING MAX(id) > '1'",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// The mysql command-line run's report of how old each row is, and the
/// first run's `SELECT 2 > 1` and `SELECT (SELECT COUNT(*) FROM posts) > 0`.
/// Measured on MySQL 8.4.11: each comparison answers a `LONGLONG` of length 1
/// with the binary and numeric flags, NOT NULL where nothing it reads can be
/// null. A count of days or units between two moments, a shifted reading of
/// the clock and a subquery's count are reported nullable whatever they read.
#[test]
fn a_comparison_over_the_clock_a_count_or_numbers_is_a_result_column() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE events (id INT NOT NULL PRIMARY KEY, created_at DATETIME NOT NULL, deleted_at DATETIME NULL)",
        "INSERT INTO events VALUES (1, '2026-01-02 03:04:05', NULL), (2, '2099-01-01 00:00:00', '2026-01-01 00:00:00')",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let nullable = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let not_null = MYSQL_NOT_NULL_FLAG | nullable;
    let dates = result_set(
        &mut adapter,
        "SELECT DATE_FORMAT(created_at, '%Y-%m'), DATE_SUB(NOW(), INTERVAL 1 DAY) < created_at,\n  TIMESTAMPDIFF(DAY, created_at, NOW()) >= 0 FROM events ORDER BY id",
    );
    assert_eq!(
        text_rows(&dates),
        [["2026-01", "0", "1"], ["2099-01", "1", "0"]].map(|row| row.map(|v| Some(v.to_owned())))
    );
    assert_eq!(
        shapes(&dates)[1..],
        [
            (
                "DATE_SUB(NOW(), INTERVAL 1 DAY) < created_at",
                MYSQL_TYPE_LONGLONG,
                1,
                0,
                nullable
            ),
            (
                "TIMESTAMPDIFF(DAY, created_at, NOW()) >= 0",
                MYSQL_TYPE_LONGLONG,
                1,
                0,
                nullable
            ),
        ]
    );
    let null_moments = result_set(
        &mut adapter,
        "SELECT deleted_at > DATE_SUB(NOW(), INTERVAL 1 DAY), DATEDIFF(NOW(), deleted_at) > 1, NOW() > deleted_at, NOW() > created_at FROM events ORDER BY id",
    );
    assert_eq!(
        text_rows(&null_moments),
        [
            [None, None, None, Some("1".to_owned())],
            ["0", "1", "1", "0"].map(|v| Some(v.to_owned())),
        ]
    );
    assert_eq!(
        shapes(&null_moments)
            .into_iter()
            .map(|(_, _, _, _, flags)| flags)
            .collect::<Vec<_>>(),
        [nullable, nullable, nullable, not_null]
    );
    for (sql, flags) in [
        ("SELECT 2 > 1", not_null),
        ("SELECT (SELECT COUNT(*) FROM posts) > 0", nullable),
    ] {
        let answered = result_set(&mut adapter, sql);
        assert_eq!(text_rows(&answered), [[Some("1".to_owned())]], "{sql}");
        assert_eq!(
            shapes(&answered),
            [(&sql["SELECT ".len()..], MYSQL_TYPE_LONGLONG, 1, 0, flags)],
            "{sql}"
        );
    }
    let statement = adapter
        .execute_stmt_prepare(
            "SELECT DATE_SUB(NOW(), INTERVAL 1 DAY) < created_at, NOW() > deleted_at FROM events ORDER BY id",
        )
        .unwrap();
    let PreparedStatementExecutionResult::ResultSet(prepared) = adapter
        .execute_stmt_execute(statement.statement_id, &[])
        .unwrap()
    else {
        panic!("the comparisons must answer rows");
    };
    assert_eq!(
        prepared.rows,
        [
            vec![BinaryResultValue::Integer(0), BinaryResultValue::Null],
            vec![BinaryResultValue::Integer(1), BinaryResultValue::Integer(1)],
        ]
    );
    assert_eq!(
        prepared
            .columns
            .iter()
            .map(|column| (column.column_type, column.flags))
            .collect::<Vec<_>>(),
        [
            (MYSQL_TYPE_LONGLONG, nullable),
            (MYSQL_TYPE_LONGLONG, nullable)
        ]
    );
    // Not measured, or held by the WHERE reader to a column of another kind.
    for sql in [
        "SELECT -1 < 0",
        "SELECT CURDATE() <= created_at FROM events",
        "SELECT (SELECT MAX(id) FROM posts) > 0",
        "SELECT TIMESTAMPDIFF(DAY, created_at, NOW()) >= id FROM events",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Rails' `relation.limit(n).offset(m).count`. Measured on MySQL 8.4.11: the
/// count of the rows the limit leaves, in the shape a plain `COUNT(*)`
/// answers — 3 of 3 posts under `LIMIT 3 OFFSET 0`, and 1 under `LIMIT 2
/// OFFSET 2`.
#[test]
fn rails_counts_the_rows_a_limit_leaves_in_a_derived_table() {
    let (_directory, mut adapter) = adapter();
    for (sql, count) in [
        (
            "SELECT COUNT(*) FROM (SELECT 1 AS one FROM `posts` LIMIT 3 OFFSET 0) subquery_for_count",
            "3",
        ),
        (
            "SELECT COUNT(*) FROM (SELECT 1 AS one FROM `posts` LIMIT 2 OFFSET 2) subquery_for_count",
            "1",
        ),
        (
            "SELECT COUNT(*) FROM (SELECT 1 AS one FROM `posts` WHERE `posts`.`user_id` = 1 LIMIT 5) subquery_for_count",
            "2",
        ),
    ] {
        let counted = result_set(&mut adapter, sql);
        assert_eq!(text_rows(&counted), [[Some(count.to_owned())]], "{sql}");
        assert_eq!(
            shapes(&counted),
            [(
                "COUNT(*)",
                MYSQL_TYPE_LONGLONG,
                21,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            )],
            "{sql}"
        );
    }
    // Something reads the body's columns, or an order could name what MySQL
    // refuses: each is held to what a derived table is held to.
    for sql in [
        "SELECT COUNT(one) FROM (SELECT 1 AS one FROM posts LIMIT 1) AS s",
        "SELECT COUNT(*), 1 FROM (SELECT 1 AS one FROM posts LIMIT 1) AS s",
        "SELECT COUNT(*) FROM (SELECT 1 AS one FROM posts ORDER BY id LIMIT 1) AS s",
        "SELECT COUNT(*) FROM (SELECT 1 AS one FROM posts LIMIT 1) AS s WHERE one = 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// TypeORM's `clearDatabase` and `dropDatabase` list the views to drop by
/// writing each statement out of `information_schema.VIEWS`. Measured on
/// MySQL 8.4.11: the answer is a `VAR_STRING` as wide as each word and column
/// counted four bytes to a character — 612 and 580 here — with 31 decimals
/// and the binary flag the catalog's words carry, and nullable.
#[test]
fn typeorm_writes_a_statement_out_of_the_catalog_s_words() {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("typeorm_owner"));
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("typeorm").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([144; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("typeorm").unwrap();
    let drops = "SELECT concat('DROP VIEW IF EXISTS `', table_schema, '`.`', table_name, '`') AS `query` FROM `INFORMATION_SCHEMA`.`VIEWS` WHERE `TABLE_SCHEMA` = 'typeorm'";
    let creates = "SELECT concat('CREATE VIEW ', table_schema, '.', table_name, ' AS ') AS `query` FROM `INFORMATION_SCHEMA`.`VIEWS` WHERE `TABLE_SCHEMA` = 'typeorm'";
    let none = result_set(&mut adapter, drops);
    assert!(none.rows.is_empty());
    assert_eq!(
        shapes(&none),
        [("query", MYSQL_TYPE_VAR_STRING, 612, 31, MYSQL_BINARY_FLAG)]
    );
    for sql in [
        "CREATE TABLE posts (id INT NOT NULL PRIMARY KEY, title VARCHAR(20) NOT NULL)",
        "CREATE VIEW v_posts AS SELECT id, title FROM posts",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let dropped = result_set(&mut adapter, drops);
    assert_eq!(
        text_rows(&dropped),
        [[Some("DROP VIEW IF EXISTS `typeorm`.`v_posts`".to_owned())]]
    );
    assert_eq!(shapes(&dropped), shapes(&none));
    let created = result_set(&mut adapter, creates);
    assert_eq!(
        text_rows(&created),
        [[Some("CREATE VIEW typeorm.v_posts AS ".to_owned())]]
    );
    assert_eq!(
        shapes(&created),
        [("query", MYSQL_TYPE_VAR_STRING, 580, 31, MYSQL_BINARY_FLAG)]
    );
    // Measured: over `VIEW_DEFINITION`, a `LONGTEXT`, the answer is a
    // `MEDIUM_BLOB` by a rule not taken here.
    assert!(adapter
        .execute_query(
            "SELECT CONCAT('x', view_definition) FROM information_schema.VIEWS WHERE table_schema = 'typeorm'"
        )
        .is_err());
}

/// Prisma counts each user's posts in a derived table joined on the outer
/// side of a `LEFT JOIN`. Measured on MySQL 8.4.11: a user with no posts
/// reads NULL there, and the count is reported nullable — a `LONGLONG` of 21
/// naming the derived table and no original table — where the same count
/// read straight out of the derived table is NOT NULL.
#[test]
fn a_count_on_the_outer_side_of_a_left_join_is_nullable() {
    let (_directory, mut adapter) = adapter();
    let joined = result_set(
        &mut adapter,
        "SELECT `users`.`id`, `aggr_selection_0_Post`.`_aggr_count_posts` FROM `users` LEFT JOIN (SELECT `posts`.`user_id`, COUNT(*) AS `_aggr_count_posts` FROM `posts` WHERE 1=1 GROUP BY `posts`.`user_id`) AS `aggr_selection_0_Post` ON (`users`.`id` = `aggr_selection_0_Post`.`user_id`) WHERE 1=1 ORDER BY `users`.`id` ASC",
    );
    assert_eq!(
        text_rows(&joined),
        [
            [Some("1".to_owned()), Some("2".to_owned())],
            [Some("2".to_owned()), None],
            [Some("3".to_owned()), Some("1".to_owned())],
        ]
    );
    let count = &joined.columns[1];
    assert_eq!(
        (
            count.column_type,
            count.column_length,
            count.flags & MYSQL_NOT_NULL_FLAG,
            count.table.as_str(),
            count.original_table.as_str()
        ),
        (MYSQL_TYPE_LONGLONG, 21, 0, "aggr_selection_0_Post", "")
    );
    // Prisma writes the count as `COALESCE(..., 0)`, which MySQL answers NOT
    // NULL; a fallback over a derived table's column is not read here.
    assert!(adapter
        .execute_query(
            "SELECT `users`.`id`, COALESCE(`aggr_selection_0_Post`.`_aggr_count_posts`, 0) AS `_aggr_count_posts` FROM `users` LEFT JOIN (SELECT `posts`.`user_id`, COUNT(*) AS `_aggr_count_posts` FROM `posts` WHERE 1=1 GROUP BY `posts`.`user_id`) AS `aggr_selection_0_Post` ON (`users`.`id` = `aggr_selection_0_Post`.`user_id`) WHERE 1=1 ORDER BY `users`.`id` ASC"
        )
        .is_err());
}

/// The mysql command-line run's pagination: `SELECT SQL_CALC_FOUND_ROWS id,
/// title FROM posts ORDER BY id LIMIT 2 OFFSET 1; SELECT FOUND_ROWS()`.
/// Measured on MySQL 8.4.11: `FOUND_ROWS()` answers the rows the statement
/// answers without its `LIMIT` — its groups, its distinct rows, and all of
/// them past an offset beyond the last — and the statement warns 1287.
#[test]
fn found_rows_counts_what_a_limit_left_out_after_sql_calc_found_rows() {
    let (_directory, mut adapter) = adapter();
    for (sql, rows, found) in [
        (
            "SELECT SQL_CALC_FOUND_ROWS id, title FROM posts ORDER BY id LIMIT 2 OFFSET 1",
            vec![["2", "Second"], ["3", "Carols post"]],
            "3",
        ),
        (
            "SELECT SQL_CALC_FOUND_ROWS id, title FROM posts ORDER BY id LIMIT 2 OFFSET 5",
            Vec::new(),
            "3",
        ),
        (
            "SELECT SQL_CALC_FOUND_ROWS id, title FROM posts ORDER BY id LIMIT 1, 1",
            vec![["2", "Second"]],
            "3",
        ),
        (
            "SELECT SQL_CALC_FOUND_ROWS id, title FROM posts WHERE id > 1 ORDER BY id",
            vec![["2", "Second"], ["3", "Carols post"]],
            "2",
        ),
    ] {
        let answered = arrived(&mut adapter, sql);
        assert_eq!(
            text_rows(&answered),
            rows.iter()
                .map(|row| row.map(|value| Some(value.to_owned())).to_vec())
                .collect::<Vec<_>>(),
            "{sql}"
        );
        assert_eq!(answered.warnings, 1, "{sql}");
        assert_eq!(
            text_rows(&arrived(&mut adapter, "SHOW WARNINGS")),
            [[
                Some("Warning".to_owned()),
                Some("1287".to_owned()),
                Some("SQL_CALC_FOUND_ROWS is deprecated and will be removed in a future release. Consider using two separate queries instead.".to_owned()),
            ]],
            "{sql}"
        );
        arrived(&mut adapter, sql);
        assert_eq!(
            text_rows(&arrived(&mut adapter, "SELECT FOUND_ROWS()")),
            [[Some(found.to_owned())]],
            "{sql}"
        );
    }
    for (sql, found) in [
        (
            "SELECT SQL_CALC_FOUND_ROWS user_id, COUNT(*) FROM posts GROUP BY user_id ORDER BY user_id LIMIT 1",
            "2",
        ),
        (
            "SELECT SQL_CALC_FOUND_ROWS DISTINCT user_id FROM posts ORDER BY user_id LIMIT 1",
            "2",
        ),
    ] {
        arrived(&mut adapter, sql);
        assert_eq!(
            text_rows(&arrived(&mut adapter, "SELECT FOUND_ROWS()")),
            [[Some(found.to_owned())]],
            "{sql}"
        );
    }
    // Without the modifier, the rows answered.
    arrived(&mut adapter, "SELECT id FROM posts LIMIT 1");
    assert_eq!(
        text_rows(&arrived(&mut adapter, "SELECT FOUND_ROWS()")),
        [[Some("1".to_owned())]]
    );
    // MySQL answers 1234 for it in a subquery. A bound value, a `WITH` and a
    // prepared statement are refused: the count reads the statement a
    // second time, and a prepared one is not read back.
    for sql in [
        "SELECT id FROM posts WHERE id IN (SELECT SQL_CALC_FOUND_ROWS id FROM posts)",
        "SELECT SQL_CALC_FOUND_ROWS id FROM posts UNION SELECT id FROM users",
        "WITH p AS (SELECT id FROM posts) SELECT SQL_CALC_FOUND_ROWS id FROM p LIMIT 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert!(adapter
        .execute_stmt_prepare("SELECT SQL_CALC_FOUND_ROWS id FROM posts LIMIT 1")
        .is_err());
}

/// Runs one statement the way a `COM_QUERY` carrying it reaches the session.
fn arrived(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> TextResultSet {
    adapter.command_arrived(ArrivedCommand::Query);
    result_set(adapter, sql)
}
