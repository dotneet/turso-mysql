//! JSON documents built out of a row — `JSON_OBJECT('id', id, 'name', name)`,
//! the `JSON_ARRAYAGG` around it that nests every row into one answer, and a
//! value put into a document by `JSON_SET` — and how each column's value is
//! written into the document.
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
            AccountId::from_bytes([153; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100))",
        "INSERT INTO users (name) VALUES ('ann'), ('bob')",
        "CREATE TABLE posts (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, user_id BIGINT UNSIGNED, title VARCHAR(200) NOT NULL)",
        "INSERT INTO posts (user_id, title) VALUES (1, 'first'), (1, 'second'), (2, 'third')",
        "CREATE TABLE kinds (id BIGINT UNSIGNED PRIMARY KEY, d0 DECIMAL(10,0), d2 DECIMAL(10,2), u INT UNSIGNED, dt DATETIME, d3 DATETIME(3), dd DATE, f DOUBLE, b TINYINT(1), y YEAR, e ENUM('a','b'), js JSON, ts TIMESTAMP NULL, t TIME, fl FLOAT)",
        "INSERT INTO kinds VALUES (18446744073709551615, 5, 1.50, 7, '2026-01-02 03:04:05', '2026-01-02 03:04:05.123', '2026-01-02', 1.5, 1, 2026, 'b', '{\"a\": 1}', '2026-01-02 03:04:05', '03:04:05', 1.5), (1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn result(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

/// The first column of every row, with NULL written as `NULL`.
fn column(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    result(adapter, sql)
        .rows
        .into_iter()
        .map(|row| match &row[0] {
            Some(value) => String::from_utf8(value.clone()).unwrap(),
            None => "NULL".to_owned(),
        })
        .collect()
}

fn assert_reports_json(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) {
    let text = result(adapter, sql);
    let column = text.columns.last().unwrap();
    assert_eq!(
        (
            column.column_type,
            column.column_length,
            column.decimals,
            column.flags
        ),
        (
            MYSQL_TYPE_JSON,
            u32::MAX - 3,
            NOT_FIXED_DECIMALS,
            MYSQL_BINARY_FLAG
        ),
        "{sql}"
    );
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns, "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
}

#[test]
fn a_document_built_from_an_unsigned_key_writes_the_key_as_a_number() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT JSON_OBJECT('id', id, 'name', name) FROM users ORDER BY id";
    assert_eq!(
        column(&mut adapter, sql),
        [r#"{"id": 1, "name": "ann"}"#, r#"{"id": 2, "name": "bob"}"#]
    );
    assert_reports_json(&mut adapter, sql);
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_OBJECT('id', id, 'd0', d0, 'u', u) FROM kinds ORDER BY id"
        ),
        [
            r#"{"u": null, "d0": null, "id": 1}"#,
            r#"{"u": 7, "d0": 5, "id": 18446744073709551615}"#
        ]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_ARRAY(id, d0) FROM kinds ORDER BY id"
        ),
        ["[1, null]", "[18446744073709551615, 5]"]
    );
    // A number written with a point keeps the places it was written with,
    // which a double writes back while none past the first is a trailing zero.
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_ARRAY(1.5, 0.1, 1.0, .5, 123456789.123, -0.5, 100.0)"
        ),
        ["[1.5, 0.1, 1.0, 0.5, 123456789.123, -0.5, 100.0]"]
    );
}

/// Measured: a moment is written with six places of a second whatever its
/// column keeps, and a `JSON` column as the document it holds.
#[test]
fn a_moment_or_a_document_is_written_into_a_document_the_way_mysql_writes_it() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_OBJECT('dt', dt, 'd3', d3, 'dd', dd, 'f', f, 'b', b, 'y', y, 'e', e, 'js', js) FROM kinds ORDER BY id"
        ),
        [
            r#"{"b": null, "e": null, "f": null, "y": null, "d3": null, "dd": null, "dt": null, "js": null}"#,
            r#"{"b": 1, "e": "b", "f": 1.5, "y": 2026, "d3": "2026-01-02 03:04:05.123000", "dd": "2026-01-02", "dt": "2026-01-02 03:04:05.000000", "js": {"a": 1}}"#
        ]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_SET(js, '$.b', d3), JSON_SET(js, '$.b', js), JSON_INSERT(js, '$.c', id) FROM kinds WHERE id = 18446744073709551615"
        ),
        [r#"{"a": 1, "b": "2026-01-02 03:04:05.123000"}"#]
    );
    let row = &result(
        &mut adapter,
        "SELECT JSON_SET(js, '$.b', d3), JSON_SET(js, '$.b', js), JSON_INSERT(js, '$.c', id) FROM kinds WHERE id = 18446744073709551615",
    )
    .rows[0];
    let texts: Vec<_> = row
        .iter()
        .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            r#"{"a": 1, "b": "2026-01-02 03:04:05.123000"}"#,
            r#"{"a": 1, "b": {"a": 1}}"#,
            r#"{"a": 1, "c": 18446744073709551615}"#
        ]
    );
}

/// Measured: a `DECIMAL` with places and a number written with a point keep
/// every place in the document — `1.50`, `10.00` — where the engine writes a
/// double's shortest digits, and a `TIME` and a `TIMESTAMP` are
/// written by rules of their own; a `FLOAT` has not been measured.
#[test]
fn a_value_written_into_a_document_by_a_rule_not_followed_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT JSON_OBJECT('d2', d2) FROM kinds",
        "SELECT JSON_ARRAY(d2) FROM kinds",
        "SELECT JSON_OBJECT('n', 1.50)",
        "SELECT JSON_ARRAY(10.00)",
        "SELECT JSON_ARRAY(1234567890.1234567)",
        "SELECT JSON_OBJECT('t', t) FROM kinds",
        "SELECT JSON_OBJECT('ts', ts) FROM kinds",
        "SELECT JSON_OBJECT('fl', fl) FROM kinds",
        "SELECT JSON_SET(js, '$.b', d2) FROM kinds",
        "SELECT JSON_SET(js, '$.b', t) FROM kinds",
        "SELECT JSON_SET(js, '$.b', 1.50) FROM kinds",
        "SELECT JSON_INSERT(js, '$.b', ts) FROM kinds",
        "SELECT JSON_ARRAYAGG(JSON_OBJECT('t', t)) FROM kinds",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

#[test]
fn json_arrayagg_nests_the_document_built_from_each_row() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT JSON_ARRAYAGG(JSON_OBJECT('id', id, 'name', name)) FROM users";
    assert_eq!(
        column(&mut adapter, sql),
        [r#"[{"id": 1, "name": "ann"}, {"id": 2, "name": "bob"}]"#]
    );
    assert_reports_json(&mut adapter, sql);
    // Grouped, each group's rows go into its own array.
    let sql = "SELECT user_id, JSON_ARRAYAGG(JSON_OBJECT('id', id, 'title', title)) FROM posts GROUP BY user_id ORDER BY user_id";
    let rows: Vec<_> = result(&mut adapter, sql)
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
        .collect();
    assert_eq!(
        rows,
        [
            r#"[{"id": 1, "title": "first"}, {"id": 2, "title": "second"}]"#,
            r#"[{"id": 3, "title": "third"}]"#
        ]
    );
    assert_reports_json(&mut adapter, sql);
    // Over no rows at all, NULL rather than an empty array.
    assert_eq!(
        column(
            &mut adapter,
            "SELECT JSON_ARRAYAGG(JSON_ARRAY(id)) FROM users WHERE id > 100"
        ),
        ["NULL"]
    );
    // It aggregates the statement, so a bare column beside it is 1140.
    assert!(adapter
        .execute_query("SELECT id, JSON_ARRAYAGG(JSON_OBJECT('id', id)) FROM users")
        .is_err());
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert_eq!(
        binary.rows[1][1],
        BinaryResultValue::Blob(br#"[{"id": 3, "title": "third"}]"#.to_vec())
    );
}

/// Drizzle's blog schema as its first migration writes it, with three users,
/// four posts and three tags. Post 4 belongs to no user. The tags of a post
/// are written in the order of the table's key, which is the order MySQL
/// reads them in; an array of rows read in no order of the statement's holds
/// them in the order the engine reads them, as `JSON_ARRAYAGG` does anywhere.
fn drizzle_adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([154; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE `users` (`id` bigint unsigned AUTO_INCREMENT NOT NULL, `email` varchar(191) NOT NULL, `name` varchar(100) NOT NULL, `balance` decimal(10,2) NOT NULL DEFAULT '0.00', `is_active` boolean NOT NULL DEFAULT true, `profile` json, `created_at` timestamp NOT NULL DEFAULT (now()), `updated_at` timestamp NOT NULL DEFAULT (now()) ON UPDATE CURRENT_TIMESTAMP, CONSTRAINT `users_id` PRIMARY KEY(`id`), CONSTRAINT `users_email_unique` UNIQUE(`email`))",
        "CREATE TABLE `posts` (`id` bigint unsigned AUTO_INCREMENT NOT NULL, `user_id` bigint unsigned NOT NULL, `title` varchar(200) NOT NULL, `body` text, `published_at` datetime, `views` int NOT NULL DEFAULT 0, CONSTRAINT `posts_id` PRIMARY KEY(`id`))",
        "CREATE TABLE `tags` (`id` bigint unsigned AUTO_INCREMENT NOT NULL, `name` varchar(100) NOT NULL, CONSTRAINT `tags_id` PRIMARY KEY(`id`), CONSTRAINT `tags_name_unique` UNIQUE(`name`))",
        "CREATE TABLE `post_tags` (`post_id` bigint unsigned NOT NULL, `tag_id` bigint unsigned NOT NULL, CONSTRAINT `post_tags_post_id_tag_id` PRIMARY KEY(`post_id`,`tag_id`))",
        "CREATE INDEX `posts_user_published` ON `posts` (`user_id`,`published_at`)",
        "INSERT INTO users (email, name, balance, profile) VALUES ('alice@example.com', 'Alice', '100.50', '{\"b\": 1, \"a\": [1, 2]}'), ('bob@example.com', 'Bob', '5.00', NULL), ('carol@example.com', 'Carol', '0.00', NULL)",
        "INSERT INTO tags (name) VALUES ('news'), ('rust'), ('sql')",
        "INSERT INTO posts (user_id, title, body, published_at, views) VALUES (1, 'Hello', 'body \"q\"', '2026-01-02 03:04:05', 1), (1, 'Second', NULL, NULL, 10), (2, 'Bob writes', 'x', NULL, 3), (9, 'Orphan', NULL, NULL, 0)",
        "INSERT INTO post_tags VALUES (1, 1), (1, 2), (3, 3)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// The text of one column of every row, with NULL written as `NULL`.
fn column_at(result: &TextResultSet, place: usize) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| match &row[place] {
            Some(value) => String::from_utf8(value.clone()).unwrap(),
            None => "NULL".to_owned(),
        })
        .collect()
}

/// What a lateral table's column reports: its name, the lateral table, the
/// body's name for it, and its type, length, decimals, flags and character
/// set, with no database and no original table.
fn assert_lateral_column(
    column: &ColumnDefinitionConfig,
    (name, table, original_name): (&str, &str, &str),
    (length, decimals, flags, character_set): (u32, u8, u16, u16),
) {
    assert_eq!(
        (
            column.name.as_str(),
            column.table.as_str(),
            column.original_table.as_str(),
            column.original_name.as_str(),
            column.schema.as_str(),
            column.column_type,
            column.column_length,
            column.decimals,
            column.flags,
            column.character_set,
        ),
        (
            name,
            table,
            "",
            original_name,
            "",
            MYSQL_TYPE_JSON,
            length,
            decimals,
            flags,
            character_set,
        )
    );
}

/// Runs a test on a stack as deep as a connection's: the engine plans a
/// subquery inside another by recursion, and Drizzle nests three.
fn on_a_connections_stack(test: fn()) {
    let finished = std::thread::Builder::new()
        .stack_size(crate::CONNECTION_THREAD_STACK_BYTES)
        .spawn(test)
        .unwrap()
        .join();
    if let Err(panic) = finished {
        std::panic::resume_unwind(panic);
    }
}

/// Drizzle's `findMany` over users with each user's posts, each post's tags
/// and each tag: a lateral table aggregating a numbered derived table, one
/// aggregating a table, and one reading a derived table cut to one row, each
/// inside the last. Measured on MySQL 8.4.11 over the same rows.
#[test]
fn drizzle_relational_queries_read_each_relation_as_one_document() {
    on_a_connections_stack(drizzle_relational_queries);
}

fn drizzle_relational_queries() {
    let (_directory, mut adapter) = drizzle_adapter();
    let sql = "select `users`.`id`, `users`.`email`, `users`.`name`, `users`.`balance`, `users`.`is_active`, `users`.`profile`, `users`.`created_at`, `users`.`updated_at`, `users_posts`.`data` as `posts` from `users` `users` left join lateral (select coalesce(json_arrayagg(json_array(`users_posts`.`id`, `users_posts`.`user_id`, `users_posts`.`title`, `users_posts`.`body`, `users_posts`.`published_at`, `users_posts`.`views`, `users_posts_postTags`.`data`)), json_array()) as `data` from (select *, row_number() over (order by `users_posts`.`id` asc) from `posts` `users_posts` where `users_posts`.`user_id` = `users`.`id`) `users_posts` left join lateral (select coalesce(json_arrayagg(json_array(`users_posts_postTags`.`post_id`, `users_posts_postTags`.`tag_id`, `users_posts_postTags_tag`.`data`)), json_array()) as `data` from `post_tags` `users_posts_postTags` left join lateral (select json_array(`users_posts_postTags_tag`.`id`, `users_posts_postTags_tag`.`name`) as `data` from (select * from `tags` `users_posts_postTags_tag` where `users_posts_postTags_tag`.`id` = `users_posts_postTags`.`tag_id` limit 1) `users_posts_postTags_tag`) `users_posts_postTags_tag` on true where `users_posts_postTags`.`post_id` = `users_posts`.`id`) `users_posts_postTags` on true) `users_posts` on true order by `users`.`id` asc";
    let text = result(&mut adapter, sql);
    assert_eq!(
        column_at(&text, 8),
        [
            r#"[[1, 1, "Hello", "body \"q\"", "2026-01-02 03:04:05.000000", 1, [[1, 1, [1, "news"]], [1, 2, [2, "rust"]]]], [2, 1, "Second", null, null, 10, []]]"#,
            r#"[[3, 2, "Bob writes", "x", null, 3, [[3, 3, [3, "sql"]]]]]"#,
            "[]",
        ]
    );
    assert_lateral_column(
        &text.columns[8],
        ("posts", "users_posts", "data"),
        (
            u32::MAX,
            0,
            MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
            MYSQL_BINARY_COLLATION,
        ),
    );
    // The statement's own columns report what they report read alone.
    let alone = result(
        &mut adapter,
        "select `users`.`id`, `users`.`email`, `users`.`name`, `users`.`balance`, `users`.`is_active`, `users`.`profile`, `users`.`created_at`, `users`.`updated_at` from `users` `users` order by `users`.`id` asc",
    );
    assert_eq!(text.columns[..8], alone.columns[..]);
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns);
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("a result set");
    };
    assert_eq!(binary.rows[2][8], BinaryResultValue::Blob(b"[]".to_vec()));
    adapter.execute_stmt_close(prepared.statement_id);

    // `findFirst` with a relation to one row reads through a derived table
    // cut to one row, and reports what `JSON_ARRAY` reports.
    let sql = "select `posts`.`id`, `posts`.`user_id`, `posts`.`title`, `posts`.`body`, `posts`.`published_at`, `posts`.`views`, `posts_user`.`data` as `user` from `posts` `posts` left join lateral (select json_array(`posts_user`.`name`) as `data` from (select * from `users` `posts_user` where `posts_user`.`id` = `posts`.`user_id` limit 1) `posts_user`) `posts_user` on true where `posts`.`title` = 'Bob writes' limit 1";
    let text = result(&mut adapter, sql);
    assert_eq!(column_at(&text, 6), [r#"["Bob"]"#]);
    assert_lateral_column(
        &text.columns[6],
        ("user", "posts_user", "data"),
        (
            u32::MAX - 3,
            NOT_FIXED_DECIMALS,
            MYSQL_BINARY_FLAG,
            u16::from(DEFAULT_UTF8MB4_COLLATION),
        ),
    );
    // A post whose user is missing answers NULL.
    let text = result(
        &mut adapter,
        "select `posts`.`id`, `posts_user`.`data` as `user` from `posts` `posts` left join lateral (select json_array(`posts_user`.`name`) as `data` from (select * from `users` `posts_user` where `posts_user`.`id` = `posts`.`user_id` limit 1) `posts_user`) `posts_user` on true order by `posts`.`id`",
    );
    assert_eq!(
        column_at(&text, 1),
        [r#"["Alice"]"#, r#"["Alice"]"#, r#"["Bob"]"#, "NULL"]
    );

    // Drizzle's page of posts with their tags, a lateral table aggregating a
    // table it matches by key.
    let sql = "select `posts`.`id`, `posts`.`user_id`, `posts`.`title`, `posts`.`body`, `posts`.`published_at`, `posts`.`views`, `posts_postTags`.`data` as `postTags` from `posts` `posts` left join lateral (select coalesce(json_arrayagg(json_array(`posts_postTags`.`post_id`, `posts_postTags`.`tag_id`)), json_array()) as `data` from `post_tags` `posts_postTags` where `posts_postTags`.`post_id` = `posts`.`id`) `posts_postTags` on true order by `posts`.`id` asc limit 1";
    let text = result(&mut adapter, sql);
    assert_eq!(column_at(&text, 6), ["[[1, 1], [1, 2]]"]);
    assert_lateral_column(
        &text.columns[6],
        ("postTags", "posts_postTags", "data"),
        (
            u32::MAX,
            0,
            MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
            MYSQL_BINARY_COLLATION,
        ),
    );
}

/// Measured on MySQL 8.4.11: the rows of a numbered derived table reach the
/// array in the order the window numbers them, a `LIMIT` keeps the first of
/// them in that order, and an aggregate with no `COALESCE` answers NULL over
/// no rows.
#[test]
fn a_lateral_document_keeps_the_order_its_rows_are_numbered_in() {
    let (_directory, mut adapter) = drizzle_adapter();
    let text = result(
        &mut adapter,
        "SELECT u.id, a.data FROM users u LEFT JOIN LATERAL (SELECT COALESCE(JSON_ARRAYAGG(JSON_ARRAY(p.id, p.title)), JSON_ARRAY()) AS data FROM (SELECT *, ROW_NUMBER() OVER (ORDER BY p.id DESC) FROM posts p WHERE p.user_id = u.id) p) a ON TRUE ORDER BY u.id",
    );
    assert_eq!(
        column_at(&text, 1),
        [
            r#"[[2, "Second"], [1, "Hello"]]"#,
            r#"[[3, "Bob writes"]]"#,
            "[]"
        ]
    );
    let text = result(
        &mut adapter,
        "SELECT u.id, a.data FROM users u LEFT JOIN LATERAL (SELECT COALESCE(JSON_ARRAYAGG(JSON_ARRAY(p.id, p.title)), JSON_ARRAY()) AS data FROM (SELECT *, ROW_NUMBER() OVER (ORDER BY p.id ASC) FROM posts p WHERE p.user_id = u.id LIMIT 1) p) a ON TRUE ORDER BY u.id",
    );
    assert_eq!(
        column_at(&text, 1),
        [r#"[[1, "Hello"]]"#, r#"[[3, "Bob writes"]]"#, "[]"]
    );
    let text = result(
        &mut adapter,
        "SELECT p.id, a.data FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(pt.post_id, pt.tag_id)) AS data FROM post_tags pt WHERE pt.post_id = p.id) a ON TRUE ORDER BY p.id",
    );
    assert_eq!(
        column_at(&text, 1),
        ["[[1, 1], [1, 2]]", "NULL", "[[3, 3]]", "NULL"]
    );
}

#[test]
fn a_lateral_table_read_other_than_as_one_document_is_refused() {
    let (_directory, mut adapter) = drizzle_adapter();
    for sql in [
        // Named outside the result columns.
        "SELECT p.id FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(pt.tag_id)) AS data FROM post_tags pt WHERE pt.post_id = p.id) a ON TRUE ORDER BY a.data",
        "SELECT p.id FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(pt.tag_id)) AS data FROM post_tags pt WHERE pt.post_id = p.id) a ON TRUE WHERE a.data IS NULL",
        "SELECT * FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(pt.tag_id)) AS data FROM post_tags pt WHERE pt.post_id = p.id) a ON TRUE",
        // Joined other than LEFT ... ON TRUE.
        "SELECT p.id, a.data FROM posts p JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(pt.tag_id)) AS data FROM post_tags pt WHERE pt.post_id = p.id) a ON TRUE",
        "SELECT p.id, a.data FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(pt.tag_id)) AS data FROM post_tags pt WHERE pt.post_id = p.id) a ON a.data IS NOT NULL",
        // A body answering more than one row.
        "SELECT p.id, a.data FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAY(pt.tag_id) AS data FROM post_tags pt WHERE pt.post_id = p.id) a ON TRUE",
        // A DECIMAL with places, which MySQL writes with every place.
        "SELECT p.id, a.data FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAY(u.balance) AS data FROM (SELECT * FROM users u WHERE u.id = p.user_id LIMIT 1) u) a ON TRUE",
        // A TIMESTAMP, which MySQL writes in the session's zone.
        "SELECT p.id, a.data FROM posts p LEFT JOIN LATERAL (SELECT JSON_ARRAY(u.created_at) AS data FROM (SELECT * FROM users u WHERE u.id = p.user_id LIMIT 1) u) a ON TRUE",
        // Rows matched by words, or cut with no order.
        "SELECT u.id, a.data FROM users u LEFT JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(p.id)) AS data FROM posts p WHERE p.title = u.name) a ON TRUE",
        "SELECT u.id, a.data FROM users u LEFT JOIN LATERAL (SELECT JSON_ARRAYAGG(JSON_ARRAY(p.id)) AS data FROM (SELECT * FROM posts p WHERE p.user_id = u.id LIMIT 2) p) a ON TRUE",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
