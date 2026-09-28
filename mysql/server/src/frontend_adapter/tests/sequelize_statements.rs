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

/// The table sequelize-cli keeps its migrations in, as it creates it before
/// every `db:migrate`.
const META_TABLE: &str = "CREATE TABLE IF NOT EXISTS `SequelizeMeta` (`name` VARCHAR(255) NOT NULL UNIQUE , PRIMARY KEY (`name`)) ENGINE=InnoDB DEFAULT CHARSET=utf8 COLLATE utf8_unicode_ci;";

/// `DEFAULT CHARSET=utf8 COLLATE utf8_unicode_ci` is MySQL's `utf8mb3` with
/// its Unicode 4.0.0 collation: every text column takes it, and every place
/// that names a column's character set or width says so.
#[test]
fn sequelizes_meta_table_holds_utf8mb3_words() {
    let (directory, mut adapter) = adapter();
    run(&mut adapter, &META_TABLE.replace("SequelizeMeta", "meta"));
    let written = "CREATE TABLE `meta` (\n  `name` varchar(255) COLLATE utf8mb3_unicode_ci NOT NULL,\n  PRIMARY KEY (`name`),\n  UNIQUE KEY `name` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb3 COLLATE=utf8mb3_unicode_ci";
    assert_eq!(created(&mut adapter, "meta"), written);
    for (sql, expected) in [
        (
            "SHOW FULL COLUMNS FROM meta",
            words(&[&[
                "name",
                "varchar(255)",
                "utf8mb3_unicode_ci",
                "NO",
                "PRI",
            ]]),
        ),
        (
            "SELECT COLUMN_NAME, CHARACTER_MAXIMUM_LENGTH, CHARACTER_OCTET_LENGTH, CHARACTER_SET_NAME, COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_NAME = 'meta'",
            words(&[&["name", "255", "765", "utf8mb3", "utf8mb3_unicode_ci"]]),
        ),
        (
            "SELECT TABLE_COLLATION FROM information_schema.TABLES WHERE TABLE_NAME = 'meta'",
            words(&[&["utf8mb3_unicode_ci"]]),
        ),
    ] {
        let mut read = rows(&mut adapter, sql);
        for row in &mut read {
            row.truncate(expected[0].len());
        }
        assert_eq!(read, expected, "{sql}");
    }
    // A column read back reports what a `utf8mb4` one of the same width does:
    // measured, a `VARCHAR(255)` is 1020 wide in either, the connection's
    // character set being what the width is counted in.
    let result = match adapter.execute_query(
        "SELECT `name` FROM `meta` AS `SequelizeMeta` ORDER BY `SequelizeMeta`.`name` ASC;",
    ) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("the read must answer rows, answered {other:?}"),
    };
    assert_eq!(result.columns[0].column_length, 1020);
    assert_eq!(result.columns[0].column_type, MYSQL_TYPE_VAR_STRING);

    let mut adapter = reopened(&directory, adapter);
    assert_eq!(created(&mut adapter, "meta"), written);
    run(
        &mut adapter,
        "INSERT INTO `meta` (`name`) VALUES ('20260101000000-create-blog.js')",
    );
    // An `ALTER TABLE` adding a column of words gives it the table's collation.
    run(&mut adapter, "ALTER TABLE meta ADD note VARCHAR(5)");
    assert!(created(&mut adapter, "meta")
        .contains("`note` varchar(5) COLLATE utf8mb3_unicode_ci DEFAULT NULL"));
}

/// `utf8mb3` holds no character past the Basic Multilingual Plane, and
/// compares the rest as `utf8mb4_unicode_ci` does.
#[test]
fn a_utf8mb3_column_takes_no_four_byte_character() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE u3 (id INT PRIMARY KEY, name VARCHAR(20), t TEXT) DEFAULT CHARSET=utf8 COLLATE utf8_unicode_ci",
    );
    // Measured: 1366, whichever column and whichever row.
    for sql in [
        "INSERT INTO u3 VALUES (1, 'a\u{1F600}', 'x')",
        "INSERT INTO u3 VALUES (1, 'ok', '\u{1F600}')",
        "INSERT INTO u3 VALUES (1, 'ok', 'fine'), (2, 'b\u{1F600}', 'y')",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::IncorrectValue)
            ),
            "{sql} must answer 1366"
        );
    }
    run(&mut adapter, "INSERT INTO u3 VALUES (1, 'ok', 'fine')");
    run(
        &mut adapter,
        "INSERT INTO u3 VALUES (2, '\u{c4}\u{d6}\u{fc}\u{20ac}', '\u{6f22}\u{5b57}')",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id FROM u3 WHERE name = '\u{e4}\u{f6}\u{dc}\u{20ac}'"
        ),
        words(&[&["2"]])
    );
    // It pads, as `utf8mb4_unicode_ci` does.
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM u3 WHERE name = 'OK  '"),
        words(&[&["1"]])
    );
    // Measured: such a character meeting the column is an illegal mix of
    // collations, 1267 or 1270, and a bound one 3988.
    for sql in [
        "UPDATE u3 SET name = CONCAT(name, '\u{1F600}') WHERE id = 1",
        "SELECT id, name FROM u3 WHERE name = 'ok\u{1F600}'",
        "SELECT id FROM u3 WHERE name IN ('a', 'b\u{1F600}')",
        "DELETE FROM u3 WHERE name = '\u{1F600}'",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql} must be refused"
        );
    }
    let statement = adapter
        .execute_stmt_prepare("SELECT id, name FROM u3 WHERE name = ?")
        .unwrap();
    let emoji = "ok\u{1F600}";
    let mut payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0, emoji.len() as u8];
    payload.extend_from_slice(emoji.as_bytes());
    assert!(matches!(
        adapter.execute_stmt_execute(statement.statement_id, &payload),
        Err(FrontendErrorKind::Unsupported)
    ));
    // A table of `utf8mb4` words takes the same character as it always has.
    run(
        &mut adapter,
        "CREATE TABLE u4 (id INT PRIMARY KEY, name VARCHAR(20))",
    );
    run(&mut adapter, "INSERT INTO u4 VALUES (1, '\u{1F600}')");
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM u4 WHERE name = '\u{1F600}'"),
        words(&[&["1"]])
    );
}

/// A column may name `utf8mb3_unicode_ci` in a `utf8mb4` table and the
/// other way round, which `SHOW CREATE TABLE` prints with the column's own
/// character set. What names a `utf8mb3` collation this server does not
/// have, or two that do not go together, is refused.
#[test]
fn a_column_names_its_own_character_set() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE m6 (a VARCHAR(10), d VARCHAR(10) COLLATE utf8mb3_unicode_ci, f VARCHAR(10) CHARACTER SET utf8mb3 COLLATE utf8mb3_unicode_ci)",
    );
    assert_eq!(
        created(&mut adapter, "m6"),
        "CREATE TABLE `m6` (\n  `a` varchar(10) DEFAULT NULL,\n  `d` varchar(10) CHARACTER SET utf8mb3 COLLATE utf8mb3_unicode_ci DEFAULT NULL,\n  `f` varchar(10) CHARACTER SET utf8mb3 COLLATE utf8mb3_unicode_ci DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    run(
        &mut adapter,
        "CREATE TABLE m10 (a VARCHAR(10), c VARCHAR(5) CHARACTER SET utf8mb4) DEFAULT CHARACTER SET utf8mb3 COLLATE utf8mb3_unicode_ci",
    );
    assert_eq!(
        created(&mut adapter, "m10"),
        "CREATE TABLE `m10` (\n  `a` varchar(10) COLLATE utf8mb3_unicode_ci DEFAULT NULL,\n  `c` varchar(5) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb3 COLLATE=utf8mb3_unicode_ci"
    );
    run(
        &mut adapter,
        "CREATE TABLE m9 (a VARCHAR(10)) COLLATE utf8mb3_unicode_ci",
    );
    assert_eq!(
        created(&mut adapter, "m9"),
        "CREATE TABLE `m9` (\n  `a` varchar(10) COLLATE utf8mb3_unicode_ci DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb3 COLLATE=utf8mb3_unicode_ci"
    );
    for sql in [
        // `utf8mb3_general_ci`, which this server does not have.
        "CREATE TABLE r1 (a VARCHAR(10)) CHARSET utf8",
        "CREATE TABLE r2 (a VARCHAR(10) CHARACTER SET utf8mb3)",
        // 1253 in MySQL.
        "CREATE TABLE r3 (a VARCHAR(10)) CHARSET=utf8mb4 COLLATE=utf8mb3_unicode_ci",
        "CREATE TABLE r4 (a VARCHAR(10) CHARACTER SET utf8mb4 COLLATE utf8mb3_unicode_ci)",
        // Not measured.
        "CREATE TABLE r5 (a VARCHAR(10), en ENUM('x','y')) CHARSET=utf8mb3 COLLATE=utf8mb3_unicode_ci",
        "CREATE DATABASE d3 COLLATE utf8_unicode_ci",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql} must be refused");
    }
}
