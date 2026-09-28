//! Statements Sequelize 6.37 and sequelize-cli 6.6 send over mysql2,
//! replayed as the framework harness's sixth run captured them.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const ACCOUNT: [u8; 32] = [193; 32];

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    (directory, connected(factory))
}

/// A new session over the same files, the catalog opened again from disk
/// once the session before it has let go of it.
fn reopened(directory: &tempfile::TempDir, adapter: Adapter) -> Adapter {
    drop(adapter);
    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    let factory = AuthorizedDatabaseAdapterFactory::new(
        catalog,
        binary_context(),
        Arc::new(RecordingAuthorizer::default()),
    );
    connected(factory)
}

fn connected(factory: AuthorizedDatabaseAdapterFactory<RecordingAuthorizer>) -> Adapter {
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes(ACCOUNT),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
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
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn words(rows: &[&[&str]]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|row| row.iter().map(|value| Some((*value).to_owned())).collect())
        .collect()
}

fn created(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE `{table}`"))[0][1]
        .clone()
        .unwrap()
}

/// A column that is both `UNIQUE` and the primary key has two indexes in
/// MySQL, `PRIMARY` and a unique one named after the column, however the
/// statement spells the two.
#[test]
fn a_unique_primary_key_column_keeps_both_its_indexes() {
    let (directory, mut adapter) = adapter();
    let two_keys = "CREATE TABLE `a` (\n  `name` varchar(255) NOT NULL,\n  PRIMARY KEY (`name`),\n  UNIQUE KEY `name` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";
    for sql in [
        "CREATE TABLE a (`name` VARCHAR(255) NOT NULL UNIQUE , PRIMARY KEY (`name`))",
        "CREATE TABLE a (`name` VARCHAR(255) NOT NULL UNIQUE PRIMARY KEY)",
        "CREATE TABLE a (`name` VARCHAR(255) NOT NULL, UNIQUE KEY (`name`), PRIMARY KEY (`name`))",
        "CREATE TABLE a (`name` VARCHAR(255) UNIQUE , PRIMARY KEY (`name`))",
    ] {
        run(&mut adapter, sql);
        assert_eq!(created(&mut adapter, "a"), two_keys, "{sql}");
        run(&mut adapter, "DROP TABLE a");
    }

    run(
        &mut adapter,
        "CREATE TABLE a (`name` VARCHAR(255) NOT NULL UNIQUE , PRIMARY KEY (`name`))",
    );
    assert_eq!(
        rows(&mut adapter, "SHOW INDEX FROM a")
            .into_iter()
            .map(|row| [row[2].clone(), row[1].clone(), row[4].clone()])
            .collect::<Vec<_>>(),
        [["PRIMARY", "0", "name"], ["name", "0", "name"]]
            .map(|row| row.map(|value| Some(value.to_owned())))
    );
    for (sql, expected) in [
        (
            "SELECT CONSTRAINT_NAME, CONSTRAINT_TYPE FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_NAME='a' ORDER BY CONSTRAINT_NAME",
            words(&[&["name", "UNIQUE"], &["PRIMARY", "PRIMARY KEY"]]),
        ),
        (
            "SELECT INDEX_NAME, COLUMN_NAME, NON_UNIQUE FROM information_schema.STATISTICS WHERE TABLE_NAME='a' ORDER BY INDEX_NAME",
            words(&[&["name", "name", "0"], &["PRIMARY", "name", "0"]]),
        ),
        (
            "SELECT CONSTRAINT_NAME, COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_NAME='a' ORDER BY CONSTRAINT_NAME",
            words(&[&["name", "name"], &["PRIMARY", "name"]]),
        ),
    ] {
        assert_eq!(rows(&mut adapter, sql), expected, "{sql}");
    }
    run(&mut adapter, "INSERT INTO a VALUES ('x')");
    assert!(matches!(
        adapter.execute_query("INSERT INTO a VALUES ('x')"),
        Err(FrontendErrorKind::ConstraintViolation)
    ));

    let mut adapter = reopened(&directory, adapter);
    assert_eq!(created(&mut adapter, "a"), two_keys);
    // The unique key goes and the primary key stays.
    run(&mut adapter, "ALTER TABLE a DROP INDEX `name`");
    assert_eq!(
        created(&mut adapter, "a"),
        "CREATE TABLE `a` (\n  `name` varchar(255) NOT NULL,\n  PRIMARY KEY (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
}

/// A second unique key over the same column takes the next free name, and a
/// counted key keeps its `UNIQUE` the same way.
#[test]
fn a_unique_primary_key_column_names_its_keys_as_mysql_does() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE b (`name` VARCHAR(255) NOT NULL UNIQUE, UNIQUE KEY (`name`), PRIMARY KEY (`name`))",
    );
    assert_eq!(
        created(&mut adapter, "b"),
        "CREATE TABLE `b` (\n  `name` varchar(255) NOT NULL,\n  PRIMARY KEY (`name`),\n  UNIQUE KEY `name` (`name`),\n  UNIQUE KEY `name_2` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    run(
        &mut adapter,
        "CREATE TABLE c (id INT AUTO_INCREMENT UNIQUE PRIMARY KEY)",
    );
    assert_eq!(
        created(&mut adapter, "c"),
        "CREATE TABLE `c` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `id` (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    run(&mut adapter, "INSERT INTO c () VALUES ()");
    assert_eq!(rows(&mut adapter, "SELECT id FROM c"), words(&[&["1"]]));
}
