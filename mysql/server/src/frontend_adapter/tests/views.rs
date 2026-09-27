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
