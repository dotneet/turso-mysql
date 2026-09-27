//! Views: dropping, replacing and reading them.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("root"));
    let (directory, catalog, factory) = catalog_factory(authorizer);
    // The notes name the database, and MySQL's were measured in one called
    // `probe`.
    catalog.create("probe").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([152; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("probe").unwrap();
    for sql in [
        "CREATE TABLE posts (id INT PRIMARY KEY, user_id INT, n INT, title VARCHAR(20))",
        "INSERT INTO posts VALUES (1, 1, 5, 'a'), (2, 1, 50, 'b'), (3, 2, 1, 'c')",
        "CREATE TABLE plain (id INT PRIMARY KEY, name VARCHAR(10))",
    ] {
        run(&mut adapter, sql);
    }
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result,
        other => panic!("{sql}: {other:?}"),
    }
}

/// Each row of the result, its values joined by `|`.
fn rows(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| {
                    value.map_or("NULL".to_owned(), |value| String::from_utf8(value).unwrap())
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn views(adapter: &mut Adapter) -> Vec<String> {
    rows(adapter, "SHOW FULL TABLES")
        .into_iter()
        .filter(|row| row.ends_with("|VIEW"))
        .collect()
}

#[test]
fn drop_view_takes_if_exists_and_several_names() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "CREATE VIEW v1 AS SELECT id FROM posts");
    run(&mut adapter, "CREATE VIEW v2 AS SELECT id FROM posts");

    // A name given twice is 1066, a table among the names 1347 before a
    // missing one's 1051, and each of them drops nothing.
    for (sql, error) in [
        ("DROP VIEW v1, v1", FrontendErrorKind::NotUniqueTable),
        ("DROP VIEW v1, nope, v1", FrontendErrorKind::NotUniqueTable),
        (
            "DROP VIEW IF EXISTS v1, v1",
            FrontendErrorKind::NotUniqueTable,
        ),
        ("DROP VIEW v1, plain", FrontendErrorKind::NotView),
        ("DROP VIEW nope, plain", FrontendErrorKind::NotView),
        ("DROP VIEW v1, nope", FrontendErrorKind::UnknownView),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(error), "{sql}");
    }
    assert_eq!(views(&mut adapter), ["v1|VIEW", "v2|VIEW"]);

    // `IF EXISTS` drops the views and notes every other name in order.
    let dropped = run(&mut adapter, "DROP VIEW IF EXISTS plain, nope, v1");
    assert_eq!(dropped.warnings, 2);
    assert_eq!(
        rows(&mut adapter, "SHOW WARNINGS"),
        [
            "Note|1347|'probe.plain' is not VIEW",
            "Note|1051|Unknown table 'probe.nope'",
        ]
    );
    assert_eq!(views(&mut adapter), ["v2|VIEW"]);
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM plain"),
        Vec::<String>::new()
    );

    // `RESTRICT` and `CASCADE` are read and change nothing.
    assert_eq!(run(&mut adapter, "DROP VIEW v2 CASCADE").warnings, 0);
    assert!(views(&mut adapter).is_empty());
    assert_eq!(
        adapter.execute_query("DROP VIEW v2 RESTRICT"),
        Err(FrontendErrorKind::UnknownView)
    );
    assert_eq!(run(&mut adapter, "DROP VIEW IF EXISTS v2").warnings, 1);
    assert_eq!(
        rows(&mut adapter, "SHOW WARNINGS"),
        ["Note|1051|Unknown table 'probe.v2'"]
    );
}

/// MySQL prints the definer's host, `localhost` for the measured `root`, and
/// the connection's collation, `utf8mb4_0900_ai_ci`; this server prints `%`,
/// knowing no host, and the `utf8mb4_general_ci` it claims everywhere.
#[test]
fn create_or_replace_and_alter_view_write_the_view_again() {
    let (_directory, mut adapter) = adapter();
    let shown =
        |adapter: &mut Adapter, view: &str| rows(adapter, &format!("SHOW CREATE VIEW {view}"));
    run(
        &mut adapter,
        "CREATE OR REPLACE VIEW v3 AS SELECT id FROM plain",
    );
    assert_eq!(
        shown(&mut adapter, "v3"),
        ["v3|CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v3` AS select `plain`.`id` AS `id` from `plain`|utf8mb4|utf8mb4_general_ci"]
    );
    run(
        &mut adapter,
        "CREATE OR REPLACE VIEW v3 AS SELECT id, name FROM plain",
    );
    assert_eq!(
        shown(&mut adapter, "v3"),
        ["v3|CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v3` AS select `plain`.`id` AS `id`,`plain`.`name` AS `name` from `plain`|utf8mb4|utf8mb4_general_ci"]
    );
    run(&mut adapter, "ALTER VIEW v3 AS SELECT name FROM plain");
    assert_eq!(
        shown(&mut adapter, "v3"),
        ["v3|CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v3` AS select `plain`.`name` AS `name` from `plain`|utf8mb4|utf8mb4_general_ci"]
    );
    run(&mut adapter, "INSERT INTO plain (id, name) VALUES (1, 'x')");
    assert_eq!(rows(&mut adapter, "SELECT name FROM v3"), ["x"]);

    for (sql, error) in [
        (
            "CREATE OR REPLACE VIEW plain AS SELECT id FROM posts",
            FrontendErrorKind::NotView,
        ),
        (
            "ALTER VIEW plain AS SELECT id FROM posts",
            FrontendErrorKind::NotView,
        ),
        (
            "ALTER VIEW nope AS SELECT id FROM posts",
            FrontendErrorKind::MissingObject,
        ),
        (
            "CREATE VIEW v3 AS SELECT id FROM plain",
            FrontendErrorKind::DuplicateObject,
        ),
        ("SHOW CREATE VIEW plain", FrontendErrorKind::NotView),
        ("SHOW CREATE VIEW nope", FrontendErrorKind::MissingObject),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(error), "{sql}");
    }
    // A body the checked CREATE VIEW refuses leaves the old view standing.
    assert!(adapter
        .execute_query("CREATE OR REPLACE VIEW v3 AS SELECT id FROM plain LIMIT 1")
        .is_err());
    assert_eq!(rows(&mut adapter, "SELECT name FROM v3"), ["x"]);
    assert_eq!(views(&mut adapter), ["v3|VIEW"]);
}

fn columns(adapter: &mut Adapter, sql: &str) -> Vec<ColumnDefinitionConfig> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result.columns
}

/// Measured on MySQL 8.4.11: a view projecting one table's columns reports
/// each the way the table does — type, length, collation and flags, the
/// primary key's among them — under the view's name as both its table and
/// its original table, or under the alias it is read through as its table.
#[test]
fn a_view_reports_its_columns_the_way_its_table_does() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE VIEW v1 AS SELECT id, title FROM posts",
    );
    let table = columns(&mut adapter, "SELECT id, title FROM posts");
    let view = columns(&mut adapter, "SELECT * FROM v1 ORDER BY id");
    assert_eq!(view.len(), 2);
    for (view, table) in view.iter().zip(&table) {
        assert_eq!(view.table, "v1");
        assert_eq!(view.original_table, "v1");
        assert_eq!(view.schema, "probe");
        assert_eq!(
            (
                &view.name,
                view.column_type,
                view.column_length,
                view.flags,
                view.decimals,
                view.character_set
            ),
            (
                &table.name,
                table.column_type,
                table.column_length,
                table.flags,
                table.decimals,
                table.character_set
            )
        );
    }
    assert_eq!(view[0].column_type, MYSQL_TYPE_LONG);
    assert_eq!(view[1].column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(view[1].column_length, 80);

    let aliased = columns(&mut adapter, "SELECT x.id FROM v1 x");
    assert_eq!(aliased[0].table, "x");
    assert_eq!(aliased[0].original_table, "v1");
    assert_eq!(aliased[0].flags, table[0].flags);

    let counted = columns(&mut adapter, "SELECT COUNT(*) FROM v1");
    assert_eq!(counted[0].table, "");
    assert_eq!(counted[0].column_type, MYSQL_TYPE_LONGLONG);
}
