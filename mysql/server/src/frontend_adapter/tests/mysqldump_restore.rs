//! Restoring what `mysqldump --databases` writes, the way the `mysql` client
//! sends it: statement by statement, with the delimiter taken off; and
//! answering what `mysqldump` sends while it dumps the restored schema again.
//!
//! `mysqldump_probe.sql` is a real dump, taken with `mysqldump
//! --single-transaction --routines --triggers --events --databases probe` from
//! MySQL 8.4.11, of a schema with counted tables, a foreign key, `JSON`,
//! `DECIMAL` defaults, a `utf8mb4_unicode_ci` table beside `utf8mb4_0900_ai_ci`
//! ones, a trigger writing a `CONCAT` of its row into another table, a view of
//! one table and a view joining two, both created by a latin1 client, and an
//! `articles` table shaped like the framework apps' posts — `ENUM`,
//! `DATETIME(6)` — holding text with every escape `mysqldump` writes: `\'`,
//! `\"`, `\\`, `\n`, `\r`, `\0` and `\Z`, a raw tab, emoji, and the empty
//! word beside NULL. Its objects were made by an account `dump_owner`@`%`,
//! which is the `DEFINER` the dump names. Every expectation here was measured
//! by restoring that same file there.

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

/// Splits a dump the way the `mysql` client 8.4.11 does, read from MySQL's
/// general log while it restored this dump: a comment line between statements
/// is sent as a statement of its own, since the client keeps comments by
/// default; blank lines are dropped; `DELIMITER` changes what ends a
/// statement and is not sent; and a statement ends at the line that ends with
/// the delimiter, which is taken off.
fn statements_the_client_sends(dump: &str) -> Vec<String> {
    let mut delimiter = ";".to_owned();
    let mut statements = Vec::new();
    let mut pending = String::new();
    for line in dump.lines() {
        if pending.is_empty() {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with("--") {
                statements.push(line.to_owned());
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
    replay(&mut adapter, DUMP);
    (directory, adapter)
}

/// Sends a dump as the client does, and fails on any statement refused.
fn replay(adapter: &mut Adapter, dump: &str) {
    let refused = statements_the_client_sends(dump)
        .into_iter()
        .filter_map(|sql| {
            send_as_the_client(adapter, &sql)
                .err()
                .map(|error| format!("{error:?}: {sql}"))
        })
        .collect::<Vec<_>>();
    assert!(refused.is_empty(), "{}", refused.join("\n"));
}

/// Sends one statement, and after a `USE` what the client sends to learn the
/// database it is in, as the general log shows: `SELECT DATABASE()` and then
/// `COM_INIT_DB` naming it.
fn send_as_the_client(adapter: &mut Adapter, sql: &str) -> Result<(), FrontendErrorKind> {
    adapter.execute_query(sql)?;
    if let Some(database) = sql.strip_prefix("USE ") {
        adapter.execute_query("SELECT DATABASE()")?;
        adapter.execute_init_db(database.trim_matches('`'))?;
    }
    Ok(())
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
    // Every escape the dump wrote reads back as the byte it stands for.
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM articles ORDER BY id"),
        [
            row(&[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("First post"),
                Some("published"),
                Some("10"),
                Some("2026-03-01 10:00:00.123456"),
            ]),
            row(&[
                Some("2"),
                Some("1"),
                Some("Second"),
                Some("More text"),
                Some("draft"),
                Some("0"),
                None,
            ]),
            row(&[
                Some("3"),
                Some("3"),
                Some("Carol's post"),
                Some("It's quoted"),
                Some("published"),
                Some("5"),
                Some("2026-09-28 01:45:12.786797"),
            ]),
            row(&[
                Some("4"),
                Some("2"),
                Some(r#"Back\slash "double" 'single'"#),
                Some("line one\nline two\r\n\ttabbed"),
                Some("draft"),
                Some("0"),
                Some("2026-01-01 00:00:00.000000"),
            ]),
            row(&[
                Some("5"),
                Some("2"),
                Some("Nul and Ctrl-Z"),
                Some("a\0b\u{1a}c"),
                Some("published"),
                Some("7"),
                Some("2026-01-01 00:00:00.500000"),
            ]),
            row(&[
                Some("6"),
                Some("3"),
                Some("Emoji 😀 and ünïcödé"),
                Some("percent % underscore _ backtick ` dollar $$"),
                Some("published"),
                Some("2147483647"),
                Some("9999-12-31 23:59:59.999999"),
            ]),
            row(&[
                Some("7"),
                Some("1"),
                Some(""),
                Some(""),
                Some("draft"),
                Some("-1"),
                Some("1000-01-01 00:00:00.000000"),
            ]),
            row(&[
                Some("8"),
                Some("2"),
                Some(r#"JSON-like {"a": "b\"c"}"#),
                Some(r#"{"q": "it's \"x\"\n"}"#),
                Some("draft"),
                Some("3"),
                Some("2026-02-28 23:59:59.000001"),
            ]),
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

/// A dump's extended `INSERT` runs to a MiB, and every reading of a statement
/// costs time in proportion to its length, so the long statement is read
/// whole only a few times on its way to running: tokenized once for the
/// session's dialect and once for sqlparser's own, parsed once into a syntax
/// tree, read once by the engine, once by each of the three ways the command
/// tokenizer reads it and checked once as a counted `INSERT` — and, since it
/// names no columns, tokenized and parsed once more with its column list
/// written out. The same holds for a table that counts no ids, which is
/// checked as a counted `INSERT` before it is found to be none.
///
/// Before the readings were kept while a statement is answered, the one into
/// the counted table was tokenized 26 times, parsed 8 times and read by the
/// command tokenizer 23 times, and the other was tokenized 33 times, parsed
/// 13 times, read by the engine 6 times and checked as a counted `INSERT` 3.
#[test]
fn a_dumps_long_insert_is_read_whole_only_a_few_times() {
    let (_directory, mut adapter) = restoring_session();
    run(&mut adapter, "CREATE DATABASE long_rows");
    send_as_the_client(&mut adapter, "USE `long_rows`").unwrap();
    run(
        &mut adapter,
        "CREATE TABLE `articles` (`id` bigint unsigned NOT NULL AUTO_INCREMENT, `title` varchar(200) NOT NULL, `body` text, `views` int NOT NULL DEFAULT '0', `published_at` datetime(6) DEFAULT NULL, PRIMARY KEY (`id`))",
    );
    run(
        &mut adapter,
        "CREATE TABLE `article_tags` (`article_id` bigint unsigned NOT NULL, `tag` varchar(50) NOT NULL, `note` text, PRIMARY KEY (`article_id`,`tag`))",
    );
    let articles = (1..=300)
        .map(|id| {
            format!(
                "({id},'Post {id}','It\\'s line one\\nline two',{id},'2026-03-01 10:00:00.123456')"
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let tags = (1..=300)
        .map(|id| format!("({id},'tag {id}','It\\'s tagged')"))
        .collect::<Vec<_>>()
        .join(",");

    for sql in [
        format!("INSERT INTO `articles` VALUES {articles}"),
        format!("INSERT INTO `article_tags` VALUES {tags}"),
    ] {
        let before = turso_mysql_parser::bytes_read();
        assert_eq!(ok(&mut adapter, &sql).affected_rows, 300);
        assert_eq!(
            whole_readings(before, turso_mysql_parser::bytes_read(), sql.len()),
            turso_mysql_parser::BytesRead {
                tokenized: 3,
                parsed: 2,
                parsed_by_the_engine: 1,
                tokenized_as_a_command: 3,
                checked_as_a_counted_insert: 1,
            },
            "{}",
            &sql[..40]
        );
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(*), MAX(id), MIN(body) FROM articles"
        ),
        [row(&[
            Some("300"),
            Some("300"),
            Some("It's line one\nline two")
        ])]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(*), MAX(article_id), MIN(note) FROM article_tags"
        ),
        [row(&[Some("300"), Some("300"), Some("It's tagged")])]
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
/// read ASCII alike, so ASCII is what is taken and answered while latin1 is
/// named.
#[test]
fn latin1_takes_ascii_statements_and_answers_ascii_results() {
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
    assert_eq!(
        rows(&mut adapter, "SELECT name FROM names WHERE id = 2"),
        [row(&[Some("Zoe")])]
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

/// The restored schema, and the catalog it lives in, for a second session.
fn restored_catalog() -> (tempfile::TempDir, Arc<MySqlDatabaseCatalog>) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("dump_owner"));
    let (directory, catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes(ACCOUNT),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    replay(&mut adapter, DUMP);
    (directory, catalog)
}

/// A session of the dump's account, with nothing selected, as `mysqldump`
/// opens one.
fn session_on(catalog: &Arc<MySqlDatabaseCatalog>) -> Adapter {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("dump_owner"));
    let mut adapter =
        AuthorizedDatabaseAdapterFactory::new(Arc::clone(catalog), binary_context(), authorizer)
            .build(AuthenticatedPrincipal::from_account_id_for_testing(
                AccountId::from_bytes(ACCOUNT),
            ))
            .unwrap();
    adapter.authorize_connection().unwrap();
    adapter
}

fn in_transaction(adapter: &Adapter) -> bool {
    adapter.status_flags() & SERVER_STATUS_IN_TRANS != 0
}

/// What `mysqldump --single-transaction --routines --triggers --events
/// --hex-blob --databases probe` 8.4.11 sent to MySQL, read from the general
/// log, one table's worth of it. Every statement is answered, and the dump's
/// reads all come from the one transaction it opened before it selected the
/// database. Measured on MySQL 8.4.11: `START TRANSACTION` with no database
/// selected opens a transaction, which `UNLOCK TABLES` and `COM_INIT_DB` leave
/// open, and `USE` of the database already selected leaves it open too.
#[test]
fn a_single_transaction_dump_is_answered_from_one_transaction() {
    let (_directory, catalog) = restored_catalog();
    let mut dump = session_on(&catalog);
    for sql in [
        "/*!40100 SET @@SQL_MODE='' */",
        "/*!40103 SET TIME_ZONE='+00:00' */",
        "/*!80000 SET SESSION information_schema_stats_expiry=0 */",
        "SET SESSION NET_READ_TIMEOUT= 86400, SESSION NET_WRITE_TIMEOUT= 86400",
        "SET @@SESSION.terminology_use_previous = NONE",
        "SHOW VARIABLES LIKE 'gtid_mode'",
        "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    ] {
        dump.execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    assert!(!in_transaction(&dump));
    run(
        &mut dump,
        "START TRANSACTION /*!40100 WITH CONSISTENT SNAPSHOT */",
    );
    assert!(in_transaction(&dump));
    run(&mut dump, "UNLOCK TABLES");
    assert!(in_transaction(&dump));
    run(&mut dump, "SHOW VARIABLES LIKE 'ndbinfo\\_version'");
    dump.execute_init_db("probe").unwrap();
    assert!(in_transaction(&dump));
    run(&mut dump, "SAVEPOINT sp");
    assert_eq!(
        rows(&mut dump, "SELECT COUNT(*) FROM audit"),
        [row(&[Some("3")])]
    );

    // A row another session commits now is not in what the dump reads.
    let mut writer = session_on(&catalog);
    writer.execute_init_db("probe").unwrap();
    run(&mut writer, "INSERT INTO audit (note) VALUES ('later')");

    for sql in [
        "show table status like 'audit'",
        "SET SQL_QUOTE_SHOW_CREATE=1",
        "SET SESSION character_set_results = 'binary'",
        "show create table `audit`",
        "SET SESSION character_set_results = 'utf8mb4'",
        "show fields from `audit`",
        "SET SESSION character_set_results = 'binary'",
        "use `probe`",
        "select @@collation_database",
        "SHOW TRIGGERS LIKE 'audit'",
        "SET SESSION character_set_results = 'utf8mb4'",
        "SET SESSION character_set_results = 'binary'",
        "SELECT COLUMN_NAME,                       JSON_EXTRACT(HISTOGRAM, '$.\"number-of-buckets-specified\"')                FROM information_schema.COLUMN_STATISTICS                WHERE SCHEMA_NAME = 'probe' AND TABLE_NAME = 'audit'",
        "SET SESSION character_set_results = 'utf8mb4'",
        "ROLLBACK TO SAVEPOINT sp",
        "RELEASE SAVEPOINT sp",
        "show events",
        "use `probe`",
        "select @@collation_database",
        "SET SESSION character_set_results = 'binary'",
        "SHOW FUNCTION STATUS WHERE Db = 'probe'",
        "SHOW PROCEDURE STATUS WHERE Db = 'probe'",
        "SET SESSION character_set_results = 'utf8mb4'",
    ] {
        dump.execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        assert!(in_transaction(&dump), "{sql}");
    }
    dump.execute_init_db("probe").unwrap();
    assert_eq!(
        rows(
            &mut dump,
            "SELECT /*!40001 SQL_NO_CACHE */ COUNT(*) FROM `audit`"
        ),
        [row(&[Some("3")])]
    );
    run(&mut dump, "COMMIT");
    assert!(!in_transaction(&dump));
    assert_eq!(
        rows(&mut dump, "SELECT COUNT(*) FROM audit"),
        [row(&[Some("4")])]
    );
}

/// A transaction begun with nothing selected is begun on the database
/// selected next, by `USE` as by `COM_INIT_DB`, and ended by `COMMIT` or
/// `ROLLBACK` even when none was selected; moving it to another database is
/// refused, the transaction being that database's.
#[test]
fn a_transaction_begun_before_a_database_is_selected_waits_for_one() {
    let (_directory, catalog) = restored_catalog();
    let mut session = session_on(&catalog);
    run(&mut session, "START TRANSACTION");
    run(&mut session, "ROLLBACK");
    assert!(!in_transaction(&session));
    run(&mut session, "BEGIN");
    run(&mut session, "COMMIT");
    assert!(!in_transaction(&session));

    run(&mut session, "START TRANSACTION");
    run(&mut session, "USE probe");
    assert!(in_transaction(&session));
    run(&mut session, "INSERT INTO audit (note) VALUES ('undone')");
    assert_eq!(
        session.execute_query("USE other_database"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        session.execute_init_db("other_database"),
        Err(FrontendErrorKind::Unsupported)
    );
    run(&mut session, "ROLLBACK");
    assert_eq!(
        rows(&mut session, "SELECT COUNT(*) FROM audit"),
        [row(&[Some("3")])]
    );
}

/// There are no stored programs and no histograms here, so each listing a
/// dump asks for answers no row, in the columns MySQL 8.4.11 answers it in.
#[test]
fn a_dump_finds_no_stored_programs_and_no_histograms() {
    let (_directory, catalog) = restored_catalog();
    let mut dump = session_on(&catalog);
    assert_eq!(
        dump.execute_query("show events"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    dump.execute_init_db("probe").unwrap();
    let listing = |dump: &mut Adapter, sql: &str| match dump.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => {
            assert!(result.rows.is_empty(), "{sql}");
            result.columns
        }
        other => panic!("{sql}: {other:?}"),
    };
    let events = listing(&mut dump, "show events");
    assert_eq!(
        events
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Db",
            "Name",
            "Definer",
            "Time zone",
            "Type",
            "Execute at",
            "Interval value",
            "Interval field",
            "Starts",
            "Ends",
            "Status",
            "Originator",
            "character_set_client",
            "collation_connection",
            "Database Collation",
        ]
    );
    let db = &events[0];
    assert_eq!(
        (
            db.schema.as_str(),
            db.table.as_str(),
            db.original_table.as_str(),
            db.column_length,
            db.flags
        ),
        (
            "information_schema",
            "EVENTS",
            "sch",
            256,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG
                | MYSQL_BINARY_FLAG
        )
    );
    assert_eq!(events[11].column_type, MYSQL_TYPE_LONG);

    for sql in [
        "SHOW FUNCTION STATUS WHERE Db = 'probe'",
        "SHOW PROCEDURE STATUS WHERE Db = 'probe'",
        "SHOW PROCEDURE STATUS LIKE 'p%'",
    ] {
        let routines = listing(&mut dump, sql);
        assert_eq!(
            routines
                .iter()
                .map(|column| (column.name.as_str(), column.original_table.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Db", "schemata"),
                ("Name", "routines"),
                ("Type", "routines"),
                ("Language", "routines"),
                ("Definer", "routines"),
                ("Modified", "routines"),
                ("Created", "routines"),
                ("Security_type", "routines"),
                ("Comment", "routines"),
                ("character_set_client", "character_sets"),
                ("collation_connection", "collations"),
                ("Database Collation", "collations"),
            ],
            "{sql}"
        );
        assert_eq!(routines[8].column_type, MYSQL_TYPE_BLOB);
    }
    assert_eq!(
        dump.execute_query("SHOW FUNCTION STATUS WHERE Name = 'f'"),
        Err(FrontendErrorKind::Unsupported)
    );

    let histograms = listing(
        &mut dump,
        "SELECT COLUMN_NAME,                       JSON_EXTRACT(HISTOGRAM, '$.\"number-of-buckets-specified\"')                FROM information_schema.COLUMN_STATISTICS                WHERE SCHEMA_NAME = 'probe' AND TABLE_NAME = 'posts'",
    );
    assert_eq!(histograms[0].name, "COLUMN_NAME");
    assert_eq!(histograms[0].original_table, "COLUMN_STATISTICS");
    assert_eq!(
        histograms[1].name,
        "JSON_EXTRACT(HISTOGRAM, '$.\"number-of-buckets-specified\"')"
    );
    assert_eq!(histograms[1].column_type, MYSQL_TYPE_JSON);
    assert_eq!(histograms[1].column_length, 4_294_967_292);
}

/// The framework harness's `mysqldump` app, replayed here. `dump_src` was
/// seeded on MySQL 8.4.11 with the apps' `schema.sql` and `data.sql`, plus 40
/// posts written with a quote, a backslash, a double quote and a line break,
/// and dumped without `--databases`, as the harness dumps it: with the default
/// options, and with `--single-transaction --routines --triggers --events
/// --hex-blob --complete-insert --net-buffer-length=4096`, which names every
/// column and splits the posts into three `INSERT`s.
const APPS_DEFAULT: &str = include_str!("mysqldump_apps_default.sql");
const APPS_OPTIONS: &str = include_str!("mysqldump_apps_options.sql");
/// The same database dumped with `--no-data`.
const APPS_NO_DATA: &str = include_str!("mysqldump_apps_no_data.sql");
/// What the harness's `rows_of` printed on MySQL for `dump_src`, and for the
/// database each of the two dumps was replayed into there, byte for byte.
const APPS_ROWS: &str = include_str!("mysqldump_apps_rows.tsv");

/// The harness's `replay-default`, `same-rows-after-replay`, `replay-backup`
/// and `same-rows-after-second-replay`: both dumps are replayed into one
/// database the client names as it connects, the second over the first, and
/// each leaves the rows MySQL left.
#[test]
fn the_framework_apps_dumps_replay_into_another_database_with_the_same_rows() {
    assert_eq!(APPS_OPTIONS.matches("INSERT INTO `posts` (").count(), 3);
    let (_directory, mut adapter) = restoring_session();
    ok(&mut adapter, "CREATE DATABASE dump_dst");
    adapter.execute_init_db("dump_dst").unwrap();
    replay(&mut adapter, APPS_NO_DATA);
    for table in ["users", "posts", "tags", "post_tag"] {
        assert_eq!(
            rows(&mut adapter, &format!("SELECT COUNT(*) FROM {table}")),
            [row(&[Some("0")])],
            "{table}"
        );
    }
    for dump in [APPS_DEFAULT, APPS_OPTIONS] {
        replay(&mut adapter, dump);
        assert_eq!(rows_as_the_harness_prints(&mut adapter), APPS_ROWS);
    }
}

/// The harness's `rows_of`, printed as `mysql --batch --skip-column-names`
/// prints it: tab between values, `NULL` for NULL, and a backslash before a
/// tab, a line break, a NUL and a backslash.
fn rows_as_the_harness_prints(adapter: &mut Adapter) -> String {
    let mut printed = String::new();
    for sql in [
        "SELECT id, email, name, balance, is_active, profile, created_at FROM users ORDER BY id",
        "SELECT id, user_id, title, body, status, views, published_at FROM posts ORDER BY id",
        "SELECT id, name FROM tags ORDER BY id",
        "SELECT post_id, tag_id FROM post_tag ORDER BY post_id, tag_id",
    ] {
        for values in rows(adapter, sql) {
            let line = values
                .iter()
                .map(|value| match value {
                    None => "NULL".to_owned(),
                    Some(value) => value
                        .replace('\\', "\\\\")
                        .replace('\t', "\\t")
                        .replace('\n', "\\n")
                        .replace('\0', "\\0"),
                })
                .collect::<Vec<_>>()
                .join("\t");
            printed.push_str(&line);
            printed.push('\n');
        }
    }
    printed
}
