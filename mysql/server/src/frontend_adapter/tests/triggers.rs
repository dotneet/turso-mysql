//! Triggers: what their bodies may write, and how they read back.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("root"));
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("probe").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([153; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("probe").unwrap();
    for sql in [
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(30))",
        "CREATE TABLE posts (id INT PRIMARY KEY, owner_id INT, title VARCHAR(20) NOT NULL)",
        "CREATE TABLE audit (msg VARCHAR(12) NOT NULL)",
        "CREATE TABLE counters (id INT PRIMARY KEY, n INT NOT NULL)",
        "INSERT INTO counters VALUES (7, 0), (8, 5)",
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
            AccountId::from_bytes([153; 32]),
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

fn triggers(adapter: &mut Adapter) {
    for sql in [
        "CREATE TRIGGER t2 AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (CONCAT('new ', NEW.name))",
        "CREATE TRIGGER t4 AFTER INSERT ON posts FOR EACH ROW UPDATE counters SET n = n + 1 WHERE id = NEW.owner_id",
        "CREATE TRIGGER t6 AFTER UPDATE ON posts FOR EACH ROW\nBEGIN\n  INSERT INTO audit (msg) VALUES (CONCAT(OLD.title, '>', NEW.title));\nEND",
        "CREATE TRIGGER t7 AFTER DELETE ON posts FOR EACH ROW UPDATE counters SET n = n - 1 WHERE id = OLD.owner_id;",
    ] {
        run(adapter, sql);
    }
}

/// MySQL keeps a trigger's body as it was written and prints it back that
/// way. The host printed differs as it does for every view and trigger: MySQL
/// prints the definer's, `localhost` for the measured `root`.
#[test]
fn a_trigger_body_is_kept_as_it_was_written() {
    let (directory, mut adapter) = adapter();
    triggers(&mut adapter);
    let shown = |adapter: &mut Adapter, trigger: &str| {
        rows(adapter, &format!("SHOW CREATE TRIGGER {trigger}"))[0]
            .split('|')
            .nth(2)
            .unwrap()
            .to_owned()
    };
    let expected_t6 = "CREATE DEFINER=`root`@`%` TRIGGER `t6` AFTER UPDATE ON `posts` FOR EACH ROW BEGIN\n  INSERT INTO audit (msg) VALUES (CONCAT(OLD.title, '>', NEW.title));\nEND";
    assert_eq!(
        shown(&mut adapter, "t2"),
        "CREATE DEFINER=`root`@`%` TRIGGER `t2` AFTER INSERT ON `users` FOR EACH ROW INSERT INTO audit (msg) VALUES (CONCAT('new ', NEW.name))"
    );
    assert_eq!(shown(&mut adapter, "t6"), expected_t6);
    // By table, then `INSERT`, `UPDATE` and `DELETE`, each body without the
    // `;` that ended its statement.
    let listed = rows(&mut adapter, "SHOW TRIGGERS")
        .into_iter()
        .map(|row| row.split('|').take(5).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            "t4|INSERT|posts|UPDATE counters SET n = n + 1 WHERE id = NEW.owner_id|AFTER",
            "t6|UPDATE|posts|BEGIN\n  INSERT INTO audit (msg) VALUES (CONCAT(OLD.title, '>', NEW.title));\nEND|AFTER",
            "t7|DELETE|posts|UPDATE counters SET n = n - 1 WHERE id = OLD.owner_id|AFTER",
            "t2|INSERT|users|INSERT INTO audit (msg) VALUES (CONCAT('new ', NEW.name))|AFTER",
        ]
    );

    let mut adapter = reopened(&directory, adapter);
    assert_eq!(shown(&mut adapter, "t6"), expected_t6);
    run(&mut adapter, "INSERT INTO users VALUES (1, 'Ann')");
    assert_eq!(rows(&mut adapter, "SELECT msg FROM audit"), ["new Ann"]);
}

/// What a trigger writes is held to the column it lands in the way any write
/// is, and a trigger failing takes the statement that fired it back.
#[test]
fn a_trigger_body_writes_what_mysql_writes() {
    let (_directory, mut adapter) = adapter();
    triggers(&mut adapter);
    run(&mut adapter, "INSERT INTO users VALUES (1, 'Ann')");
    // `new Alexandra` is thirteen characters, and `CONCAT` over NULL is NULL.
    assert_eq!(
        adapter.execute_query("INSERT INTO users VALUES (2, 'Alexandra')"),
        Err(FrontendErrorKind::DataTooLong)
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO users VALUES (3, NULL)"),
        Err(FrontendErrorKind::NotNullViolation)
    );
    assert_eq!(rows(&mut adapter, "SELECT id, name FROM users"), ["1|Ann"]);

    run(
        &mut adapter,
        "INSERT INTO posts VALUES (1, 7, 'a'), (2, 7, 'b'), (3, 8, 'c')",
    );
    run(&mut adapter, "UPDATE posts SET title = 'z' WHERE id = 1");
    run(&mut adapter, "DELETE FROM posts WHERE id = 3");
    assert_eq!(
        rows(&mut adapter, "SELECT msg FROM audit ORDER BY msg"),
        ["a>z", "new Ann"]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, n FROM counters ORDER BY id"),
        ["7|2", "8|5"]
    );
}

#[test]
fn a_trigger_body_this_cannot_follow_is_refused() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE amounts (id INT PRIMARY KEY, total DECIMAL(8,2), seen DATETIME)",
    );
    for sql in [
        // A `BEFORE` trigger can change the row before it is written.
        "CREATE TRIGGER b BEFORE INSERT ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.name)",
        // 1363 and 1442 in MySQL.
        "CREATE TRIGGER b AFTER DELETE ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.name)",
        "CREATE TRIGGER b AFTER INSERT ON users FOR EACH ROW UPDATE users SET name = 'x' WHERE id = NEW.id",
        // A word into a whole number, a number joined into one, and columns
        // whose value the engine writes out another way.
        "CREATE TRIGGER b AFTER INSERT ON users FOR EACH ROW UPDATE counters SET n = 'x' WHERE id = NEW.id",
        "CREATE TRIGGER b AFTER INSERT ON users FOR EACH ROW UPDATE counters SET n = CONCAT(NEW.id, '1') WHERE id = NEW.id",
        "CREATE TRIGGER b AFTER INSERT ON amounts FOR EACH ROW INSERT INTO audit (msg) VALUES (CONCAT('t ', NEW.total))",
        "CREATE TRIGGER b AFTER INSERT ON amounts FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.seen)",
        "CREATE TRIGGER b AFTER INSERT ON users FOR EACH ROW UPDATE counters SET n = 1 WHERE id = NEW.name",
        // A column no table has, and a table that is not there.
        "CREATE TRIGGER b AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.missing)",
        "CREATE TRIGGER b AFTER INSERT ON users FOR EACH ROW INSERT INTO nope (msg) VALUES (NEW.name)",
        "CREATE TRIGGER b AFTER INSERT ON nope FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.name)",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert!(rows(&mut adapter, "SHOW TRIGGERS").is_empty());

    // MySQL runs a second trigger for one table, event and timing after the
    // first, in the order they were made, which is not kept here.
    run(
        &mut adapter,
        "CREATE TRIGGER t2 AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.name)",
    );
    assert!(adapter
        .execute_query(
            "CREATE TRIGGER t3 AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (msg) VALUES ('x')"
        )
        .is_err());
    run(
        &mut adapter,
        "CREATE TRIGGER t3 AFTER UPDATE ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.name)",
    );
}

fn before_triggers(adapter: &mut Adapter) {
    for sql in [
        "CREATE TABLE n1 (id INT PRIMARY KEY, title VARCHAR(20) NOT NULL, slug VARCHAR(10) NOT NULL, n INT, updated DATETIME, UNIQUE KEY (slug))",
        "CREATE TRIGGER b1 BEFORE INSERT ON n1 FOR EACH ROW SET NEW.slug = CONCAT(NEW.title, '-x')",
        "CREATE TRIGGER b2 BEFORE UPDATE ON n1 FOR EACH ROW SET NEW.n = OLD.n + 1, NEW.slug = NEW.title",
    ] {
        run(adapter, sql);
    }
}

/// Measured on MySQL 8.4.11: a `BEFORE` trigger changes the row before it is
/// written, and the row is then held to its columns as it stands — a `NOT
/// NULL` column the statement left out or gave NULL is taken from the
/// trigger, a value the trigger makes too long is 1406, and a `UNIQUE` key
/// over a column only the trigger writes is 1062.
#[test]
fn a_before_trigger_sets_the_row_it_runs_for() {
    let (directory, mut adapter) = adapter();
    before_triggers(&mut adapter);
    run(&mut adapter, "INSERT INTO n1 (id, title) VALUES (1, 'a')");
    run(
        &mut adapter,
        "INSERT INTO n1 (id, title, slug) VALUES (2, 'b', NULL)",
    );
    for (sql, error) in [
        (
            "INSERT INTO n1 (id, title) VALUES (4, 'dddddddddd')",
            FrontendErrorKind::DataTooLong,
        ),
        (
            "INSERT INTO n1 (id, title) VALUES (5, 'a')",
            FrontendErrorKind::ConstraintViolation,
        ),
        // MySQL holds a value the statement writes to its column before the
        // trigger writes over it — this one is 1406 there — and here a value
        // is held to its column only as the row is written.
        (
            "INSERT INTO n1 (id, title, slug) VALUES (3, 'c', 'zzzzzzzzzzzzzzzz')",
            FrontendErrorKind::Unsupported,
        ),
        (
            "UPDATE n1 SET n = 7 WHERE id = 1",
            FrontendErrorKind::Unsupported,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(error), "{sql}");
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, title, slug, n FROM n1 ORDER BY id"
        ),
        ["1|a|a-x|NULL", "2|b|b-x|NULL"]
    );

    // A row whose every column comes out as it was is not counted as changed.
    assert_eq!(
        run(&mut adapter, "UPDATE n1 SET title = 'zz' WHERE id = 2").affected_rows,
        1
    );
    assert_eq!(
        run(&mut adapter, "UPDATE n1 SET title = 'zz' WHERE id = 2").affected_rows,
        0
    );
    run(&mut adapter, "UPDATE n1 SET title = 'a' WHERE id = 2");
    assert_eq!(
        adapter.execute_query("UPDATE n1 SET title = 'a' WHERE id = 1"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, title, slug, n FROM n1 ORDER BY id"
        ),
        ["1|a|a-x|NULL", "2|a|a|NULL"]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM n1 WHERE slug = 'a'"),
        ["2"]
    );

    // A prepared statement is held to the same rule.
    assert_eq!(
        adapter
            .execute_stmt_prepare("INSERT INTO n1 (id, title, slug) VALUES (?, ?, ?)")
            .map(|_| ()),
        Err(FrontendErrorKind::Unsupported)
    );
    let prepared = adapter
        .execute_stmt_prepare("INSERT INTO n1 (id, title) VALUES (6, 'p')")
        .unwrap();
    adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap();
    adapter.execute_stmt_close(prepared.statement_id);
    assert_eq!(
        rows(&mut adapter, "SELECT slug FROM n1 WHERE id = 6"),
        ["p-x"]
    );

    let mut adapter = reopened(&directory, adapter);
    run(&mut adapter, "INSERT INTO n1 (id, title) VALUES (3, 'zz')");
    assert_eq!(
        rows(&mut adapter, "SELECT slug FROM n1 WHERE id = 3"),
        ["zz-x"]
    );
    let listed = rows(&mut adapter, "SHOW TRIGGERS")
        .into_iter()
        .map(|row| row.split('|').take(5).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            "b1|INSERT|n1|SET NEW.slug = CONCAT(NEW.title, '-x')|BEFORE",
            "b2|UPDATE|n1|SET NEW.n = OLD.n + 1, NEW.slug = NEW.title|BEFORE",
        ]
    );
}

/// A trigger reading the clock runs under the time zone of the session whose
/// statement fires it, which the engine's clock does not follow.
#[test]
fn a_before_trigger_stamps_the_moment_a_row_is_changed() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE stamped (id INT PRIMARY KEY, title VARCHAR(20), updated DATETIME)",
        "CREATE TRIGGER s1 BEFORE UPDATE ON stamped FOR EACH ROW SET NEW.updated = NOW()",
        "INSERT INTO stamped (id, title) VALUES (1, 'a')",
        "UPDATE stamped SET title = 'b' WHERE id = 1",
    ] {
        run(&mut adapter, sql);
    }
    let [stamp] = rows(&mut adapter, "SELECT updated FROM stamped")
        .try_into()
        .unwrap();
    assert_eq!(stamp.len(), "2026-09-28 00:00:00".len(), "{stamp}");
    run(&mut adapter, "SET time_zone = '+09:00'");
    assert_eq!(
        adapter.execute_query("UPDATE stamped SET title = 'c' WHERE id = 1"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[test]
fn a_before_trigger_this_cannot_follow_is_refused() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE touched (id INT PRIMARY KEY, title VARCHAR(20), fine DATETIME(3), at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP)",
    );
    for sql in [
        // A `BEFORE` trigger writing another table, `SET NEW` in an `AFTER`
        // one (1362) and in a `DELETE` one (1363).
        "CREATE TRIGGER b BEFORE INSERT ON users FOR EACH ROW INSERT INTO audit (msg) VALUES (NEW.name)",
        "CREATE TRIGGER b AFTER INSERT ON users FOR EACH ROW SET NEW.name = 'x'",
        "CREATE TRIGGER b BEFORE DELETE ON users FOR EACH ROW SET NEW.name = 'x'",
        // The key, a column the trigger sets read before it is set, and a
        // clock into a word or a fraction of a second.
        "CREATE TRIGGER b BEFORE INSERT ON users FOR EACH ROW SET NEW.id = 1",
        "CREATE TRIGGER b BEFORE INSERT ON users FOR EACH ROW SET NEW.name = CONCAT(NEW.name, 'x')",
        "CREATE TRIGGER b BEFORE INSERT ON touched FOR EACH ROW SET NEW.title = NOW()",
        "CREATE TRIGGER b BEFORE INSERT ON touched FOR EACH ROW SET NEW.fine = NOW()",
        // A table whose `UPDATE` stamps a column of its own.
        "CREATE TRIGGER b BEFORE UPDATE ON touched FOR EACH ROW SET NEW.title = 'x'",
        "CREATE TRIGGER b BEFORE INSERT ON users FOR EACH ROW SET @x = 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert!(rows(&mut adapter, "SHOW TRIGGERS").is_empty());
}
