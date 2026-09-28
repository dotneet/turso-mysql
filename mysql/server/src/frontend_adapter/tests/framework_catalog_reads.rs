//! The `information_schema` reads a framework makes before it migrates, each
//! as its current release writes it.
//!
//! Every expected shape was measured on MySQL 8.4.11.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([101; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE `users` (`id` bigint NOT NULL AUTO_INCREMENT PRIMARY KEY, `email` varchar(255) NOT NULL)",
        "CREATE VIEW `emails` AS SELECT email FROM users",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> (Vec<ColumnDefinitionConfig>, Vec<Vec<Option<String>>>) {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    let rows = result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect();
    (result.columns, rows)
}

fn text(value: &str) -> Option<String> {
    Some(value.to_owned())
}

/// Rails asks `table_exists?` and `data_source_exists?` this way, with
/// `database()` standing for the selected database.
#[test]
fn rails_finds_out_whether_a_table_is_there() {
    let (_directory, mut adapter) = adapter();
    let exists = |table: &str, kind: &str| {
        format!(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = database() AND table_name = '{table}' AND table_name IN (SELECT table_name FROM information_schema.tables WHERE table_schema = database()){kind}"
        )
    };
    assert_eq!(
        rows(
            &mut adapter,
            &exists("users", " AND table_type = 'BASE TABLE'")
        )
        .1,
        [[text("users")]]
    );
    assert!(rows(&mut adapter, &exists("schema_migrations", ""))
        .1
        .is_empty());
    assert!(rows(
        &mut adapter,
        &exists("emails", " AND table_type = 'BASE TABLE'")
    )
    .1
    .is_empty());

    // Its primary key read, with the newline the heredoc leaves at the end.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT column_name\nFROM information_schema.statistics\nWHERE index_name = 'PRIMARY'\n  AND table_schema = database()\n  AND table_name = 'users'\nORDER BY seq_in_index\n"
        )
        .1,
        [[text("id")]]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT table_comment FROM information_schema.tables WHERE table_schema = database() AND table_name = 'users'"
        )
        .1,
        [[text("")]]
    );
}

/// Laravel's `hasTable`. Measured on MySQL 8.4.11: the `EXISTS` answers a NOT
/// NULL `LONGLONG` of length 1 with the binary and numeric flags.
#[test]
fn laravel_finds_out_whether_a_table_is_there() {
    let (_directory, mut adapter) = adapter();
    let has_table = |table: &str| {
        format!(
            "select exists (select 1 from information_schema.tables where table_schema = schema() and table_name = '{table}' and table_type in ('BASE TABLE', 'SYSTEM VERSIONED')) as `exists`"
        )
    };
    let (columns, found) = rows(&mut adapter, &has_table("users"));
    assert_eq!(found, [[text("1")]]);
    assert_eq!(columns[0].name, "exists");
    assert_eq!(columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(columns[0].column_length, 1);
    assert_eq!(
        columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
    assert_eq!(
        rows(&mut adapter, &has_table("migrations")).1,
        [[text("0")]]
    );
    assert_eq!(rows(&mut adapter, &has_table("emails")).1, [[text("0")]]);
}

/// Laravel lists every table before `migrate:fresh` drops them. Measured on
/// MySQL 8.4.11: a view has no engine, collation or storage and its comment is
/// `VIEW`; the storage figures are ones this server does not keep, so they are
/// NULL; and their sum is an unsigned `LONGLONG` of length 22 without the
/// binary flag.
#[test]
fn laravel_lists_the_tables_it_would_drop() {
    let (_directory, mut adapter) = adapter();
    let (columns, listed) = rows(
        &mut adapter,
        "select table_name as `name`, table_schema as `schema`, (data_length + index_length) as `size`, table_comment as `comment`, engine as `engine`, table_collation as `collation` from information_schema.tables where table_type in ('BASE TABLE', 'SYSTEM VERSIONED') and table_schema not in ('information_schema', 'mysql', 'ndbinfo', 'performance_schema', 'sys') order by table_schema, table_name",
    );
    assert_eq!(
        listed,
        [
            [
                text("records"),
                text("reports"),
                None,
                text(""),
                text("InnoDB"),
                text("utf8mb4_0900_ai_ci")
            ],
            [
                text("users"),
                text("reports"),
                None,
                text(""),
                text("InnoDB"),
                text("utf8mb4_0900_ai_ci")
            ],
        ]
    );
    assert_eq!(columns[2].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(columns[2].column_length, 22);
    assert_eq!(columns[2].flags, MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG);
    assert_eq!(columns[3].column_type, MYSQL_TYPE_BLOB);

    let (_, views) = rows(
        &mut adapter,
        "SELECT table_name, engine, table_collation, table_comment, data_length FROM information_schema.tables WHERE table_schema = database() AND table_type = 'VIEW'",
    );
    assert_eq!(views, [[text("emails"), None, None, text("VIEW"), None]]);
}

/// Laravel's `Schema::getIndexes` and `Schema::getForeignKeys`, which
/// `hasIndex`, `db:table` and every migration's introspection read, each
/// grouping a table's catalog rows with `GROUP_CONCAT(... ORDER BY ...)`.
///
/// Measured on MySQL 8.4.11 over both protocols: one row for each index or
/// foreign key, in the order of its name without regard to case, its columns
/// joined by commas in their order in the key; the joined columns a
/// `LONG_BLOB` of 36864 under the default `group_concat_max_len`; `unique` a
/// NOT NULL `LONG` of 1 over the text protocol and a `LONGLONG` over the
/// binary one; and each read column's origin named after its alias over the
/// text protocol and after the catalog table's own column over the binary one.
#[test]
fn laravel_reads_a_tables_indexes_and_foreign_keys() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "DROP VIEW `emails`",
        "create table `tags` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null)",
        "alter table `tags` add unique `tags_name_unique`(`name`)",
        "create table `post_tag` (`post_id` bigint unsigned not null, `tag_id` bigint unsigned not null, primary key (`post_id`, `tag_id`))",
        "create table `posts` (`id` bigint unsigned not null auto_increment primary key)",
        "alter table `post_tag` add constraint `post_tag_post_id_foreign` foreign key (`post_id`) references `posts` (`id`) on delete cascade",
        "alter table `post_tag` add constraint `post_tag_tag_id_foreign` foreign key (`tag_id`) references `tags` (`id`) on delete cascade",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    let indexes = |table: &str, schema: &str| {
        format!("select index_name as `name`, group_concat(column_name order by seq_in_index) as `columns`, index_type as `type`, not non_unique as `unique` from information_schema.statistics where table_schema = {schema} and table_name = '{table}' group by index_name, index_type, non_unique")
    };
    let foreign_keys = |table: &str| {
        format!("select kc.constraint_name as `name`, group_concat(kc.column_name order by kc.ordinal_position) as `columns`, kc.referenced_table_schema as `foreign_schema`, kc.referenced_table_name as `foreign_table`, group_concat(kc.referenced_column_name order by kc.ordinal_position) as `foreign_columns`, rc.update_rule as `on_update`, rc.delete_rule as `on_delete` from information_schema.key_column_usage kc join information_schema.referential_constraints rc on kc.constraint_schema = rc.constraint_schema and kc.constraint_name = rc.constraint_name where kc.table_schema = schema() and kc.table_name = '{table}' and kc.referenced_table_name is not null group by kc.constraint_name, kc.referenced_table_schema, kc.referenced_table_name, rc.update_rule, rc.delete_rule")
    };

    let (columns, listed) = rows(&mut adapter, &indexes("post_tag", "schema()"));
    assert_eq!(
        listed,
        [
            [
                text("post_tag_tag_id_foreign"),
                text("tag_id"),
                text("BTREE"),
                text("0")
            ],
            [
                text("PRIMARY"),
                text("post_id,tag_id"),
                text("BTREE"),
                text("1")
            ],
        ]
    );
    assert_eq!(
        columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.original_name.as_str(),
                column.table.as_str(),
                column.schema.as_str(),
                column.column_type,
                column.column_length,
                column.flags,
                column.decimals
            ))
            .collect::<Vec<_>>(),
        [
            (
                "name",
                "name",
                "statistics",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
                0
            ),
            ("columns", "", "", "", MYSQL_TYPE_LONG_BLOB, 36864, 0, 31),
            (
                "type",
                "type",
                "statistics",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                44,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
                0
            ),
            (
                "unique",
                "unique",
                "",
                "",
                MYSQL_TYPE_LONG,
                1,
                MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
                0
            ),
        ]
    );
    assert_eq!(
        rows(&mut adapter, &indexes("tags", "'reports'")).1,
        [
            [text("PRIMARY"), text("id"), text("BTREE"), text("1")],
            [
                text("tags_name_unique"),
                text("name"),
                text("BTREE"),
                text("1")
            ],
        ]
    );
    assert!(rows(&mut adapter, &indexes("missing", "schema()"))
        .1
        .is_empty());

    let statement = adapter
        .execute_stmt_prepare(&indexes("post_tag", "schema()"))
        .unwrap();
    assert_eq!(
        statement
            .columns
            .iter()
            .map(|column| (
                column.original_name.as_str(),
                column.original_table.as_str(),
                column.schema.as_str(),
                column.column_type,
                column.flags,
                column.decimals
            ))
            .collect::<Vec<_>>(),
        [
            ("INDEX_NAME", "STATISTICS", "", MYSQL_TYPE_VAR_STRING, 0, 31),
            ("", "", "", MYSQL_TYPE_LONG_BLOB, 0, 31),
            (
                "INDEX_TYPE",
                "STATISTICS",
                "",
                MYSQL_TYPE_VAR_STRING,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
                31
            ),
            (
                "",
                "",
                "",
                MYSQL_TYPE_LONGLONG,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
                0
            ),
        ]
    );
    let Ok(PreparedStatementExecutionResult::ResultSet(binary)) =
        adapter.execute_stmt_execute(statement.statement_id, &[])
    else {
        panic!("the prepared index read must answer rows");
    };
    assert_eq!(binary.columns, statement.columns);
    assert_eq!(
        binary.rows[1],
        [
            BinaryResultValue::Text("PRIMARY".to_owned()),
            BinaryResultValue::Blob(b"post_id,tag_id".to_vec()),
            BinaryResultValue::Text("BTREE".to_owned()),
            BinaryResultValue::Integer(1),
        ]
    );

    let (columns, listed) = rows(&mut adapter, &foreign_keys("post_tag"));
    assert_eq!(
        listed,
        [
            [
                text("post_tag_post_id_foreign"),
                text("post_id"),
                text("reports"),
                text("posts"),
                text("id"),
                text("NO ACTION"),
                text("CASCADE")
            ],
            [
                text("post_tag_tag_id_foreign"),
                text("tag_id"),
                text("reports"),
                text("tags"),
                text("id"),
                text("NO ACTION"),
                text("CASCADE")
            ],
        ]
    );
    assert_eq!(
        columns
            .iter()
            .map(|column| (
                column.original_name.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.column_type,
                column.flags
            ))
            .collect::<Vec<_>>(),
        [
            ("name", "kc", "", MYSQL_TYPE_VAR_STRING, 0),
            ("", "", "", MYSQL_TYPE_LONG_BLOB, 0),
            (
                "foreign_schema",
                "kc",
                "",
                MYSQL_TYPE_VAR_STRING,
                MYSQL_BINARY_FLAG
            ),
            (
                "foreign_table",
                "kc",
                "",
                MYSQL_TYPE_VAR_STRING,
                MYSQL_BINARY_FLAG
            ),
            ("", "", "", MYSQL_TYPE_LONG_BLOB, 0),
            (
                "on_update",
                "rc",
                "foreign_keys",
                MYSQL_TYPE_STRING,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_BINARY_FLAG
                    | MYSQL_ENUM_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG
            ),
            (
                "on_delete",
                "rc",
                "foreign_keys",
                MYSQL_TYPE_STRING,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_BINARY_FLAG
                    | MYSQL_ENUM_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG
            ),
        ]
    );
    let statement = adapter
        .execute_stmt_prepare(&foreign_keys("users"))
        .unwrap();
    assert_eq!(statement.columns[0].original_name, "CONSTRAINT_NAME");
    assert_eq!(
        statement.columns[5].original_table,
        "REFERENTIAL_CONSTRAINTS"
    );
    let Ok(PreparedStatementExecutionResult::ResultSet(binary)) =
        adapter.execute_stmt_execute(statement.statement_id, &[])
    else {
        panic!("the prepared foreign key read must answer rows");
    };
    assert!(binary.rows.is_empty());

    // The width MySQL reports for the joined columns follows the limit, by a
    // rule not worked out past the default.
    adapter
        .execute_query("SET SESSION group_concat_max_len = 2048")
        .unwrap();
    assert_eq!(
        adapter.execute_query(&indexes("post_tag", "schema()")),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// Django reads the table list and a table's collation this way, the first
/// before it records a migration and the second before it alters a column.
#[test]
fn django_reads_the_tables_and_a_tables_collation() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "\n            SELECT\n                table_name,\n                table_type,\n                table_comment\n            FROM information_schema.tables\n            WHERE table_schema = DATABASE()\n            "
        )
        .1,
        [
            [text("emails"), text("VIEW"), text("VIEW")],
            [text("records"), text("BASE TABLE"), text("")],
            [text("users"), text("BASE TABLE"), text("")],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT  table_collation\nFROM    information_schema.tables\nWHERE   table_schema = DATABASE()\nAND     table_name = 'users'"
        )
        .1,
        [[text("utf8mb4_0900_ai_ci")]]
    );
}
