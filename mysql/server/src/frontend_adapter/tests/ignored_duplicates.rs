//! `IGNORE` skipping a row whose key collides with one already stored, and
//! the warning 1062 MySQL raises for each row it skips.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([193; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result,
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

/// The message of each warning the last statement raised, after checking
/// each is a `Warning` 1062.
fn duplicates(adapter: &mut Adapter) -> Vec<String> {
    rows(adapter, "SHOW WARNINGS")
        .into_iter()
        .map(|warning| {
            assert_eq!(
                warning[..2],
                [Some("Warning".to_owned()), Some("1062".to_owned())]
            );
            warning[2].clone().unwrap()
        })
        .collect()
}

fn table_k(adapter: &mut Adapter) {
    run(
        adapter,
        "CREATE TABLE k (id INT PRIMARY KEY, u INT, s VARCHAR(20), n INT, UNIQUE KEY uk_u (u), UNIQUE KEY uk_sn (s, n))",
    );
    run(
        adapter,
        "INSERT INTO k VALUES (1, 10, 'a', 1), (2, 20, 'b', 2)",
    );
}

/// Each row skipped is warned about once, naming the key it collided with
/// and the row's value for it, and the statement counts only the rows it
/// wrote.
#[test]
fn insert_ignore_warns_1062_for_each_row_it_skips() {
    let (_directory, mut adapter) = adapter();
    table_k(&mut adapter);
    let written = run(
        &mut adapter,
        "INSERT IGNORE INTO k VALUES (1, 30, 'c', 3), (3, 20, 'd', 4), (4, 40, 'a', 1), (5, 50, 'e', 5)",
    );
    assert_eq!((written.affected_rows, written.warnings), (1, 3));
    assert_eq!(
        duplicates(&mut adapter),
        [
            "Duplicate entry '1' for key 'k.PRIMARY'",
            "Duplicate entry '20' for key 'k.uk_u'",
            "Duplicate entry 'a-1' for key 'k.uk_sn'",
        ]
    );
    assert_eq!(
        rows(&mut adapter, "SHOW COUNT(*) WARNINGS"),
        [[Some("3".to_owned())]]
    );

    // The warnings go with the statement that raised them.
    run(&mut adapter, "INSERT INTO k VALUES (6, 60, 'f', 6)");
    assert!(duplicates(&mut adapter).is_empty());
}

/// `INSERT IGNORE ... SELECT` skips and warns the same way, with its columns
/// named or not.
#[test]
fn insert_ignore_select_skips_and_warns_with_its_columns_named_or_not() {
    let (_directory, mut adapter) = adapter();
    table_k(&mut adapter);
    let copied = run(
        &mut adapter,
        "INSERT IGNORE INTO k (id, u, s, n) SELECT id + 100, u, s, n + 100 FROM k",
    );
    assert_eq!((copied.affected_rows, copied.warnings), (0, 2));
    assert_eq!(
        duplicates(&mut adapter),
        [
            "Duplicate entry '10' for key 'k.uk_u'",
            "Duplicate entry '20' for key 'k.uk_u'",
        ]
    );
    let copied = run(
        &mut adapter,
        "INSERT IGNORE INTO k SELECT id + 200, u + 200, s, n FROM k WHERE id < 100",
    );
    assert_eq!((copied.affected_rows, copied.warnings), (0, 2));
    assert_eq!(
        duplicates(&mut adapter),
        [
            "Duplicate entry 'a-1' for key 'k.uk_sn'",
            "Duplicate entry 'b-2' for key 'k.uk_sn'",
        ]
    );
    let copied = run(
        &mut adapter,
        "INSERT IGNORE INTO k SELECT id + 300, u + 300, s, n + 300 FROM k WHERE id < 100",
    );
    assert_eq!((copied.affected_rows, copied.warnings), (2, 0));
    assert_eq!(
        rows(&mut adapter, "SELECT id, u FROM k ORDER BY id"),
        [
            [Some("1".to_owned()), Some("10".to_owned())],
            [Some("2".to_owned()), Some("20".to_owned())],
            [Some("301".to_owned()), Some("310".to_owned())],
            [Some("302".to_owned()), Some("320".to_owned())],
        ]
    );
}

/// `UPDATE IGNORE` leaves a row whose new key collides as it stood and warns
/// for it, over one table and over a join alike; a joined one reads the
/// changed table's own columns named through it.
#[test]
fn update_ignore_warns_1062_over_one_table_and_over_a_join() {
    let (_directory, mut adapter) = adapter();
    table_k(&mut adapter);
    run(&mut adapter, "INSERT INTO k VALUES (5, 50, 'e', 5)");
    let updated = run(
        &mut adapter,
        "UPDATE IGNORE k SET u = 10 WHERE id IN (2, 5)",
    );
    assert_eq!((updated.affected_rows, updated.warnings), (0, 2));
    assert_eq!(
        duplicates(&mut adapter),
        [
            "Duplicate entry '10' for key 'k.uk_u'",
            "Duplicate entry '10' for key 'k.uk_u'",
        ]
    );

    run(&mut adapter, "CREATE TABLE j (id INT PRIMARY KEY, kid INT)");
    run(&mut adapter, "INSERT INTO j VALUES (1, 2), (2, 5), (3, 1)");
    let updated = run(
        &mut adapter,
        "UPDATE IGNORE k JOIN j ON j.kid = k.id SET k.u = 50",
    );
    assert_eq!((updated.affected_rows, updated.warnings), (0, 2));
    assert_eq!(
        duplicates(&mut adapter),
        [
            "Duplicate entry '50' for key 'k.uk_u'",
            "Duplicate entry '50' for key 'k.uk_u'",
        ]
    );
    let updated = run(
        &mut adapter,
        "UPDATE IGNORE k JOIN j ON j.kid = k.id SET k.u = k.u + 1000",
    );
    assert_eq!((updated.affected_rows, updated.warnings), (3, 0));
    assert_eq!(
        rows(&mut adapter, "SELECT id, u FROM k ORDER BY id"),
        [
            [Some("1".to_owned()), Some("1010".to_owned())],
            [Some("2".to_owned()), Some("1020".to_owned())],
            [Some("5".to_owned()), Some("1050".to_owned())],
        ]
    );
}

/// The value is written as the column holds it: a word as the row spelled
/// it, a day and a moment in their own form, a `DECIMAL` to its scale, a
/// double the way MySQL writes one, bytes past printable ASCII as `\xHH`,
/// and anything longer cut at 64 characters.
#[test]
fn the_warning_writes_the_value_as_its_column_holds_it() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE w (code VARCHAR(10) NOT NULL, d DATE, dt DATETIME, x DECIMAL(5,2), b VARBINARY(4), PRIMARY KEY (code), UNIQUE KEY (d), UNIQUE KEY (dt), UNIQUE KEY (x), UNIQUE KEY (b))",
    );
    run(
        &mut adapter,
        "INSERT INTO w VALUES ('Abc', '2024-01-02', '2024-01-02 03:04:05', 1.5, X'00FF')",
    );
    let skipped = run(
        &mut adapter,
        "INSERT IGNORE INTO w VALUES ('abc', '2024-02-01', '2024-02-01 00:00:00', 2, X'01'), ('z1', '2024-01-02', '2024-02-02 00:00:00', 3, X'02'), ('z2', '2024-02-03', '2024-01-02 03:04:05', 4, X'03'), ('z3', '2024-02-04', '2024-02-04 00:00:00', 1.50, X'04'), ('z4', '2024-02-05', '2024-02-05 00:00:00', 5, X'00FF')",
    );
    assert_eq!((skipped.affected_rows, skipped.warnings), (0, 5));
    assert_eq!(
        duplicates(&mut adapter),
        [
            "Duplicate entry 'abc' for key 'w.PRIMARY'",
            "Duplicate entry '2024-01-02' for key 'w.d'",
            "Duplicate entry '2024-01-02 03:04:05' for key 'w.dt'",
            "Duplicate entry '1.50' for key 'w.x'",
            "Duplicate entry '\\x00\\xFF' for key 'w.b'",
        ]
    );

    run(
        &mut adapter,
        "CREATE TABLE e (id BIGINT UNSIGNED PRIMARY KEY, f DOUBLE, t TIME, UNIQUE KEY uf (f), UNIQUE KEY ut (t))",
    );
    run(
        &mut adapter,
        "INSERT INTO e VALUES (18446744073709551615, 1.5, '10:00:00'), (2, 2.25e10, '11:00:00')",
    );
    run(
        &mut adapter,
        "INSERT IGNORE INTO e VALUES (18446744073709551615, 7, '07:00:00'), (5, 1.5, '05:00:00'), (6, 2.25e10, '06:00:00'), (7, 8, '10:00')",
    );
    assert_eq!(
        duplicates(&mut adapter),
        [
            "Duplicate entry '18446744073709551615' for key 'e.PRIMARY'",
            "Duplicate entry '1.5' for key 'e.uf'",
            "Duplicate entry '22500000000' for key 'e.uf'",
            "Duplicate entry '10:00:00' for key 'e.ut'",
        ]
    );

    run(
        &mut adapter,
        "CREATE TABLE lng (s VARCHAR(200) PRIMARY KEY)",
    );
    run(
        &mut adapter,
        &format!("INSERT INTO lng VALUES ('{}')", "x".repeat(100)),
    );
    run(
        &mut adapter,
        &format!("INSERT IGNORE INTO lng VALUES ('{}')", "x".repeat(100)),
    );
    assert_eq!(
        duplicates(&mut adapter),
        [format!(
            "Duplicate entry '{}' for key 'lng.PRIMARY'",
            "x".repeat(64)
        )]
    );
}

/// A table counting its own ids warns the same way, rows written one at a
/// time or not, and a prepared statement does too.
#[test]
fn a_counted_table_and_a_prepared_statement_warn_the_same_way() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE ai (id INT AUTO_INCREMENT PRIMARY KEY, u INT UNIQUE)",
    );
    run(&mut adapter, "INSERT INTO ai (u) VALUES (1)");
    let written = run(&mut adapter, "INSERT IGNORE INTO ai (u) VALUES (1), (2)");
    assert_eq!((written.affected_rows, written.warnings), (1, 1));
    assert_eq!(
        duplicates(&mut adapter),
        ["Duplicate entry '1' for key 'ai.u'"]
    );

    let statement = adapter
        .execute_stmt_prepare("INSERT IGNORE INTO ai (id, u) VALUES (2, 3)")
        .unwrap();
    match adapter.execute_stmt_execute(statement.statement_id, &[]) {
        Ok(PreparedStatementExecutionResult::Ok(result)) => {
            assert_eq!((result.affected_rows, result.warnings), (0, 1));
        }
        other => panic!("the prepared INSERT IGNORE answered {other:?}"),
    }
    adapter.execute_stmt_close(statement.statement_id);
    assert_eq!(
        duplicates(&mut adapter),
        ["Duplicate entry '2' for key 'ai.PRIMARY'"]
    );
}
