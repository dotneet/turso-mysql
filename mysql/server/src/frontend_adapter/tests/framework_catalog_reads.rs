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
