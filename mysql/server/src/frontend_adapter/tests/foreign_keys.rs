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
