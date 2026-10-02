use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const CHILD_ROW_WITHOUT_PARENT: &str =
    "Cannot add or update a child row: a foreign key constraint fails";
const PARENT_ROW_WITH_CHILDREN: &str =
    "Cannot delete or update a parent row: a foreign key constraint fails";

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("d").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([211; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("d").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    if let Err(error) = adapter.execute_query(sql) {
        panic!("{sql}: {error:?} {:?}", adapter.take_error_message());
    }
}

fn written(adapter: &mut Adapter, sql: &str) -> (u64, u64, u16) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => {
            (result.affected_rows, result.last_insert_id, result.warnings)
        }
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<String>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map_or("NULL".to_owned(), |v| String::from_utf8(v).unwrap()))
                .collect()
        })
        .collect()
}

fn warnings(adapter: &mut Adapter) -> Vec<(String, String)> {
    rows(adapter, "SHOW WARNINGS")
        .into_iter()
        .map(|row| (row[1].clone(), row[2].clone()))
        .collect()
}

fn refused(adapter: &mut Adapter, sql: &str) -> (FrontendErrorKind, String) {
    match adapter.execute_query(sql) {
        Err(kind) => (
            kind,
            String::from_utf8(adapter.take_error_message().unwrap_or_default()).unwrap(),
        ),
        Ok(result) => panic!("{sql} must be refused, answered {result:?}"),
    }
}

fn counter(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE {table}"))[0][1]
        .split_whitespace()
        .find_map(|word| word.strip_prefix("AUTO_INCREMENT="))
        .unwrap_or("1")
        .to_owned()
}

fn child_warning(constraint: &str) -> (String, String) {
    (
        "1452".to_owned(),
        format!("{CHILD_ROW_WITHOUT_PARENT} ({constraint})"),
    )
}

const C_IBFK_1: &str = "`d`.`c`, CONSTRAINT `c_ibfk_1` FOREIGN KEY (`pid`) REFERENCES `p` (`id`)";

#[test]
fn insert_ignore_skips_a_counted_child_naming_no_parent_with_warning_1452() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "CREATE TABLE p (id INT PRIMARY KEY)");
    run(
        &mut adapter,
        "CREATE TABLE c (id INT AUTO_INCREMENT PRIMARY KEY, pid INT, FOREIGN KEY (pid) REFERENCES p (id))",
    );
    run(&mut adapter, "INSERT INTO p VALUES (1), (2)");

    assert_eq!(
        written(&mut adapter, "INSERT IGNORE INTO c (pid) VALUES (9)"),
        (0, 0, 1)
    );
    assert_eq!(warnings(&mut adapter), [child_warning(C_IBFK_1)]);
    assert_eq!(rows(&mut adapter, "SELECT LAST_INSERT_ID()"), [["0"]]);
    assert_eq!(counter(&mut adapter, "c"), "2");

    assert_eq!(
        written(
            &mut adapter,
            "INSERT IGNORE INTO c (pid) VALUES (1), (9), (2)"
        ),
        (2, 2, 1)
    );
    assert_eq!(warnings(&mut adapter), [child_warning(C_IBFK_1)]);
    assert_eq!(
        rows(&mut adapter, "SELECT id, pid FROM c ORDER BY id"),
        [["2", "1"], ["3", "2"]]
    );
    assert_eq!(counter(&mut adapter, "c"), "5");

    assert_eq!(
        written(&mut adapter, "INSERT IGNORE INTO c (pid) VALUES (8), (7)"),
        (0, 0, 2)
    );
    assert_eq!(
        warnings(&mut adapter),
        [child_warning(C_IBFK_1), child_warning(C_IBFK_1)]
    );
    assert_eq!(rows(&mut adapter, "SELECT LAST_INSERT_ID()"), [["2"]]);
    assert_eq!(counter(&mut adapter, "c"), "7");

    assert_eq!(
        written(
            &mut adapter,
            "INSERT IGNORE INTO c (id, pid) VALUES (100, 9)"
        ),
        (0, 0, 1)
    );
    assert_eq!(counter(&mut adapter, "c"), "7");
    assert_eq!(
        written(
            &mut adapter,
            "INSERT IGNORE INTO c (id, pid) VALUES (101, 9), (102, 2)"
        ),
        (1, 102, 1)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT IGNORE INTO c (id, pid) VALUES (103, 1), (104, 9)"
        ),
        (1, 104, 1)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT IGNORE INTO c (id, pid) VALUES (105, 9), (106, 8)"
        ),
        (0, 0, 2)
    );
    assert_eq!(counter(&mut adapter, "c"), "104");
    assert_eq!(
        written(&mut adapter, "INSERT IGNORE INTO c (pid) VALUES (9), (1)"),
        (1, 104, 1)
    );
    assert_eq!(
        written(&mut adapter, "INSERT IGNORE INTO c SET pid = 9"),
        (0, 0, 1)
    );
    assert_eq!(counter(&mut adapter, "c"), "107");
    assert_eq!(
        rows(&mut adapter, "SELECT id, pid FROM c ORDER BY id"),
        [
            ["2", "1"],
            ["3", "2"],
            ["102", "2"],
            ["103", "1"],
            ["104", "1"]
        ]
    );
}

#[test]
fn insert_ignore_skips_a_child_naming_no_parent_in_a_table_that_does_not_count() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "CREATE TABLE p (id INT PRIMARY KEY)");
    run(
        &mut adapter,
        "CREATE TABLE c (id INT PRIMARY KEY, pid INT, FOREIGN KEY (pid) REFERENCES p (id))",
    );
    run(&mut adapter, "INSERT INTO p VALUES (1), (2)");

    assert_eq!(
        written(
            &mut adapter,
            "INSERT IGNORE INTO c VALUES (1, 1), (2, 9), (3, 2)"
        ),
        (2, 0, 1)
    );
    assert_eq!(warnings(&mut adapter), [child_warning(C_IBFK_1)]);

    let statement = adapter
        .execute_stmt_prepare("INSERT IGNORE INTO c (id, pid) VALUES (?, ?)")
        .unwrap();
    let Ok(PreparedStatementExecutionResult::Ok(skipped)) =
        adapter.execute_stmt_execute(statement.statement_id, &integers(&[4, 9]))
    else {
        panic!("the prepared INSERT IGNORE must skip the row");
    };
    assert_eq!((skipped.affected_rows, skipped.warnings), (0, 1));
    assert_eq!(warnings(&mut adapter), [child_warning(C_IBFK_1)]);
    assert_eq!(
        rows(&mut adapter, "SELECT id, pid FROM c ORDER BY id"),
        [["1", "1"], ["3", "2"]]
    );
}

fn integers(values: &[i64]) -> Vec<u8> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for _ in values {
        payload.extend_from_slice(&[MYSQL_TYPE_LONGLONG, 0]);
    }
    for value in values {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    payload
}

#[test]
fn a_refused_row_names_the_constraint_that_refused_it() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE p (id INT PRIMARY KEY)",
        "CREATE TABLE p2 (a INT, b INT, PRIMARY KEY (a, b))",
        "CREATE TABLE c2 (id INT PRIMARY KEY, x INT, y INT, CONSTRAINT fk_Named FOREIGN KEY (x, y) \
         REFERENCES p2 (a, b) ON DELETE CASCADE ON UPDATE SET NULL)",
        "CREATE TABLE c4 (id INT PRIMARY KEY, a INT, b INT, c INT, \
         FOREIGN KEY (a) REFERENCES p (id) ON DELETE RESTRICT ON UPDATE NO ACTION, \
         FOREIGN KEY (b) REFERENCES p (id) ON DELETE NO ACTION, \
         FOREIGN KEY (c) REFERENCES p (id) ON UPDATE RESTRICT)",
        "INSERT INTO p VALUES (1)",
    ] {
        run(&mut adapter, sql);
    }
    let c2 = "`d`.`c2`, CONSTRAINT `fk_Named` FOREIGN KEY (`x`, `y`) REFERENCES `p2` (`a`, `b`) \
              ON DELETE CASCADE ON UPDATE SET NULL";
    assert_eq!(
        refused(&mut adapter, "INSERT INTO c2 VALUES (1, 5, 5)"),
        (
            FrontendErrorKind::ForeignKeyViolation,
            format!("{CHILD_ROW_WITHOUT_PARENT} ({c2})")
        )
    );
    assert_eq!(
        written(&mut adapter, "INSERT IGNORE INTO c2 VALUES (1, 5, 5)"),
        (0, 0, 1)
    );
    assert_eq!(warnings(&mut adapter), [child_warning(c2)]);
    for (sql, constraint) in [
        (
            "INSERT INTO c4 VALUES (1, 9, NULL, NULL)",
            "CONSTRAINT `c4_ibfk_1` FOREIGN KEY (`a`) REFERENCES `p` (`id`) ON DELETE RESTRICT",
        ),
        (
            "INSERT INTO c4 VALUES (1, NULL, 9, NULL)",
            "CONSTRAINT `c4_ibfk_2` FOREIGN KEY (`b`) REFERENCES `p` (`id`)",
        ),
        (
            "INSERT INTO c4 VALUES (1, NULL, NULL, 9)",
            "CONSTRAINT `c4_ibfk_3` FOREIGN KEY (`c`) REFERENCES `p` (`id`) ON UPDATE RESTRICT",
        ),
    ] {
        assert_eq!(
            refused(&mut adapter, sql),
            (
                FrontendErrorKind::ForeignKeyViolation,
                format!("{CHILD_ROW_WITHOUT_PARENT} (`d`.`c4`, {constraint})")
            ),
            "{sql}"
        );
    }
    run(&mut adapter, "INSERT INTO c4 VALUES (1, 1, 1, 1)");
    for sql in [
        "DELETE FROM p WHERE id = 1",
        "UPDATE p SET id = 3 WHERE id = 1",
    ] {
        assert_eq!(
            refused(&mut adapter, sql),
            (
                FrontendErrorKind::ParentRowReferenced,
                format!(
                    "{PARENT_ROW_WITH_CHILDREN} (`d`.`c4`, CONSTRAINT `c4_ibfk_1` FOREIGN KEY \
                     (`a`) REFERENCES `p` (`id`) ON DELETE RESTRICT)"
                )
            ),
            "{sql}"
        );
    }
    let printed = rows(&mut adapter, "SHOW CREATE TABLE c4")[0][1].clone();
    assert!(printed.contains(
        "  CONSTRAINT `c4_ibfk_1` FOREIGN KEY (`a`) REFERENCES `p` (`id`) ON DELETE RESTRICT,\n  \
         CONSTRAINT `c4_ibfk_2` FOREIGN KEY (`b`) REFERENCES `p` (`id`),\n  \
         CONSTRAINT `c4_ibfk_3` FOREIGN KEY (`c`) REFERENCES `p` (`id`) ON UPDATE RESTRICT\n"
    ));
    let printed = rows(&mut adapter, "SHOW CREATE TABLE c2")[0][1].clone();
    assert!(printed.contains(&format!("  {}\n", c2.split_once(", ").unwrap().1)));
}

#[test]
fn a_parent_row_is_refused_for_the_child_constraint_first_by_name() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE p (id INT PRIMARY KEY)",
        "INSERT INTO p VALUES (1), (2), (3)",
        "CREATE TABLE aa (id INT PRIMARY KEY, pid INT, FOREIGN KEY (pid) REFERENCES p (id))",
        "CREATE TABLE zz (id INT PRIMARY KEY, pid INT, FOREIGN KEY (pid) REFERENCES p (id))",
        "CREATE TABLE mm (id INT PRIMARY KEY, pid INT, CONSTRAINT a_fk FOREIGN KEY (pid) REFERENCES p (id))",
        "CREATE TABLE nn (id INT PRIMARY KEY, pid INT, CONSTRAINT B_fk FOREIGN KEY (pid) REFERENCES p (id))",
        "CREATE TABLE two (id INT PRIMARY KEY, x INT, y INT, CONSTRAINT z_fk FOREIGN KEY (x) \
         REFERENCES p (id), CONSTRAINT b_fk2 FOREIGN KEY (y) REFERENCES p (id))",
        "INSERT INTO zz VALUES (1, 1)",
        "INSERT INTO aa VALUES (1, 1)",
        "INSERT INTO mm VALUES (1, 2)",
        "INSERT INTO nn VALUES (1, 2)",
        "INSERT INTO two VALUES (1, 3, 3)",
    ] {
        run(&mut adapter, sql);
    }
    for (sql, constraint) in [
        (
            "DELETE FROM p WHERE id = 1",
            "`d`.`aa`, CONSTRAINT `aa_ibfk_1` FOREIGN KEY (`pid`) REFERENCES `p` (`id`)",
        ),
        (
            "DELETE FROM p WHERE id = 2",
            "`d`.`nn`, CONSTRAINT `B_fk` FOREIGN KEY (`pid`) REFERENCES `p` (`id`)",
        ),
        (
            "UPDATE p SET id = 4 WHERE id = 3",
            "`d`.`two`, CONSTRAINT `b_fk2` FOREIGN KEY (`y`) REFERENCES `p` (`id`)",
        ),
    ] {
        assert_eq!(
            refused(&mut adapter, sql),
            (
                FrontendErrorKind::ParentRowReferenced,
                format!("{PARENT_ROW_WITH_CHILDREN} ({constraint})")
            ),
            "{sql}"
        );
    }
}

fn keys_printed(adapter: &mut Adapter, table: &str) -> Vec<String> {
    rows(adapter, &format!("SHOW CREATE TABLE {table}"))[0][1]
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("PRIMARY KEY")
                || line.starts_with("UNIQUE KEY")
                || line.starts_with("KEY")
        })
        .map(|line| line.trim_end_matches(',').to_owned())
        .collect()
}

#[test]
fn the_key_a_foreign_key_asks_for_is_made_where_its_clause_stands() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE p (id INT PRIMARY KEY, x INT, UNIQUE KEY (x))",
    );
    run(
        &mut adapter,
        "CREATE TABLE p6 (k INT, id INT, PRIMARY KEY (k, id))",
    );
    for (sql, table, keys) in [
        (
            "CREATE TABLE c1 (id INT PRIMARY KEY, a INT, b INT, c INT, u INT, KEY ka (a), \
             FOREIGN KEY (b) REFERENCES p (id), KEY kc (c), UNIQUE KEY uu (u))",
            "c1",
            &[
                "PRIMARY KEY (`id`)",
                "UNIQUE KEY `uu` (`u`)",
                "KEY `ka` (`a`)",
                "KEY `b` (`b`)",
                "KEY `kc` (`c`)",
            ][..],
        ),
        (
            "CREATE TABLE c2 (id INT PRIMARY KEY, a INT, FOREIGN KEY (a) REFERENCES p (id), \
             KEY ka (a))",
            "c2",
            &["PRIMARY KEY (`id`)", "KEY `ka` (`a`)"][..],
        ),
        (
            "CREATE TABLE c3 (id INT PRIMARY KEY, a INT, b INT, FOREIGN KEY (a) REFERENCES p (id), \
             KEY (a, b))",
            "c3",
            &["PRIMARY KEY (`id`)", "KEY `a` (`a`,`b`)"][..],
        ),
        (
            "CREATE TABLE c4 (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT fx FOREIGN KEY (b) \
             REFERENCES p (id), KEY ka (a))",
            "c4",
            &["PRIMARY KEY (`id`)", "KEY `fx` (`b`)", "KEY `ka` (`a`)"][..],
        ),
        (
            "CREATE TABLE c5 (id INT PRIMARY KEY, a INT, b INT, KEY b (a), \
             FOREIGN KEY (b) REFERENCES p (id), KEY kz (b, a))",
            "c5",
            &["PRIMARY KEY (`id`)", "KEY `b` (`a`)", "KEY `kz` (`b`,`a`)"][..],
        ),
        (
            "CREATE TABLE c6 (id INT PRIMARY KEY, a INT, b INT, FOREIGN KEY (b) REFERENCES p (id), \
             FOREIGN KEY (a) REFERENCES p (id), KEY kz (b))",
            "c6",
            &["PRIMARY KEY (`id`)", "KEY `a` (`a`)", "KEY `kz` (`b`)"][..],
        ),
        (
            "CREATE TABLE c7 (id INT PRIMARY KEY, a INT, b INT, FOREIGN KEY (b) REFERENCES p (x), \
             KEY ka (a), UNIQUE KEY ub (b))",
            "c7",
            &["PRIMARY KEY (`id`)", "UNIQUE KEY `ub` (`b`)", "KEY `ka` (`a`)"][..],
        ),
        (
            "CREATE TABLE c8 (id INT PRIMARY KEY, b INT, c INT, \
             FOREIGN KEY (b, c) REFERENCES p6 (k, id), KEY (b))",
            "c8",
            &["PRIMARY KEY (`id`)", "KEY `b` (`b`,`c`)", "KEY `b_2` (`b`)"][..],
        ),
        (
            "CREATE TABLE c9 (id INT PRIMARY KEY, b INT, c INT, KEY (b), \
             FOREIGN KEY (b, c) REFERENCES p6 (k, id), KEY kc (c))",
            "c9",
            &[
                "PRIMARY KEY (`id`)",
                "KEY `b` (`b`)",
                "KEY `b_2` (`b`,`c`)",
                "KEY `kc` (`c`)",
            ][..],
        ),
        (
            "CREATE TABLE c11 (id INT PRIMARY KEY, b INT, c INT, FOREIGN KEY (b) REFERENCES p (id), \
             FOREIGN KEY (b, c) REFERENCES p6 (k, id))",
            "c11",
            &["PRIMARY KEY (`id`)", "KEY `b` (`b`,`c`)"][..],
        ),
        (
            "CREATE TABLE c12 (id INT PRIMARY KEY, b INT, c INT, FOREIGN KEY (b) REFERENCES p (id), \
             KEY kc (c), FOREIGN KEY (b) REFERENCES p (x))",
            "c12",
            &["PRIMARY KEY (`id`)", "KEY `kc` (`c`)", "KEY `b` (`b`)"][..],
        ),
        (
            "CREATE TABLE c13 (id INT PRIMARY KEY, b INT, c INT UNIQUE, \
             FOREIGN KEY (c) REFERENCES p (id), KEY (c, b))",
            "c13",
            &["PRIMARY KEY (`id`)", "UNIQUE KEY `c` (`c`)", "KEY `c_2` (`c`,`b`)"][..],
        ),
        (
            "CREATE TABLE c14 (id INT PRIMARY KEY, b INT, FOREIGN KEY (id) REFERENCES p (id), \
             KEY kb (b))",
            "c14",
            &["PRIMARY KEY (`id`)", "KEY `kb` (`b`)"][..],
        ),
        (
            "CREATE TABLE c10 (id INT AUTO_INCREMENT PRIMARY KEY, a INT, b INT, \
             FOREIGN KEY (b) REFERENCES p (id), KEY ka (a))",
            "c10",
            &["PRIMARY KEY (`id`)", "KEY `b` (`b`)", "KEY `ka` (`a`)"][..],
        ),
    ] {
        run(&mut adapter, sql);
        assert_eq!(keys_printed(&mut adapter, table), keys, "{sql}");
    }
    assert_eq!(
        rows(&mut adapter, "SHOW INDEX FROM c1")
            .into_iter()
            .map(|row| row[2].clone())
            .collect::<Vec<_>>(),
        ["PRIMARY", "uu", "ka", "b", "kc"]
    );
    for sql in [
        "CREATE TABLE d1 (id INT PRIMARY KEY, a INT, b INT, FOREIGN KEY (b) REFERENCES p (id), \
         KEY b (a))",
        "CREATE TABLE d2 (id INT PRIMARY KEY, b INT, c INT, \
         FOREIGN KEY (b, c) REFERENCES p6 (k, id), KEY (b), KEY b_2 (c))",
        "CREATE TABLE d3 (id INT PRIMARY KEY, a INT, c INT, CONSTRAINT fk_x FOREIGN KEY (a) \
         REFERENCES p (id), KEY fk_x (c))",
    ] {
        assert_eq!(
            refused(&mut adapter, sql).0,
            FrontendErrorKind::DuplicateKeyName,
            "{sql}"
        );
    }
}

fn parent_warning(constraint: &str) -> (String, String) {
    (
        "1451".to_owned(),
        format!("{PARENT_ROW_WITH_CHILDREN} ({constraint})"),
    )
}

#[test]
fn update_ignore_and_delete_ignore_skip_the_rows_a_foreign_key_refuses() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE p (id INT PRIMARY KEY)",
        "CREATE TABLE c (id INT AUTO_INCREMENT PRIMARY KEY, pid INT, FOREIGN KEY (pid) REFERENCES p (id))",
        "INSERT INTO p VALUES (1), (2)",
        "INSERT INTO c (pid) VALUES (1), (1), (2), (2)",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        written(&mut adapter, "UPDATE IGNORE c SET pid = 9"),
        (0, 0, 4)
    );
    assert_eq!(warnings(&mut adapter), vec![child_warning(C_IBFK_1); 4]);
    assert_eq!(
        written(
            &mut adapter,
            "UPDATE IGNORE c SET pid = CASE WHEN id = 2 THEN 9 ELSE 2 END"
        ),
        (1, 0, 1)
    );
    assert_eq!(warnings(&mut adapter), [child_warning(C_IBFK_1)]);
    assert_eq!(
        rows(&mut adapter, "SELECT id, pid FROM c ORDER BY id"),
        [["1", "2"], ["2", "1"], ["3", "2"], ["4", "2"]]
    );

    assert_eq!(
        written(&mut adapter, "DELETE IGNORE FROM p WHERE id = 1"),
        (0, 0, 1)
    );
    assert_eq!(warnings(&mut adapter), [parent_warning(C_IBFK_1)]);
    assert_eq!(
        written(&mut adapter, "UPDATE IGNORE p SET id = id + 10"),
        (0, 0, 2)
    );
    assert_eq!(warnings(&mut adapter), vec![parent_warning(C_IBFK_1); 2]);
    run(&mut adapter, "DELETE FROM c WHERE id = 2");
    let statement = adapter
        .execute_stmt_prepare("DELETE IGNORE FROM p WHERE id > ?")
        .unwrap();
    let Ok(PreparedStatementExecutionResult::Ok(deleted)) =
        adapter.execute_stmt_execute(statement.statement_id, &integers(&[0]))
    else {
        panic!("the prepared DELETE IGNORE must skip the refused row");
    };
    assert_eq!((deleted.affected_rows, deleted.warnings), (1, 1));
    assert_eq!(warnings(&mut adapter), [parent_warning(C_IBFK_1)]);
    assert_eq!(rows(&mut adapter, "SELECT id FROM p"), [["2"]]);

    assert_eq!(
        refused(&mut adapter, "DELETE FROM p").0,
        FrontendErrorKind::ParentRowReferenced
    );
    assert_eq!(
        refused(&mut adapter, "UPDATE OR IGNORE c SET pid = 1").0,
        FrontendErrorKind::Syntax
    );
}

#[test]
fn update_ignore_skips_a_row_whose_key_collides() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE t (id INT PRIMARY KEY, u INT, UNIQUE KEY (u))",
        "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(written(&mut adapter, "UPDATE IGNORE t SET u = u + 1").0, 1);
    assert_eq!(
        written(&mut adapter, "UPDATE IGNORE t SET id = id + 1").0,
        1
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, u FROM t ORDER BY id"),
        [["1", "1"], ["2", "2"], ["4", "4"]]
    );
}

#[test]
fn a_foreign_key_mysql_cannot_make_is_refused_with_mysqls_error() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE p (id INT PRIMARY KEY, a INT, b INT, k INT, u INT UNSIGNED, s VARCHAR(10), \
         s2 VARCHAR(10) COLLATE utf8mb4_bin, dc DECIMAL(10,2), dt DATETIME, d DATE, y YEAR, \
         UNIQUE KEY uab (a, b), KEY kk (k), UNIQUE KEY uu (u), UNIQUE KEY us (s), \
         UNIQUE KEY us2 (s2), UNIQUE KEY udc (dc), UNIQUE KEY udt (dt), UNIQUE KEY ud (d), \
         UNIQUE KEY uy (y))",
        "CREATE TABLE taken (id INT PRIMARY KEY, x INT, CONSTRAINT dup FOREIGN KEY (x) REFERENCES p (id))",
    ] {
        run(&mut adapter, sql);
    }
    for (sql, kind, message) in [
        (
            "CREATE TABLE c (id INT PRIMARY KEY, x INT, FOREIGN KEY (x) REFERENCES nope (id), \
             FOREIGN KEY (zz) REFERENCES p (id))",
            FrontendErrorKind::ForeignKeyColumnMissing,
            "Key column 'zz' doesn't exist in table",
        ),
        (
            "CREATE TABLE c (id INT PRIMARY KEY, x INT, FOREIGN KEY (x, id) REFERENCES nope (id))",
            FrontendErrorKind::ForeignKeyColumnCountMismatch,
            "Incorrect foreign key definition for 'foreign key without name': Key reference and \
             table reference don't match",
        ),
        (
            "CREATE TABLE c (id INT PRIMARY KEY, x INT, CONSTRAINT dup FOREIGN KEY (x) REFERENCES nope (id))",
            FrontendErrorKind::ReferencedTableMissing,
            "Failed to open the referenced table 'nope'",
        ),
        (
            "CREATE TABLE c (id INT PRIMARY KEY, x INT, FOREIGN KEY (x) REFERENCES p (nope))",
            FrontendErrorKind::ReferencedColumnMissing,
            "Failed to add the foreign key constraint. Missing column 'nope' for constraint \
             'c_ibfk_1' in the referenced table 'p'",
        ),
        (
            "CREATE TABLE c (id INT PRIMARY KEY, x VARCHAR(3), FOREIGN KEY (x) REFERENCES p (k))",
            FrontendErrorKind::ForeignKeyColumnsIncompatible,
            "Referencing column 'x' and referenced column 'k' in foreign key constraint \
             'c_ibfk_1' are incompatible.",
        ),
        (
            "CREATE TABLE c (id INT PRIMARY KEY, x INT, FOREIGN KEY (x) REFERENCES p (k))",
            FrontendErrorKind::ReferencedKeyMissing,
            "Failed to add the foreign key constraint. Missing unique key for constraint \
             'c_ibfk_1' in the referenced table 'p'",
        ),
        (
            "CREATE TABLE c (id INT PRIMARY KEY, x INT, CONSTRAINT dup FOREIGN KEY (x) REFERENCES p (id))",
            FrontendErrorKind::DuplicateForeignKeyName,
            "Duplicate foreign key constraint name 'dup'",
        ),
    ] {
        assert_eq!(
            refused(&mut adapter, sql),
            (kind, message.to_owned()),
            "{sql}"
        );
    }
    for (columns, referenced) in [
        ("a INT, b INT", "(a, b) REFERENCES p (b, a)"),
        ("a INT", "(a) REFERENCES p (a)"),
        ("x MEDIUMINT", "(x) REFERENCES p (id)"),
        ("x BIGINT", "(x) REFERENCES p (id)"),
        ("x INT", "(x) REFERENCES p (u)"),
        ("x VARCHAR(10) COLLATE utf8mb4_bin", "(x) REFERENCES p (s)"),
        ("x VARCHAR(10)", "(x) REFERENCES p (s2)"),
        ("x DATETIME", "(x) REFERENCES p (d)"),
        ("x INT", "(x) REFERENCES p (y)"),
    ] {
        let sql =
            format!("CREATE TABLE c (id INT PRIMARY KEY, {columns}, FOREIGN KEY {referenced})");
        assert!(adapter.execute_query(&sql).is_err(), "{sql}");
    }
    for (columns, referenced) in [
        ("a INT, b INT", "(a, b) REFERENCES p (a, b)"),
        ("x INT UNSIGNED", "(x) REFERENCES p (u)"),
        ("x CHAR(20)", "(x) REFERENCES p (s)"),
        ("x DECIMAL(12,4)", "(x) REFERENCES p (dc)"),
        ("x TIMESTAMP NULL", "(x) REFERENCES p (dt)"),
        ("x TIME(3)", "(x) REFERENCES p (dt)"),
        ("x TINYINT UNSIGNED", "(x) REFERENCES p (y)"),
        ("x INT NOT NULL", "(x) REFERENCES p (id)"),
    ] {
        run(
            &mut adapter,
            &format!("CREATE TABLE c (id INT PRIMARY KEY, {columns}, FOREIGN KEY {referenced})"),
        );
        run(&mut adapter, "DROP TABLE c");
    }
    run(
        &mut adapter,
        "CREATE TABLE selfref (id INT PRIMARY KEY, up INT, FOREIGN KEY (up) REFERENCES selfref (id))",
    );

    run(&mut adapter, "SET foreign_key_checks = 0");
    run(
        &mut adapter,
        "CREATE TABLE later (id INT PRIMARY KEY, x INT, FOREIGN KEY (x) REFERENCES nope (id))",
    );
    assert_eq!(
        refused(
            &mut adapter,
            "CREATE TABLE c (id INT PRIMARY KEY, x INT, FOREIGN KEY (x) REFERENCES p (k))"
        )
        .0,
        FrontendErrorKind::ReferencedKeyMissing
    );
    run(&mut adapter, "SET foreign_key_checks = 1");

    run(
        &mut adapter,
        "CREATE TABLE c (id INT PRIMARY KEY, x INT, s VARCHAR(10))",
    );
    for (sql, kind) in [
        (
            "ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES nope (id)",
            FrontendErrorKind::ReferencedTableMissing,
        ),
        (
            "ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES p (k)",
            FrontendErrorKind::ReferencedKeyMissing,
        ),
        (
            "ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES p (s)",
            FrontendErrorKind::ForeignKeyColumnsIncompatible,
        ),
        (
            "ALTER TABLE c ADD FOREIGN KEY (zz) REFERENCES p (id)",
            FrontendErrorKind::ForeignKeyColumnMissing,
        ),
        (
            "ALTER TABLE c ADD CONSTRAINT dup FOREIGN KEY (x) REFERENCES p (id)",
            FrontendErrorKind::DuplicateForeignKeyName,
        ),
    ] {
        assert_eq!(refused(&mut adapter, sql).0, kind, "{sql}");
    }
    run(
        &mut adapter,
        "ALTER TABLE c ADD CONSTRAINT mine FOREIGN KEY (x) REFERENCES p (id)",
    );
    assert_eq!(
        refused(
            &mut adapter,
            "ALTER TABLE c ADD CONSTRAINT mine FOREIGN KEY (s) REFERENCES p (s)"
        )
        .0,
        FrontendErrorKind::DuplicateForeignKeyName
    );
}
