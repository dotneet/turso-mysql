//! Restoring what `mysqldump --databases` writes, the way the `mysql` client
//! sends it: statement by statement, with the delimiter taken off.
//!
//! `mysqldump_probe.sql` is a real dump, taken with `mysqldump
//! --single-transaction --databases probe` from MySQL 8.4.11, of a schema with
//! counted tables, a foreign key, `JSON`, `DECIMAL` defaults, a
//! `utf8mb4_unicode_ci` table beside `utf8mb4_0900_ai_ci` ones, a trigger
//! writing a `CONCAT` of its row into another table, and a view of one table
//! and a view joining two, both created by a latin1 client. Its objects were
//! made by an account `dump_owner`@`%`, which is the `DEFINER` the dump names.
//! Every expectation here was measured by restoring that same file there.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const DUMP: &str = include_str!("mysqldump_probe.sql");

const ACCOUNT: [u8; 32] = [0x62; 32];

/// A session of the account the dump's `DEFINER` names, with no database
/// selected: the dump makes and selects its own.
fn restoring_session() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("dump_owner"));
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes(ACCOUNT),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    (directory, adapter)
}

/// Splits a dump the way the `mysql` client does: comment lines are dropped,
/// `DELIMITER` changes what ends a statement, and a statement ends at the
/// line that ends with the delimiter.
fn statements_the_client_sends(dump: &str) -> Vec<String> {
    let mut delimiter = ";".to_owned();
    let mut statements = Vec::new();
    let mut pending = String::new();
    for line in dump.lines() {
        if pending.is_empty() {
            if line.trim().is_empty() || line.starts_with("--") {
                continue;
            }
            if let Some(new_delimiter) = line.strip_prefix("DELIMITER ") {
                delimiter = new_delimiter.trim().to_owned();
                continue;
            }
        } else {
            pending.push('\n');
        }
        pending.push_str(line);
        if let Some(statement) = pending.trim_end().strip_suffix(delimiter.as_str()) {
            statements.push(statement.trim_end().to_owned());
            pending.clear();
        }
    }
    assert!(pending.is_empty(), "the dump ends inside a statement");
    statements
}

fn restored() -> (tempfile::TempDir, Adapter) {
    let (directory, mut adapter) = restoring_session();
    let refused = statements_the_client_sends(DUMP)
        .into_iter()
        .filter_map(|sql| {
            adapter
                .execute_query(&sql)
                .err()
                .map(|error| format!("{error:?}: {sql}"))
        })
        .collect::<Vec<_>>();
    assert!(refused.is_empty(), "{}", refused.join("\n"));
    (directory, adapter)
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

fn row(values: &[Option<&str>]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| value.map(str::to_owned))
        .collect()
}

fn printed_table(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE `{table}`"))[0][1]
        .clone()
        .unwrap()
}

#[test]
fn a_standard_dump_restores_every_row_it_holds() {
    let (_directory, mut adapter) = restored();
    assert_eq!(
        rows(&mut adapter, "SELECT DATABASE()"),
        [row(&[Some("probe")])]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM users ORDER BY id"),
        [
            row(&[
                Some("1"),
                Some("Ann"),
                Some("ann@example.com"),
                Some("12.50"),
                Some(r#"{"lang": "en", "tags": [1, 2]}"#),
                Some("2026-09-27 14:05:35"),
            ]),
            row(&[
                Some("2"),
                Some("Bob"),
                Some("bob@example.com"),
                Some("0.00"),
                None,
                Some("2026-09-27 14:05:35"),
            ]),
            row(&[
                Some("3"),
                Some("Émile"),
                Some("emile@example.com"),
                Some("-3.25"),
                Some(r#"{"lang": "fr"}"#),
                Some("2026-09-27 14:05:35"),
            ]),
        ]
    );
    // The posts were dumped before the users they name, and restored while
    // foreign-key checks were off.
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM posts ORDER BY id"),
        [
            row(&[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("first\npost"),
                Some("1.500"),
            ]),
            row(&[Some("2"), Some("1"), Some("It's \"quoted\""), None, None]),
            row(&[
                Some("3"),
                Some("3"),
                Some("Café"),
                Some("naïve"),
                Some("-0.125"),
            ]),
        ]
    );
    // The dump makes the trigger after the rows, so restoring them wrote
    // nothing more into the audit table.
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM audit ORDER BY id"),
        [
            row(&[Some("1"), Some("post Hello")]),
            row(&[Some("2"), Some("post It's \"quoted\"")]),
            row(&[Some("3"), Some("post Café")]),
        ]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM user_names ORDER BY id"),
        [
            row(&[Some("1"), Some("Ann")]),
            row(&[Some("2"), Some("Bob")]),
            row(&[Some("3"), Some("Émile")]),
        ]
    );
    let mut joined = rows(&mut adapter, "SELECT * FROM user_posts");
    joined.sort();
    assert_eq!(
        joined,
        [
            row(&[Some("Ann"), Some("Hello")]),
            row(&[Some("Ann"), Some("It's \"quoted\"")]),
            row(&[Some("Émile"), Some("Café")]),
        ]
    );
}

#[test]
fn a_restored_schema_prints_and_behaves_as_it_was_dumped() {
    let (_directory, mut adapter) = restored();
    // Each table prints as the dump wrote it. MySQL's restored `users` prints
    // `CHARACTER SET utf8mb4` before each column's `COLLATE` as well, which
    // it does for a column whose collation was written on the column — as the
    // dump writes it — and which this server does not yet; see TODO.md.
    assert_eq!(
        printed_table(&mut adapter, "users"),
        concat!(
            "CREATE TABLE `users` (\n",
            "  `id` int NOT NULL AUTO_INCREMENT,\n",
            "  `name` varchar(100) COLLATE utf8mb4_unicode_ci NOT NULL,\n",
            "  `email` varchar(191) COLLATE utf8mb4_unicode_ci NOT NULL,\n",
            "  `balance` decimal(10,2) NOT NULL DEFAULT '0.00',\n",
            "  `profile` json DEFAULT NULL,\n",
            "  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,\n",
            "  PRIMARY KEY (`id`),\n",
            "  UNIQUE KEY `users_email_unique` (`email`)\n",
            ") ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"
        )
    );
    assert_eq!(
        printed_table(&mut adapter, "posts"),
        concat!(
            "CREATE TABLE `posts` (\n",
            "  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n",
            "  `user_id` int NOT NULL,\n",
            "  `title` varchar(200) NOT NULL,\n",
            "  `body` text,\n",
            "  `score` decimal(8,3) DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`),\n",
            "  KEY `posts_user_id_index` (`user_id`),\n",
            "  CONSTRAINT `posts_user_id_foreign` FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE\n",
            ") ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    assert_eq!(
        printed_table(&mut adapter, "audit"),
        concat!(
            "CREATE TABLE `audit` (\n",
            "  `id` int NOT NULL AUTO_INCREMENT,\n",
            "  `note` varchar(255) NOT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // The dump puts back every setting it changed, the connection's collation
    // among them: the one this session began with.
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT @@foreign_key_checks, @@unique_checks, @@sql_notes, @@time_zone, \
             @@character_set_client, @@character_set_results, @@collation_connection"
        ),
        [row(&[
            Some("1"),
            Some("1"),
            Some("1"),
            Some("SYSTEM"),
            Some("utf8mb4"),
            Some("utf8mb4"),
            Some("utf8mb4_general_ci"),
        ])]
    );

    // The counters go on from where the dump left them, a left-out DECIMAL
    // takes its default, and the foreign key holds again.
    run(
        &mut adapter,
        "INSERT INTO users (name, email) VALUES ('Cy', 'cy@example.com')",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, balance FROM users WHERE email = 'cy@example.com'"
        ),
        [row(&[Some("4"), Some("0.00")])]
    );
    assert_eq!(
        adapter.execute_query("UPDATE posts SET user_id = 99 WHERE id = 2"),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );
    // The view over a join prints the text the dump restored it from, as
    // MySQL's does.
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE VIEW user_posts")[0][1].as_deref(),
        Some(
            "CREATE ALGORITHM=UNDEFINED DEFINER=`dump_owner`@`%` SQL SECURITY DEFINER VIEW `user_posts` AS select `u`.`name` AS `name`,`p`.`title` AS `title` from (`users` `u` join `posts` `p` on((`p`.`user_id` = `u`.`id`)))"
        )
    );
    // The trigger's body reads back as the dump wrote it, as MySQL's does.
    let triggers = rows(&mut adapter, "SHOW TRIGGERS");
    assert_eq!(triggers.len(), 1);
    assert_eq!(
        triggers[0][..5],
        row(&[
            Some("posts_audit"),
            Some("INSERT"),
            Some("posts"),
            Some("INSERT INTO audit (note) VALUES (CONCAT('post ', NEW.title))"),
            Some("AFTER"),
        ])
    );
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TRIGGER posts_audit")[0][2].as_deref(),
        Some(
            "CREATE DEFINER=`dump_owner`@`%` TRIGGER `posts_audit` AFTER INSERT ON `posts` FOR EACH ROW INSERT INTO audit (note) VALUES (CONCAT('post ', NEW.title))"
        )
    );
    // MySQL writes a new post and its audit row, numbered 4 in both tables.
    let new_post = ok(
        &mut adapter,
        "INSERT INTO posts (user_id, title) VALUES (2, 'New')",
    );
    assert_eq!((new_post.affected_rows, new_post.last_insert_id), (1, 4));
    assert_eq!(
        rows(&mut adapter, "SELECT id, note FROM audit WHERE id > 3"),
        [row(&[Some("4"), Some("post New")])]
    );
}

fn ok(adapter: &mut Adapter, sql: &str) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result,
        other => panic!("{sql}: {other:?}"),
    }
}

fn warnings(adapter: &mut Adapter) -> Vec<Vec<Option<String>>> {
    rows(adapter, "SHOW WARNINGS")
}

/// Measured on MySQL 8.4.11. A character set and encryption a database here
/// cannot keep are refused.
#[test]
fn create_database_takes_the_options_every_database_here_has() {
    let (_directory, mut adapter) = restoring_session();
    for sql in [
        "CREATE DATABASE IF NOT EXISTS other",
        "CREATE DATABASE /*!32312 IF NOT EXISTS*/ `dumped` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci */ /*!80016 DEFAULT ENCRYPTION='N' */",
        "create database `laravel` default character set `utf8mb4` default collate `utf8mb4_0900_ai_ci`",
        "CREATE DATABASE o3 CHARSET = 'utf8mb4' COLLATE = 'utf8mb4_0900_ai_ci' ENCRYPTION 'N'",
        "CREATE DATABASE o4 COLLATE utf8mb4_0900_ai_ci CHARACTER SET utf8mb4",
    ] {
        assert_eq!(ok(&mut adapter, sql).warnings, 0, "{sql}");
    }
    let listed = rows(&mut adapter, "SHOW DATABASES");
    for name in ["other", "dumped", "laravel", "o3", "o4"] {
        assert!(listed.contains(&row(&[Some(name)])), "{name}: {listed:?}");
    }

    ok(&mut adapter, "USE dumped");
    assert_eq!(
        ok(&mut adapter, "CREATE DATABASE IF NOT EXISTS other").warnings,
        1
    );
    assert_eq!(
        warnings(&mut adapter),
        [row(&[
            Some("Note"),
            Some("1007"),
            Some("Can't create database 'other'; database exists"),
        ])]
    );
    assert_eq!(
        adapter.execute_query("CREATE DATABASE other"),
        Err(FrontendErrorKind::DuplicateDatabase)
    );
    ok(&mut adapter, "SET sql_notes = 0");
    assert_eq!(
        ok(&mut adapter, "CREATE DATABASE IF NOT EXISTS other").warnings,
        0
    );
    assert!(warnings(&mut adapter).is_empty());

    for sql in [
        "CREATE DATABASE l1 CHARACTER SET latin1",
        "CREATE DATABASE e1 ENCRYPTION 'Y'",
        "CREATE DATABASE /*!99999 IF NOT EXISTS*/ future",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    let listed = rows(&mut adapter, "SHOW DATABASES");
    for name in ["l1", "e1", "future"] {
        assert!(!listed.contains(&row(&[Some(name)])), "{name}: {listed:?}");
    }
}

/// Measured on MySQL 8.4.11: InnoDB keeps no switch for its keys, so
/// `DISABLE KEYS` and `ENABLE KEYS` answer OK with note 1031, and 1146 for a
/// table that is not there.
#[test]
fn disable_keys_and_enable_keys_leave_a_note() {
    let (_directory, mut adapter) = restoring_session();
    ok(&mut adapter, "CREATE DATABASE probe");
    ok(&mut adapter, "USE probe");
    ok(
        &mut adapter,
        "CREATE TABLE uq (id INT NOT NULL PRIMARY KEY, e INT, UNIQUE KEY (e))",
    );
    ok(&mut adapter, "CREATE VIEW uqv AS SELECT id FROM uq");
    for sql in [
        "/*!40000 ALTER TABLE `uq` DISABLE KEYS */",
        "ALTER TABLE uq ENABLE KEYS",
    ] {
        assert_eq!(ok(&mut adapter, sql).warnings, 1, "{sql}");
        assert_eq!(
            warnings(&mut adapter),
            [row(&[
                Some("Note"),
                Some("1031"),
                Some("Table storage engine for 'uq' doesn't have this option"),
            ])]
        );
    }
    assert_eq!(
        adapter.execute_query("ALTER TABLE missing DISABLE KEYS"),
        Err(FrontendErrorKind::MissingObject)
    );
    // MySQL answers 1347 over a view.
    assert!(adapter
        .execute_query("ALTER TABLE uqv DISABLE KEYS")
        .is_err());
    ok(&mut adapter, "SET sql_notes = 0");
    assert_eq!(ok(&mut adapter, "ALTER TABLE uq DISABLE KEYS").warnings, 0);
    assert!(warnings(&mut adapter).is_empty());
}

/// A dump turns the checks off before it makes and selects its database, so
/// the connection `USE` opens has to be given the session's settings.
#[test]
fn settings_made_before_use_reach_the_database_it_selects() {
    let (_directory, mut adapter) = restoring_session();
    ok(
        &mut adapter,
        "SET FOREIGN_KEY_CHECKS = 0, UNIQUE_CHECKS = 0",
    );
    ok(&mut adapter, "CREATE DATABASE probe");
    ok(&mut adapter, "USE probe");
    ok(
        &mut adapter,
        "CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, e INT, UNIQUE KEY (e))",
    );
    ok(
        &mut adapter,
        "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT NOT NULL, \
         FOREIGN KEY (parent_id) REFERENCES parent (id))",
    );
    ok(&mut adapter, "INSERT INTO child VALUES (1, 7)");
    // Measured on MySQL 8.4.11 with its default innodb_change_buffering=none:
    // a duplicate key is refused while unique_checks is off, as while it is on.
    ok(&mut adapter, "INSERT INTO parent VALUES (7, 1)");
    assert_eq!(
        adapter.execute_query("INSERT INTO parent VALUES (8, 1)"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    ok(&mut adapter, "SET FOREIGN_KEY_CHECKS = 1");
    assert_eq!(
        adapter.execute_query("INSERT INTO child VALUES (2, 9)"),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );
}

/// A view in a dump is written by the client it was created with, which is
/// latin1 wherever the client was left at its default. latin1 and utf8mb4
/// read ASCII alike, so ASCII is what is taken while latin1 is named.
#[test]
fn latin1_takes_ascii_statements_and_answers_no_result() {
    let (_directory, mut adapter) = restoring_session();
    ok(&mut adapter, "CREATE DATABASE probe");
    ok(&mut adapter, "USE probe");
    ok(
        &mut adapter,
        "CREATE TABLE names (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) NOT NULL)",
    );
    ok(&mut adapter, "INSERT INTO names VALUES (1, 'Émile')");
    ok(&mut adapter, "SET character_set_client = latin1");
    assert_eq!(
        adapter.execute_query("INSERT INTO names VALUES (2, 'Zoë')"),
        Err(FrontendErrorKind::Unsupported)
    );
    ok(&mut adapter, "INSERT INTO names VALUES (2, 'Zoe')");
    assert_eq!(
        adapter.execute_stmt_prepare("SELECT name FROM names WHERE id = ?"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT name FROM names ORDER BY id"),
        [row(&[Some("Émile")]), row(&[Some("Zoe")])]
    );
    ok(&mut adapter, "SET character_set_results = latin1");
    assert_eq!(
        adapter.execute_query("SELECT name FROM names ORDER BY id"),
        Err(FrontendErrorKind::Unsupported)
    );
    ok(
        &mut adapter,
        "CREATE VIEW first_names AS select `names`.`name` AS `name` from `names`",
    );
    ok(&mut adapter, "SET NAMES utf8mb4");
    ok(&mut adapter, "INSERT INTO names VALUES (3, 'Zoë')");
    assert_eq!(
        rows(&mut adapter, "SELECT name FROM first_names"),
        [
            row(&[Some("Émile")]),
            row(&[Some("Zoe")]),
            row(&[Some("Zoë")])
        ]
    );
}
