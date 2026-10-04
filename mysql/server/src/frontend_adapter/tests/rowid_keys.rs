//! A table whose one integer primary key is the engine's rowid.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// A key a row leaves out is 1364 and a NULL key 1048, whichever way the row
/// is written; the engine never numbers the row itself, as it does a counted
/// table's. A key written as a word of digits is that number, and one the
/// column cannot hold is refused as any other `INT` column's value is.
#[test]
fn a_key_is_given_by_every_row_and_never_numbered_by_the_engine() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(10))",
    );
    for (sql, refused) in [
        (
            "INSERT INTO t (v) VALUES ('a')",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "INSERT INTO t (v) VALUES ('a'), ('b')",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "INSERT INTO t () VALUES ()",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "INSERT INTO t SET v = 'z'",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "REPLACE INTO t (v) VALUES ('r')",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "INSERT INTO t (v) VALUES ('a') ON DUPLICATE KEY UPDATE v = 'q'",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "INSERT INTO t (v) SELECT 'sel'",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "INSERT INTO t VALUES (NULL, 'a')",
            FrontendErrorKind::NotNullViolation,
        ),
        (
            "INSERT INTO t VALUES (1, 'x'), (NULL, 'y')",
            FrontendErrorKind::NotNullViolation,
        ),
        (
            "REPLACE INTO t VALUES (NULL, 'r')",
            FrontendErrorKind::NotNullViolation,
        ),
        (
            "INSERT INTO t SELECT NULL, 'sel'",
            FrontendErrorKind::NotNullViolation,
        ),
        (
            "INSERT INTO t VALUES (2147483648, 'big')",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO t VALUES (-2147483649, 'small')",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO t VALUES ('abc', 'word')",
            FrontendErrorKind::IncorrectValue,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    for sql in [
        "INSERT INTO t VALUES ('12', 's12')",
        "INSERT INTO t VALUES (0, 'zero')",
        "INSERT INTO t VALUES (-5, 'neg')",
        "INSERT INTO t VALUES (1e3, 'float')",
    ] {
        assert_eq!(written(&mut adapter, sql), (1, 0), "{sql}");
    }
    assert_eq!(
        adapter
            .execute_query("INSERT INTO t VALUES (12, 'dup')")
            .map(|_| ()),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "0");
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM t"),
        [
            ["-5", "neg"],
            ["0", "zero"],
            ["12", "s12"],
            ["1000", "float"],
        ]
    );

    // Bound the same way, a NULL key is 1048 and a missing one 1364.
    let statement = adapter
        .execute_stmt_prepare("INSERT INTO t (id, v) VALUES (?, ?)")
        .unwrap();
    assert_eq!(
        adapter
            .execute_stmt_execute(statement.statement_id, &a_null_and_a_word("b"))
            .map(|_| ()),
        Err(FrontendErrorKind::NotNullViolation)
    );
    let statement = adapter
        .execute_stmt_prepare("INSERT INTO t (v) VALUES (?)")
        .unwrap();
    assert_eq!(
        adapter
            .execute_stmt_execute(statement.statement_id, &a_word("b"))
            .map(|_| ()),
        Err(FrontendErrorKind::MissingRequiredDefault)
    );
}

/// An `UPDATE` or an upsert moving a key keeps MySQL's rules for it: NULL is
/// 1048, a key already taken 1062, and a key written as a word of digits is
/// that number.
#[test]
fn a_key_moved_by_an_update_or_an_upsert_keeps_the_rules_of_a_written_key() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(10), n INT)",
    );
    run(
        &mut adapter,
        "INSERT INTO t VALUES (3, 'c', 30), (1, 'z', 10), (2, 'b', 20), (-1, 'm', 5), (10, 'a', 1)",
    );
    for (sql, refused) in [
        (
            "UPDATE t SET id = NULL WHERE id = 1",
            FrontendErrorKind::NotNullViolation,
        ),
        (
            "UPDATE t SET id = 2 WHERE id = 1",
            FrontendErrorKind::ConstraintViolation,
        ),
        (
            "UPDATE t SET id = 3000000000 WHERE id = 1",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO t VALUES (2, 'dup', 0) ON DUPLICATE KEY UPDATE id = NULL",
            FrontendErrorKind::NotNullViolation,
        ),
        (
            "INSERT INTO t VALUES (2, 'dup', 0) ON DUPLICATE KEY UPDATE id = 3",
            FrontendErrorKind::ConstraintViolation,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    for (sql, affected) in [
        (
            "INSERT INTO t VALUES (1, 'dup', 0) ON DUPLICATE KEY UPDATE n = n + 100",
            2,
        ),
        (
            "INSERT INTO t VALUES (1, 'dup', 0) ON DUPLICATE KEY UPDATE id = 50",
            2,
        ),
        ("REPLACE INTO t VALUES (3, 'rep', 0)", 2),
        ("UPDATE t SET id = '7' WHERE id = 10", 1),
    ] {
        assert_eq!(written(&mut adapter, sql), (affected, 0), "{sql}");
    }
    assert_eq!(
        rows(&mut adapter, "SELECT id, v, n FROM t"),
        [
            ["-1", "m", "5"],
            ["2", "b", "20"],
            ["3", "rep", "0"],
            ["7", "a", "1"],
            ["50", "z", "110"],
        ]
    );
}

/// A `BEFORE UPDATE` trigger leaves the key's rules where they were: NULL is
/// 1048, a word that is no number 1366 and a number past the type 1264, and a
/// key written as a word of digits moves the row, the trigger's change with it.
#[test]
fn a_key_moved_under_a_before_update_trigger_keeps_its_rules() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(10), n INT)",
    );
    run(
        &mut adapter,
        "INSERT INTO t VALUES (1, 'a', 1), (2, 'b', 2)",
    );
    run(
        &mut adapter,
        "CREATE TRIGGER tu BEFORE UPDATE ON t FOR EACH ROW SET NEW.n = NEW.n + 100",
    );
    for (sql, refused) in [
        (
            "UPDATE t SET id = NULL WHERE id = 1",
            FrontendErrorKind::NotNullViolation,
        ),
        (
            "UPDATE t SET id = 'x' WHERE id = 2",
            FrontendErrorKind::IncorrectValue,
        ),
        (
            "UPDATE t SET id = 3000000000 WHERE id = 2",
            FrontendErrorKind::OutOfRange,
        ),
    ] {
        assert_eq!(
            adapter.execute_query(sql).map(|_| ()),
            Err(refused),
            "{sql}"
        );
    }
    assert_eq!(
        written(&mut adapter, "UPDATE t SET id = '7' WHERE id = 1"),
        (1, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v, n FROM t"),
        [["2", "b", "2"], ["7", "a", "101"]]
    );
}

/// The rows of a table read without an `ORDER BY` come in key order, as
/// InnoDB keeps them, and a read through a plain index comes in the order of
/// the index and then the key, NULLs in a unique index included.
#[test]
fn rows_read_without_an_order_come_in_key_order() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(10), n INT)",
    );
    run(
        &mut adapter,
        "INSERT INTO t VALUES (3, 'c', 30), (1, 'z', 10), (2, 'b', 20), (-1, 'm', 5), (10, 'a', 1)",
    );
    let in_key_order = [["-1", "m"], ["1", "z"], ["2", "b"], ["3", "c"], ["10", "a"]];
    assert_eq!(rows(&mut adapter, "SELECT id, v FROM t"), in_key_order);
    run(&mut adapter, "CREATE INDEX iv ON t (v)");
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM t WHERE n > 0"),
        in_key_order
    );
    run(
        &mut adapter,
        "CREATE TABLE un (id INT PRIMARY KEY, u INT UNIQUE)",
    );
    for id in [30, 10, 20] {
        run(&mut adapter, &format!("INSERT INTO un VALUES ({id}, NULL)"));
    }
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM un WHERE u IS NULL"),
        [["10"], ["20"], ["30"]]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM t WHERE v > ''"),
        [["10", "a"], ["2", "b"], ["3", "c"], ["-1", "m"], ["1", "z"],]
    );
}

/// Every integer type a key may be the rowid as is printed and reported as
/// it was declared, and holds what its type holds.
#[test]
fn a_key_of_each_integer_type_reads_back_as_declared() {
    let (_directory, mut adapter) = adapter();
    for (table, declared, printed, too_large) in [
        ("s", "SMALLINT", "smallint", "32768"),
        ("ti", "TINYINT", "tinyint", "128"),
        ("mi", "MEDIUMINT", "mediumint", "8388608"),
        ("ig", "INTEGER", "int", "2147483648"),
        ("bi", "BIGINT", "bigint", "9223372036854775808"),
    ] {
        run(
            &mut adapter,
            &format!("CREATE TABLE {table} (id {declared} PRIMARY KEY, v INT)"),
        );
        assert_eq!(
            rows(&mut adapter, &format!("SHOW CREATE TABLE {table}"))[0][1],
            format!(
                "CREATE TABLE `{table}` (\n  `id` {printed} NOT NULL,\n  `v` int DEFAULT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
            )
        );
        assert_eq!(
            rows(&mut adapter, &format!("DESCRIBE {table}"))[0],
            ["id", printed, "NO", "PRI", "NULL", ""]
        );
        assert!(
            adapter
                .execute_query(&format!("INSERT INTO {table} VALUES ({too_large}, 1)"))
                .is_err(),
            "{declared}"
        );
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, COLUMN_KEY, EXTRA, COLUMN_TYPE FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND COLUMN_KEY = 'PRI' ORDER BY TABLE_NAME"
        ),
        [
            ["bi", "PRI", "", "bigint"],
            ["ig", "PRI", "", "int"],
            ["mi", "PRI", "", "mediumint"],
            ["s", "PRI", "", "smallint"],
            ["ti", "PRI", "", "tinyint"],
        ]
    );
    run(
        &mut adapter,
        "INSERT INTO bi VALUES (9223372036854775807, 1), (-9223372036854775808, 1)",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM bi"),
        [["-9223372036854775808"], ["9223372036854775807"]]
    );
}

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([211; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

/// The affected rows and the id one write reports.
fn written(adapter: &mut Adapter, sql: &str) -> (u64, u64) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => (result.affected_rows, result.last_insert_id),
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<String>> {
    let result = adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    let CommandExecutionResult::ResultSet(result) = result else {
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
                .collect()
        })
        .collect()
}

fn one(adapter: &mut Adapter, sql: &str) -> String {
    rows(adapter, sql)[0][0].clone()
}

/// The null bitmap marking the first of two parameters NULL, the
/// new-parameters flag, two VAR_STRING types and the one word.
fn a_null_and_a_word(word: &str) -> Vec<u8> {
    let mut payload = vec![0b01, 1, MYSQL_TYPE_VAR_STRING, 0, MYSQL_TYPE_VAR_STRING, 0];
    payload.push(u8::try_from(word.len()).unwrap());
    payload.extend_from_slice(word.as_bytes());
    payload
}

/// One VAR_STRING parameter holding `word`.
fn a_word(word: &str) -> Vec<u8> {
    let mut payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0];
    payload.push(u8::try_from(word.len()).unwrap());
    payload.extend_from_slice(word.as_bytes());
    payload
}
