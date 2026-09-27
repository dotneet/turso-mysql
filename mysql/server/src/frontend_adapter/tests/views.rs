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

/// A new session over the same files, the catalog opened again from disk
/// once the session before it has let go of it.
fn reopened(directory: &tempfile::TempDir, adapter: Adapter) -> Adapter {
    drop(adapter);
    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    let factory = AuthorizedDatabaseAdapterFactory::new(
        catalog,
        binary_context(),
        Arc::new(RecordingAuthorizer::with_schema_creator("root")),
    );
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([152; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("probe").unwrap();
    adapter
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

/// MySQL keeps a view with a condition as the text it prints back, every
/// column qualified by its table as the table declares it and each comparison
/// and each run of `AND` or `OR` in parentheses; the view is made from that
/// text here and kept as it. The host and the collation printed differ as they
/// do for every view.
#[test]
fn a_view_with_a_condition_is_kept_the_way_mysql_prints_it() {
    let (directory, mut adapter) = adapter();
    let shown = |adapter: &mut Adapter, view: &str| {
        rows(adapter, &format!("SHOW CREATE VIEW {view}"))[0]
            .split('|')
            .nth(1)
            .unwrap()
            .to_owned()
    };
    let prefix = |view: &str| {
        format!(
            "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `{view}` AS "
        )
    };
    for (sql, view, select, expected_rows) in [
        (
            "CREATE VIEW v3 AS SELECT id, title FROM posts WHERE n > 1",
            "v3",
            "select `posts`.`id` AS `id`,`posts`.`title` AS `title` from `posts` where (`posts`.`n` > 1)",
            &["1|a", "2|b"][..],
        ),
        (
            "CREATE VIEW y1 AS SELECT ID, Title AS T, posts.N FROM posts WHERE N > 1 AND TITLE = 'A'",
            "y1",
            "select `posts`.`id` AS `ID`,`posts`.`title` AS `T`,`posts`.`n` AS `N` from `posts` where ((`posts`.`n` > 1) and (`posts`.`title` = 'A'))",
            &["1|a|5"],
        ),
        (
            "CREATE VIEW x1 AS SELECT id FROM posts WHERE (n > 1 AND (n < 50 AND title = 'a')) OR (user_id = 2 OR user_id IS NULL)",
            "x1",
            "select `posts`.`id` AS `id` from `posts` where (((`posts`.`n` > 1) and (`posts`.`n` < 50) and (`posts`.`title` = 'a')) or (`posts`.`user_id` = 2) or (`posts`.`user_id` is null))",
            &["1", "3"],
        ),
        (
            "CREATE VIEW x2 AS SELECT id AS pid FROM posts WHERE n <> user_id AND title <> 'it''s'",
            "x2",
            "select `posts`.`id` AS `pid` from `posts` where ((`posts`.`n` <> `posts`.`user_id`) and (`posts`.`title` <> 'it\\'s'))",
            &["1", "2", "3"],
        ),
    ] {
        run(&mut adapter, sql);
        assert_eq!(shown(&mut adapter, view), format!("{}{select}", prefix(view)), "{sql}");
        let ordered = format!("SELECT * FROM {view} ORDER BY 1");
        assert_eq!(rows(&mut adapter, &ordered), expected_rows, "{sql}");
    }

    let view = columns(&mut adapter, "SELECT * FROM y1");
    let table = columns(&mut adapter, "SELECT id, title, n FROM posts");
    for (view, (table, name)) in view.iter().zip(table.iter().zip(["ID", "T", "N"])) {
        assert_eq!(
            (
                view.name.as_str(),
                view.table.as_str(),
                view.original_table.as_str()
            ),
            (name, "y1", "y1")
        );
        assert_eq!(
            (view.column_type, view.column_length, view.flags),
            (table.column_type, table.column_length, table.flags)
        );
    }

    // A value that is not the column's kind is refused as the same SELECT
    // is, and so is a condition whose printing has not been measured.
    for sql in [
        "CREATE VIEW bad AS SELECT id FROM posts WHERE n = 'x'",
        "CREATE VIEW bad AS SELECT id FROM posts WHERE title LIKE 'a%'",
        "CREATE VIEW bad AS SELECT id FROM posts WHERE n IN (1, 2)",
        "CREATE VIEW bad AS SELECT id FROM posts WHERE n = -1",
        "CREATE VIEW bad AS SELECT id FROM posts p WHERE p.n > 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert_eq!(views(&mut adapter).len(), 4);

    let mut adapter = reopened(&directory, adapter);
    assert_eq!(
        shown(&mut adapter, "y1"),
        format!(
            "{}select `posts`.`id` AS `ID`,`posts`.`title` AS `T`,`posts`.`n` AS `N` from `posts` where ((`posts`.`n` > 1) and (`posts`.`title` = 'A'))",
            prefix("y1")
        )
    );
    assert_eq!(rows(&mut adapter, "SELECT T FROM y1"), ["a"]);
}

/// MySQL reads a view grouping its rows out of a table it gathers the groups
/// into, and reports each column in that table's shape: a grouped column
/// keeps its own table as its original table and loses its key flags, and an
/// aggregate names the view.
#[test]
fn a_view_grouping_its_rows_reports_the_columns_mysql_reports() {
    let (directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE keyed (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, code VARCHAR(10) NOT NULL UNIQUE, grp INT NOT NULL, amount INT DEFAULT 3)",
        "INSERT INTO keyed (code, grp, amount) VALUES ('a', 1, 10), ('b', 1, 20), ('c', 2, NULL)",
        "CREATE VIEW v2 AS SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id",
        "CREATE VIEW g1 AS SELECT id, code, grp, COUNT(*) AS c FROM keyed GROUP BY id, code, grp",
        "CREATE VIEW g2 AS SELECT grp, MIN(code) AS lo, MAX(id) AS hi, COUNT(amount) AS n FROM keyed WHERE grp > 0 GROUP BY grp",
        "CREATE VIEW g3 AS SELECT COUNT(*) AS c, MAX(amount) AS m FROM keyed",
    ] {
        run(&mut adapter, sql);
    }
    let shown = |adapter: &mut Adapter, view: &str| {
        rows(adapter, &format!("SHOW CREATE VIEW {view}"))[0]
            .split('|')
            .nth(1)
            .unwrap()
            .to_owned()
    };
    for (view, select) in [
        (
            "v2",
            "select `posts`.`user_id` AS `user_id`,count(0) AS `c` from `posts` group by `posts`.`user_id`",
        ),
        (
            "g1",
            "select `keyed`.`id` AS `id`,`keyed`.`code` AS `code`,`keyed`.`grp` AS `grp`,count(0) AS `c` from `keyed` group by `keyed`.`id`,`keyed`.`code`,`keyed`.`grp`",
        ),
        (
            "g2",
            "select `keyed`.`grp` AS `grp`,min(`keyed`.`code`) AS `lo`,max(`keyed`.`id`) AS `hi`,count(`keyed`.`amount`) AS `n` from `keyed` where (`keyed`.`grp` > 0) group by `keyed`.`grp`",
        ),
        (
            "g3",
            "select count(0) AS `c`,max(`keyed`.`amount`) AS `m` from `keyed`",
        ),
    ] {
        assert_eq!(
            shown(&mut adapter, view),
            format!("CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `{view}` AS {select}")
        );
    }
    for (sql, expected) in [
        ("SELECT * FROM v2 ORDER BY 1", &["1|2", "2|1"][..]),
        (
            "SELECT * FROM g1 ORDER BY 1",
            &["1|a|1|1", "2|b|1|1", "3|c|2|1"],
        ),
        ("SELECT * FROM g2 ORDER BY 1", &["1|a|2|2", "2|c|3|0"]),
        ("SELECT * FROM g3", &["3|20"]),
    ] {
        assert_eq!(rows(&mut adapter, sql), expected, "{sql}");
    }

    let shape = |column: &ColumnDefinitionConfig| {
        (
            column.name.clone(),
            column.table.clone(),
            column.original_table.clone(),
            column.column_type,
            column.column_length,
            column.flags & !MYSQL_NUM_FLAG,
            column.decimals,
        )
    };
    let not_null = MYSQL_NOT_NULL_FLAG;
    let no_default = MYSQL_NO_DEFAULT_VALUE_FLAG;
    let unsigned = MYSQL_UNSIGNED_FLAG;
    for (sql, expected) in [
        (
            "SELECT * FROM v2",
            vec![
                ("user_id", "v2", "posts", MYSQL_TYPE_LONG, 11, 0),
                ("c", "v2", "v2", MYSQL_TYPE_LONGLONG, 21, not_null),
            ],
        ),
        (
            "SELECT * FROM g1",
            vec![
                (
                    "id",
                    "g1",
                    "keyed",
                    MYSQL_TYPE_LONGLONG,
                    20,
                    not_null | unsigned,
                ),
                (
                    "code",
                    "g1",
                    "keyed",
                    MYSQL_TYPE_VAR_STRING,
                    40,
                    not_null | no_default,
                ),
                (
                    "grp",
                    "g1",
                    "keyed",
                    MYSQL_TYPE_LONG,
                    11,
                    not_null | no_default,
                ),
                ("c", "g1", "g1", MYSQL_TYPE_LONGLONG, 21, not_null),
            ],
        ),
        (
            "SELECT * FROM g2",
            vec![
                (
                    "grp",
                    "g2",
                    "keyed",
                    MYSQL_TYPE_LONG,
                    11,
                    not_null | no_default,
                ),
                ("lo", "g2", "g2", MYSQL_TYPE_VAR_STRING, 40, no_default),
                ("hi", "g2", "g2", MYSQL_TYPE_LONGLONG, 20, unsigned),
                ("n", "g2", "g2", MYSQL_TYPE_LONGLONG, 21, not_null),
            ],
        ),
        (
            "SELECT * FROM g3",
            vec![
                ("c", "g3", "g3", MYSQL_TYPE_LONGLONG, 21, not_null),
                ("m", "g3", "g3", MYSQL_TYPE_LONG, 11, 0),
            ],
        ),
        (
            "SELECT c FROM g1 x",
            vec![("c", "x", "g1", MYSQL_TYPE_LONGLONG, 21, not_null)],
        ),
    ] {
        let expected = expected
            .into_iter()
            .map(|(name, table, original, kind, length, flags)| {
                (
                    name.to_owned(),
                    table.to_owned(),
                    original.to_owned(),
                    kind,
                    length,
                    flags,
                    0,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            columns(&mut adapter, sql)
                .iter()
                .map(shape)
                .collect::<Vec<_>>(),
            expected,
            "{sql}"
        );
    }

    // Measured, `SUM` of an `INT` is a `NEWDECIMAL` of 33 and `AVG` one of 16
    // with 4 decimals; a view reads neither here, and nothing unnamed.
    for sql in [
        "CREATE VIEW bad AS SELECT grp, SUM(amount) AS s FROM keyed GROUP BY grp",
        "CREATE VIEW bad AS SELECT grp, AVG(amount) AS a FROM keyed GROUP BY grp",
        "CREATE VIEW bad AS SELECT grp, COUNT(*) FROM keyed GROUP BY grp",
        "CREATE VIEW bad AS SELECT grp FROM keyed GROUP BY grp HAVING COUNT(*) > 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }

    let mut adapter = reopened(&directory, adapter);
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM v2 ORDER BY 1"),
        ["1|2", "2|1"]
    );
    assert_eq!(
        columns(&mut adapter, "SELECT c FROM v2")[0].column_length,
        21
    );
}
