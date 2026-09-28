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

/// `sync({ force })` and `queryInterface.showIndex` list a table's indexes
/// naming the database after the table, which answers what naming it before
/// the table does; the one written after stands where both are written.
#[test]
fn sequelize_lists_indexes_naming_the_database_after_the_table() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `tags` (`id` BIGINT auto_increment , `name` VARCHAR(100) NOT NULL UNIQUE, PRIMARY KEY (`id`)) ENGINE=InnoDB;",
    );
    let listed = rows(&mut adapter, "SHOW INDEX FROM `tags`");
    assert_eq!(
        listed
            .iter()
            .map(|row| [row[0].clone(), row[2].clone(), row[4].clone()])
            .collect::<Vec<_>>(),
        [["tags", "PRIMARY", "id"], ["tags", "name", "name"]]
            .map(|row| row.map(|value| Some(value.to_owned())))
    );
    for sql in [
        "SHOW INDEX FROM `tags` FROM `reports`",
        "SHOW KEYS IN tags IN reports",
        "SHOW INDEXES FROM reports.tags FROM reports",
    ] {
        assert_eq!(rows(&mut adapter, sql), listed, "{sql}");
    }
    // Another database is refused, where MySQL reads that database's table.
    assert!(matches!(
        adapter.execute_query("SHOW INDEX FROM `tags` FROM `sequelize`"),
        Err(FrontendErrorKind::Unsupported)
    ));
}

/// One word bound as mysql2 binds a string: the null bitmap, the
/// new-parameters flag and one `VAR_STRING`.
fn one_word(word: &str) -> Vec<u8> {
    let mut payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0, word.len() as u8];
    payload.extend_from_slice(word.as_bytes());
    payload
}

/// A double and then a word, as mysql2 binds a number and a string where a
/// statement's parameters are not typed for it.
fn a_double_and_a_word(number: f64, word: &str) -> Vec<u8> {
    let mut payload = vec![0, 1, MYSQL_TYPE_DOUBLE, 0, MYSQL_TYPE_VAR_STRING, 0];
    payload.extend_from_slice(&number.to_le_bytes());
    payload.push(word.len() as u8);
    payload.extend_from_slice(word.as_bytes());
    payload
}

fn one_double(number: f64) -> Vec<u8> {
    let mut payload = vec![0, 1, MYSQL_TYPE_DOUBLE, 0];
    payload.extend_from_slice(&number.to_le_bytes());
    payload
}

fn prepared_rows(adapter: &mut Adapter, sql: &str, payload: &[u8]) -> Vec<Vec<BinaryResultValue>> {
    let statement = adapter.execute_stmt_prepare(sql).unwrap();
    let result = adapter.execute_stmt_execute(statement.statement_id, payload);
    adapter.execute_stmt_close(statement.statement_id);
    match result {
        Ok(PreparedStatementExecutionResult::ResultSet(result)) => result.rows,
        other => panic!("{sql} must answer rows, answered {other:?}"),
    }
}

/// `sequelize.query(sql, { bind })` sends its values through a prepared
/// statement, and mysql2 binds a number as a double where the statement does
/// not say the parameter is a whole number. Measured on MySQL 8.4.11, a bound
/// double meets a column of whole numbers exactly, as the engine compares a
/// whole number with a real.
#[test]
fn sequelizes_bound_number_meets_a_whole_number_column_as_a_double() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE posts (id INT PRIMARY KEY, views INT NOT NULL, big BIGINT, title VARCHAR(20))",
    );
    run(
        &mut adapter,
        "INSERT INTO posts VALUES (1, 1, 9007199254740993, 'a'), (2, 2, 9007199254740992, 'b'), (3, 12, NULL, 'x')",
    );
    assert_eq!(
        prepared_rows(
            &mut adapter,
            "SELECT COUNT(*) AS n FROM posts WHERE views >= ? AND title <> ?",
            &a_double_and_a_word(1.0, "x"),
        ),
        [[BinaryResultValue::Integer(2)]]
    );
    for (sql, number, ids) in [
        ("SELECT id FROM posts WHERE views = ?", 1.5, vec![]),
        ("SELECT id FROM posts WHERE views = ?", 2.0, vec![2]),
        (
            "SELECT id FROM posts WHERE views >= ? ORDER BY id",
            1.5,
            vec![2, 3],
        ),
        (
            "SELECT id FROM posts WHERE big = ?",
            9_007_199_254_740_992.0,
            vec![2],
        ),
    ] {
        assert_eq!(
            prepared_rows(&mut adapter, sql, &one_double(number)),
            ids.into_iter()
                .map(|id| vec![BinaryResultValue::Integer(id)])
                .collect::<Vec<_>>(),
            "{sql} bound {number}"
        );
    }
}

/// A transaction started inside another is a savepoint Sequelize names after
/// the outer transaction's id, dashes and all.
#[test]
fn sequelizes_nested_transaction_is_a_savepoint_named_with_dashes() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `tags` (`id` BIGINT auto_increment , `name` VARCHAR(100) NOT NULL UNIQUE, PRIMARY KEY (`id`)) ENGINE=InnoDB;",
    );
    let insert = adapter
        .execute_stmt_prepare("INSERT INTO `tags` (`id`,`name`) VALUES (DEFAULT,?);")
        .unwrap()
        .statement_id;
    run(&mut adapter, "START TRANSACTION;");
    adapter
        .execute_stmt_execute(insert, &one_word("kept"))
        .unwrap();
    run(
        &mut adapter,
        "SAVEPOINT `ae10d62c-ada2-47cf-99ae-f2d02c8ea9a1-sp-1`;",
    );
    adapter
        .execute_stmt_execute(insert, &one_word("dropped"))
        .unwrap();
    assert!(matches!(
        adapter.execute_stmt_execute(insert, &one_word("kept")),
        Err(FrontendErrorKind::ConstraintViolation)
    ));
    // Measured: the name matches whatever the case of its letters.
    run(
        &mut adapter,
        "ROLLBACK TO SAVEPOINT `AE10D62C-ADA2-47CF-99AE-F2D02C8EA9A1-SP-1`;",
    );
    adapter
        .execute_stmt_execute(insert, &one_word("after"))
        .unwrap();
    run(&mut adapter, "COMMIT;");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `id`, `name` FROM `tags` AS `Tag` WHERE `Tag`.`name` IN ('kept', 'dropped', 'after') ORDER BY `Tag`.`name` ASC;"
        )
        .into_iter()
        .map(|row| row[1].clone().unwrap())
        .collect::<Vec<_>>(),
        ["after", "kept"]
    );

    run(&mut adapter, "START TRANSACTION");
    run(&mut adapter, "SAVEPOINT `a b.c!`");
    run(&mut adapter, "RELEASE SAVEPOINT `A B.C!`");
    // Released, so there is nothing to roll back to: 1305.
    assert!(matches!(
        adapter.execute_query("ROLLBACK TO SAVEPOINT `a b.c!`"),
        Err(FrontendErrorKind::NoSuchSavepoint)
    ));
    run(&mut adapter, "SAVEPOINT `x``y`");
    run(&mut adapter, "ROLLBACK TO `X``Y`");
    run(&mut adapter, "COMMIT");
}

/// The engine names the columns of the index behind a key as it folds them,
/// `id` for a column written `Id`; that index is still the key and no other.
#[test]
fn a_key_column_written_in_capitals_has_one_index() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE CamelCase (Id INT PRIMARY KEY, Name VARCHAR(10))",
    );
    assert_eq!(
        created(&mut adapter, "camelcase"),
        "CREATE TABLE `camelcase` (\n  `Id` int NOT NULL,\n  `Name` varchar(10) DEFAULT NULL,\n  PRIMARY KEY (`Id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(rows(&mut adapter, "SHOW INDEX FROM CamelCase").len(), 1);
}

/// The app's blog tables as sequelize-cli's migration makes them, three users,
/// three posts and three tags, the first post carrying two tags and the third
/// one, and the second none.
fn blog() -> (tempfile::TempDir, Adapter) {
    let (directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE IF NOT EXISTS `users` (`id` BIGINT auto_increment , `email` VARCHAR(191) NOT NULL UNIQUE, `name` VARCHAR(100) NOT NULL, `balance` DECIMAL(10,2) NOT NULL DEFAULT 0, `is_active` TINYINT(1) NOT NULL DEFAULT true, `profile` JSON, `version` INTEGER NOT NULL DEFAULT 0, `created_at` DATETIME NOT NULL, `updated_at` DATETIME NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB;",
        "CREATE TABLE IF NOT EXISTS `posts` (`id` BIGINT auto_increment , `user_id` BIGINT NOT NULL, `title` VARCHAR(200) NOT NULL, `body` TEXT, `published_at` DATETIME, `views` INTEGER NOT NULL DEFAULT 0, `created_at` DATETIME NOT NULL, `updated_at` DATETIME NOT NULL, PRIMARY KEY (`id`), FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE ON UPDATE CASCADE) ENGINE=InnoDB;",
        "CREATE TABLE IF NOT EXISTS `tags` (`id` BIGINT auto_increment , `name` VARCHAR(100) NOT NULL UNIQUE, PRIMARY KEY (`id`)) ENGINE=InnoDB;",
        "CREATE TABLE IF NOT EXISTS `post_tags` (`post_id` BIGINT NOT NULL , `tag_id` BIGINT NOT NULL , PRIMARY KEY (`post_id`, `tag_id`), FOREIGN KEY (`post_id`) REFERENCES `posts` (`id`) ON DELETE CASCADE, FOREIGN KEY (`tag_id`) REFERENCES `tags` (`id`) ON DELETE CASCADE) ENGINE=InnoDB;",
        "INSERT INTO users (id, email, name, balance, is_active, created_at, updated_at) VALUES (1, 'alice@example.com', 'Alice', 10, 1, '2026-01-01 00:00:00', '2026-01-01 00:00:00'), (2, 'bob@example.com', 'Bob', 5, 1, '2026-01-01 00:00:00', '2026-01-01 00:00:00'), (3, 'carol@example.com', 'Carol', 0, 0, '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
        "INSERT INTO posts (id, user_id, title, body, views, created_at, updated_at) VALUES (1, 1, 'a', 'x', 3, '2026-01-01 00:00:00', '2026-01-01 00:00:00'), (2, 1, 'b', NULL, 5, '2026-01-01 00:00:00', '2026-01-01 00:00:00'), (3, 2, 'c', 'y', 7, '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
        "INSERT INTO tags (id, name) VALUES (1, 'news'), (2, 'rust'), (3, 'sql')",
        "INSERT INTO post_tags VALUES (1, 1), (1, 2), (3, 3)",
    ] {
        run(&mut adapter, sql);
    }
    (directory, adapter)
}

/// A result column's name, the table it is read through, and its flags.
type Shape = (String, String, u16);

/// Runs a statement in both protocols, holds the two to the same columns and
/// rows, and answers the text protocol's.
fn report(adapter: &mut Adapter, sql: &str) -> (Vec<Shape>, Vec<Vec<Option<String>>>) {
    let text = match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(text)) => text,
        other => panic!("{sql} must answer rows, answered {other:?}"),
    };
    let prepared = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("{sql} must prepare: {error:?}"));
    assert_eq!(prepared.columns, text.columns, "{sql}");
    let binary = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    adapter.execute_stmt_close(prepared.statement_id);
    assert_eq!(binary.columns, text.columns, "{sql}");
    assert_eq!(binary.rows.len(), text.rows.len(), "{sql}");
    let shapes = text
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.table.clone(), column.flags))
        .collect();
    let rows = text
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect();
    (shapes, rows)
}

fn values(rows: &[&[Option<&str>]]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|row| row.iter().map(|value| value.map(str::to_owned)).collect())
        .collect()
}

/// An include through a join table — `User.findAll({ include: [{ model:
/// Post, include: [Tag] }] })` — reads the join table and the tags it names
/// joined in parentheses, `LEFT OUTER JOIN (post_tags INNER JOIN tags ON ...)
/// ON ...`. Measured on MySQL 8.4.11: a post with no tag, or whose one row in
/// the join table names a tag that is not there, is read once with every
/// column of both tables missing, and those columns keep their keys but not
/// `NOT NULL`.
#[test]
fn sequelizes_include_through_a_join_table_joins_two_tables_in_parentheses() {
    let (_directory, mut adapter) = blog();
    let (shapes, rows) = report(
        &mut adapter,
        "SELECT `User`.`id`, `posts`.`id` AS `posts.id`, `posts->tags`.`id` AS `posts.tags.id`, `posts->tags`.`name` AS `posts.tags.name`, `posts->tags->PostTag`.`post_id` AS `posts.tags.PostTag.postId`, `posts->tags->PostTag`.`tag_id` AS `posts.tags.PostTag.tagId` FROM `users` AS `User` LEFT OUTER JOIN `posts` AS `posts` ON `User`.`id` = `posts`.`user_id` LEFT OUTER JOIN ( `post_tags` AS `posts->tags->PostTag` INNER JOIN `tags` AS `posts->tags` ON `posts->tags`.`id` = `posts->tags->PostTag`.`tag_id`) ON `posts`.`id` = `posts->tags->PostTag`.`post_id` ORDER BY `User`.`id` ASC, `posts`.`id` ASC;",
    );
    const PART_KEY: u16 = 16384;
    const NO_DEFAULT: u16 = 4096;
    const KEY: u16 = 2 | 512 | PART_KEY;
    assert_eq!(
        shapes,
        [
            ("id", "User", 1 | KEY),
            ("posts.id", "posts", KEY),
            ("posts.tags.id", "posts->tags", KEY),
            ("posts.tags.name", "posts->tags", 4 | NO_DEFAULT | PART_KEY),
            (
                "posts.tags.PostTag.postId",
                "posts->tags->PostTag",
                2 | NO_DEFAULT | PART_KEY
            ),
            (
                "posts.tags.PostTag.tagId",
                "posts->tags->PostTag",
                2 | NO_DEFAULT | PART_KEY
            ),
        ]
        .map(|(name, table, flags)| (name.to_owned(), table.to_owned(), flags))
    );
    let mut sorted = rows.clone();
    sorted.sort_by_key(|row| (row[0].clone(), row[1].clone(), row[2].clone()));
    assert_eq!(rows, sorted, "ordered by user and post");
    assert_eq!(
        sorted,
        values(&[
            &[
                Some("1"),
                Some("1"),
                Some("1"),
                Some("news"),
                Some("1"),
                Some("1")
            ],
            &[
                Some("1"),
                Some("1"),
                Some("2"),
                Some("rust"),
                Some("1"),
                Some("2")
            ],
            &[Some("1"), Some("2"), None, None, None, None],
            &[
                Some("2"),
                Some("3"),
                Some("3"),
                Some("sql"),
                Some("3"),
                Some("3")
            ],
            &[Some("3"), None, None, None, None, None],
        ])
    );

    // A row of the join table naming no tag keeps its post out of the
    // parentheses, and a condition on the tag moves with the tag.
    run(&mut adapter, "SET foreign_key_checks = 0");
    run(&mut adapter, "INSERT INTO post_tags VALUES (2, 99)");
    let tagged = "SELECT `Post`.`id`, `tags`.`name` AS `tags.name`, `tags->PostTag`.`tag_id` AS `tags.PostTag.tagId` FROM `posts` AS `Post` LEFT OUTER JOIN ( `post_tags` AS `tags->PostTag` INNER JOIN `tags` AS `tags` ON `tags`.`id` = `tags->PostTag`.`tag_id`) ON `Post`.`id` = `tags->PostTag`.`post_id` ORDER BY `Post`.`id`, `tags`.`id`";
    assert_eq!(
        report(&mut adapter, tagged).1,
        values(&[
            &[Some("1"), Some("news"), Some("1")],
            &[Some("1"), Some("rust"), Some("2")],
            &[Some("2"), None, None],
            &[Some("3"), Some("sql"), Some("3")],
        ])
    );
    assert_eq!(
        report(
            &mut adapter,
            &tagged.replace(
                "`tags->PostTag`.`post_id` ORDER",
                "`tags->PostTag`.`post_id` AND `tags`.`name` <> 'news' ORDER"
            )
        )
        .1,
        values(&[
            &[Some("1"), Some("rust"), Some("2")],
            &[Some("2"), None, None],
            &[Some("3"), Some("sql"), Some("3")],
        ])
    );
    assert_eq!(
        report(
            &mut adapter,
            "SELECT count(DISTINCT(`Post`.`id`)) AS `count` FROM `posts` AS `Post` LEFT OUTER JOIN ( `post_tags` AS `tags->PostTag` INNER JOIN `tags` AS `tags` ON `tags`.`id` = `tags->PostTag`.`tag_id`) ON `Post`.`id` = `tags->PostTag`.`post_id` LEFT OUTER JOIN `users` AS `user` ON `Post`.`user_id` = `user`.`id`;"
        )
        .1,
        values(&[&[Some("3")]])
    );
}

/// `required: true` on both includes makes every join an inner one, and the
/// condition on the tag stands in the `ON` of the parentheses.
#[test]
fn sequelizes_required_include_through_a_join_table_joins_inside_parentheses() {
    let (_directory, mut adapter) = blog();
    assert_eq!(
        report(
            &mut adapter,
            "SELECT `Post`.`id`, `user`.`email` AS `user.email`, `tags`.`name` AS `tags.name`, `tags->PostTag`.`post_id` AS `tags.PostTag.postId` FROM `posts` AS `Post` INNER JOIN `users` AS `user` ON `Post`.`user_id` = `user`.`id` AND `user`.`is_active` = true INNER JOIN ( `post_tags` AS `tags->PostTag` INNER JOIN `tags` AS `tags` ON `tags`.`id` = `tags->PostTag`.`tag_id`) ON `Post`.`id` = `tags->PostTag`.`post_id` AND `tags`.`name` = 'rust';"
        )
        .1,
        values(&[&[Some("1"), Some("alice@example.com"), Some("rust"), Some("1")]])
    );
}

/// What the engine could not answer the same rows for stays refused: a left
/// join inside the parentheses, and an inner `ON` that does not match a
/// column of one table against a column of the other, which would let the
/// second table's rows through beside a first table left missing.
#[test]
fn a_join_in_parentheses_the_engine_cannot_follow_is_refused() {
    let (_directory, mut adapter) = blog();
    for sql in [
        "SELECT p.id FROM posts p LEFT JOIN (post_tags pt LEFT JOIN tags t ON t.id = pt.tag_id) ON p.id = pt.post_id",
        "SELECT p.id FROM posts p LEFT JOIN (post_tags pt JOIN tags t ON t.name = 'news') ON p.id = pt.post_id",
        "SELECT p.id FROM posts p LEFT JOIN (post_tags pt JOIN tags t ON t.id = pt.tag_id JOIN users u ON u.id = p.user_id) ON p.id = pt.post_id",
        "SELECT p.id FROM posts p RIGHT JOIN (post_tags pt JOIN tags t ON t.id = pt.tag_id) ON p.id = pt.post_id",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}

/// `sequelize.query` with replacements sends
/// `UPDATE posts SET body = CONCAT(COALESCE(body, ''), '!') WHERE user_id = 1`
/// to append to a column that may be NULL. Measured on MySQL 8.4.11: the
/// fallback stands for the NULL, so `x` becomes `x!` and NULL `!`, and both
/// rows count as changed.
#[test]
fn sequelizes_raw_update_appends_to_a_column_that_may_be_null() {
    let (_directory, mut adapter) = blog();
    run(
        &mut adapter,
        "UPDATE posts SET title = CONCAT(IFNULL(body, 'none'), '-', `posts`.`title`) WHERE id IN (2, 3)",
    );
    let Ok(CommandExecutionResult::Ok(result)) = adapter
        .execute_query("UPDATE posts SET body = CONCAT(COALESCE(body, ''), '!') WHERE user_id = 1")
    else {
        panic!("the append must be taken");
    };
    assert_eq!(result.affected_rows, 2);
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, title, body FROM posts ORDER BY id"
        ),
        values(&[
            &[Some("1"), Some("a"), Some("x!")],
            &[Some("2"), Some("none-b"), Some("!")],
            &[Some("3"), Some("y-c"), Some("y")],
        ])
    );
    // A number or a moment is spelled by rules of its own, and a column of
    // numbers reads the words it is given as a number.
    for sql in [
        "UPDATE posts SET title = CONCAT(COALESCE(views, ''), '!')",
        "UPDATE posts SET title = CONCAT(COALESCE(published_at, ''), '!')",
        "UPDATE posts SET title = CONCAT(COALESCE(body, ''), views)",
        "UPDATE posts SET views = CONCAT(COALESCE(body, ''), '1')",
        "UPDATE posts SET body = 'z', title = CONCAT(COALESCE(body, ''), '!')",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}

/// `Post.max('views', { include: [{ model: User, where: { email } }] })`
/// projects the user's columns beside `max(views)` with no `GROUP BY`.
/// Measured on MySQL 8.4.11: under `ONLY_FULL_GROUP_BY` that is taken when a
/// comparison with a written value in the `WHERE` or an inner join's `ON`
/// decides each column, itself or through a key, and each such column keeps
/// its keys but not `NOT_NULL`, the one answer being NULL for all of them when
/// no row matches.
#[test]
fn sequelizes_max_over_an_include_projects_the_columns_its_condition_decides() {
    let (_directory, mut adapter) = blog();
    let (shapes, rows) = report(
        &mut adapter,
        "SELECT max(`views`) AS `max`, `user`.`id` AS `user.id`, `user`.`email` AS `user.email`, `user`.`name` AS `user.name`, `user`.`balance` AS `user.balance`, `user`.`profile` AS `user.profile`, `user`.`created_at` AS `user.createdAt`, `user`.`version` AS `user.version` FROM `posts` AS `Post` INNER JOIN `users` AS `user` ON `Post`.`user_id` = `user`.`id` AND `user`.`email` = 'alice@example.com';",
    );
    const PART_KEY: u16 = 16384;
    const NO_DEFAULT: u16 = 4096;
    const BINARY: u16 = 128;
    assert_eq!(
        shapes,
        [
            ("max", "", 32768 | BINARY),
            ("user.id", "user", 2 | 512 | PART_KEY),
            ("user.email", "user", 4 | NO_DEFAULT | PART_KEY),
            ("user.name", "user", NO_DEFAULT),
            ("user.balance", "user", 0),
            ("user.profile", "user", 16 | BINARY),
            ("user.createdAt", "user", BINARY | NO_DEFAULT),
            ("user.version", "user", 0),
        ]
        .map(|(name, table, flags)| (name.to_owned(), table.to_owned(), flags))
    );
    assert_eq!(
        rows,
        values(&[&[
            Some("5"),
            Some("1"),
            Some("alice@example.com"),
            Some("Alice"),
            Some("10.00"),
            None,
            Some("2026-01-01 00:00:00"),
            Some("0"),
        ]])
    );
    for (sql, answer) in [
        (
            "SELECT MAX(p.views) AS m, u.name FROM posts p LEFT JOIN users u ON u.id = p.user_id WHERE p.id = 1",
            [Some("3"), Some("Alice")],
        ),
        (
            "SELECT COUNT(*) AS c, u.name FROM users u WHERE u.email = 'nobody'",
            [Some("0"), None],
        ),
        (
            "SELECT MAX(views) AS m, title FROM posts WHERE 1 = id",
            [Some("3"), Some("a")],
        ),
        (
            "SELECT MAX(p.views) AS m, u.name FROM posts p JOIN users u ON u.id = p.user_id WHERE u.name = 'Bob'",
            [Some("7"), Some("Bob")],
        ),
        (
            "SELECT MAX(p.views) AS m, u.name FROM posts p JOIN users u ON u.id = p.user_id WHERE u.name = 'Nobody'",
            [None, None],
        ),
    ] {
        assert_eq!(report(&mut adapter, sql).1, values(&[&answer]), "{sql}");
    }
    // Each of these is 1140 in MySQL: nothing decides the column.
    for sql in [
        "SELECT MAX(p.views) AS m, p.title FROM posts p JOIN users u ON u.id = p.user_id WHERE u.id = 1",
        "SELECT MAX(p.views) AS m, u.name FROM posts p LEFT JOIN users u ON u.id = p.user_id AND u.email = 'alice@example.com'",
        "SELECT MAX(views) AS m, title FROM posts WHERE id = 1 OR id = 2",
        "SELECT MAX(views) AS m, title FROM posts",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}

/// A post as mysql2 binds Sequelize's `Post.create`: the title and the two
/// moments as words, the views and the user as doubles.
fn a_new_post(title: &str, user: f64) -> Vec<u8> {
    let mut payload = vec![0, 1];
    for code in [
        MYSQL_TYPE_VAR_STRING,
        MYSQL_TYPE_DOUBLE,
        MYSQL_TYPE_VAR_STRING,
        MYSQL_TYPE_VAR_STRING,
        MYSQL_TYPE_DOUBLE,
    ] {
        payload.extend([code, 0]);
    }
    payload.push(title.len() as u8);
    payload.extend_from_slice(title.as_bytes());
    payload.extend_from_slice(&0f64.to_le_bytes());
    for moment in ["2026-09-28 11:24:57.348", "2026-09-28 11:24:57.348"] {
        payload.push(moment.len() as u8);
        payload.extend_from_slice(moment.as_bytes());
    }
    payload.extend_from_slice(&user.to_le_bytes());
    payload
}

fn written(result: Result<PreparedStatementExecutionResult, FrontendErrorKind>) -> (u64, u64) {
    match result {
        Ok(PreparedStatementExecutionResult::Ok(result)) => {
            (result.affected_rows, result.last_insert_id)
        }
        other => panic!("the write must be taken, answered {other:?}"),
    }
}

/// mysql2 keeps each statement it prepared for as long as its connection
/// lives, and `sequelize.sync({ force: true })` drops every table and makes
/// it again, in another column order, under it. Measured on MySQL 8.4.11: a
/// write prepared before is prepared again over the new table and runs,
/// counting ids from 1 again, and once the table is gone it answers 1146.
#[test]
fn sequelizes_writes_prepared_before_sync_force_run_over_the_new_tables() {
    let (_directory, mut adapter) = blog();
    let insert = adapter
        .execute_stmt_prepare("INSERT INTO `posts` (`id`,`title`,`views`,`created_at`,`updated_at`,`user_id`) VALUES (DEFAULT,?,?,?,?,?);")
        .unwrap()
        .statement_id;
    let update = adapter
        .execute_stmt_prepare("UPDATE `posts` SET `views`=`views` + 2 WHERE `user_id` = ?")
        .unwrap()
        .statement_id;
    assert_eq!(
        written(adapter.execute_stmt_execute(insert, &a_new_post("x", 1.0))),
        (1, 4)
    );
    for sql in [
        "DROP TABLE IF EXISTS `post_tags`;",
        "DROP TABLE IF EXISTS `posts`;",
        "DROP TABLE IF EXISTS `tags`;",
        "DROP TABLE IF EXISTS `users`;",
        "CREATE TABLE IF NOT EXISTS `users` (`id` BIGINT auto_increment , `email` VARCHAR(191) NOT NULL UNIQUE, `name` VARCHAR(100) NOT NULL, `balance` DECIMAL(10,2) NOT NULL DEFAULT 0, `is_active` TINYINT(1) NOT NULL DEFAULT true, `profile` JSON, `created_at` DATETIME NOT NULL, `updated_at` DATETIME NOT NULL, `version` INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (`id`)) ENGINE=InnoDB;",
        "CREATE TABLE IF NOT EXISTS `posts` (`id` BIGINT auto_increment , `title` VARCHAR(200) NOT NULL, `body` TEXT, `published_at` DATETIME, `views` INTEGER NOT NULL DEFAULT 0, `created_at` DATETIME NOT NULL, `updated_at` DATETIME NOT NULL, `user_id` BIGINT NOT NULL, PRIMARY KEY (`id`), FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE ON UPDATE CASCADE) ENGINE=InnoDB;",
        "ALTER TABLE `posts` ADD INDEX `posts_user_published` (`user_id`, `published_at`)",
        "INSERT INTO users (id, email, name, created_at, updated_at) VALUES (1, 'x@example.com', 'X', '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        written(adapter.execute_stmt_execute(insert, &a_new_post("y", 1.0))),
        (1, 1)
    );
    assert_eq!(
        written(adapter.execute_stmt_execute(update, &one_double(1.0))),
        (1, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, title, views, user_id FROM posts"),
        words(&[&["1", "y", "2", "1"]])
    );
    run(&mut adapter, "DROP TABLE `posts`");
    for (statement, payload) in [(insert, a_new_post("z", 1.0)), (update, one_double(1.0))] {
        assert!(matches!(
            adapter.execute_stmt_execute(statement, &payload),
            Err(FrontendErrorKind::MissingObject)
        ));
    }
}
