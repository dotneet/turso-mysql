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
