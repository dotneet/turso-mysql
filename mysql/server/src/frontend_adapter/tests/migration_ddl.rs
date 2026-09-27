//! The DDL a framework's migrations write, as each spells it.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const ACCOUNT: [u8; 32] = [141; 32];

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let adapter = connected(factory);
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

fn printed_table(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE `{table}`"))[0][1]
        .clone()
        .unwrap()
}

fn defaults(adapter: &mut Adapter, table: &str) -> Vec<Vec<Option<String>>> {
    rows(
        adapter,
        &format!(
            "SELECT COLUMN_NAME, COLUMN_DEFAULT FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
        ),
    )
}

fn some(values: &[&str]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| Some((*value).to_owned()))
        .collect()
}

/// Laravel quotes every default it writes: `integer('votes')->default(0)` and
/// `boolean('active')->default(false)` both become `DEFAULT '0'`.
#[test]
fn a_laravel_table_takes_its_quoted_whole_number_defaults() {
    let (directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "create table `users` (`id` bigint unsigned not null auto_increment primary key, `votes` int not null default '0', `rank` smallint not null default '1', `level` tinyint not null default '1', `active` tinyint(1) not null default '0', `score` int default '4.5', `balance` bigint unsigned not null default '0') default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
    );
    run(
        &mut adapter,
        "CREATE TABLE keyed (id INT NOT NULL PRIMARY KEY, c INT NOT NULL DEFAULT '5')",
    );
    run(&mut adapter, "CREATE TABLE plain (c INT DEFAULT ' 7')");
    run(
        &mut adapter,
        "alter table `plain` add `flag` tinyint(1) not null default '1'",
    );
    run(
        &mut adapter,
        "ALTER TABLE `plain` MODIFY `c` INT NOT NULL DEFAULT '-4.5'",
    );

    let expected_users = concat!(
        "CREATE TABLE `users` (\n",
        "  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n",
        "  `votes` int NOT NULL DEFAULT '0',\n",
        "  `rank` smallint NOT NULL DEFAULT '1',\n",
        "  `level` tinyint NOT NULL DEFAULT '1',\n",
        "  `active` tinyint(1) NOT NULL DEFAULT '0',\n",
        "  `score` int DEFAULT '5',\n",
        "  `balance` bigint unsigned NOT NULL DEFAULT '0',\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"
    );
    let expected_keyed = concat!(
        "CREATE TABLE `keyed` (\n",
        "  `id` int NOT NULL,\n",
        "  `c` int NOT NULL DEFAULT '5',\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    let expected_plain = concat!(
        "CREATE TABLE `plain` (\n",
        "  `c` int NOT NULL DEFAULT '-5',\n",
        "  `flag` tinyint(1) NOT NULL DEFAULT '1'\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    for reopen in [false, true] {
        if reopen {
            adapter = reopened(&directory, adapter);
        }
        let adapter = &mut adapter;
        assert_eq!(printed_table(adapter, "users"), expected_users);
        assert_eq!(printed_table(adapter, "keyed"), expected_keyed);
        assert_eq!(printed_table(adapter, "plain"), expected_plain);
        assert_eq!(
            defaults(adapter, "users"),
            vec![
                vec![Some("id".to_owned()), None],
                some(&["votes", "0"]),
                some(&["rank", "1"]),
                some(&["level", "1"]),
                some(&["active", "0"]),
                some(&["score", "5"]),
                some(&["balance", "0"]),
            ]
        );
        let shown = rows(adapter, "SHOW COLUMNS FROM `plain`");
        assert_eq!(shown[0][4].as_deref(), Some("-5"));
        assert_eq!(shown[1][4].as_deref(), Some("1"));
    }

    run(&mut adapter, "INSERT INTO `users` () VALUES ()");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT votes, `rank`, level, active, score, balance FROM users"
        ),
        vec![some(&["0", "1", "1", "0", "5", "0"])]
    );
}

/// A word that names no number, or a number the column cannot hold once
/// rounded, is 1067 in MySQL and refused here.
#[test]
fn a_whole_number_default_mysql_refuses_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE t (c INT DEFAULT 'abc')",
        "CREATE TABLE t (c INT DEFAULT '')",
        "CREATE TABLE t (c TINYINT DEFAULT '300')",
        "CREATE TABLE t (c TINYINT UNSIGNED DEFAULT '-1')",
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, c TINYINT DEFAULT '127.5')",
        "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, c INT DEFAULT '5a')",
        "ALTER TABLE records ADD COLUMN c INT DEFAULT 'x'",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

fn refused_with(adapter: &mut Adapter, sql: &str) -> FrontendErrorKind {
    match adapter.execute_query(sql) {
        Err(error) => error,
        Ok(result) => panic!("{sql} must fail, answered {result:?}"),
    }
}

fn table_names(adapter: &mut Adapter) -> Vec<String> {
    rows(adapter, "SHOW TABLES")
        .into_iter()
        .map(|row| row[0].clone().unwrap())
        .collect()
}

/// Laravel's `Schema::rename` and Rails' `rename_table` write `RENAME TABLE`.
#[test]
fn rename_table_renames_every_pair_in_order_or_none_of_them() {
    let (directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20) NOT NULL)",
    );
    run(
        &mut adapter,
        "INSERT INTO posts (title) VALUES ('a'), ('b')",
    );
    run(
        &mut adapter,
        "CREATE TABLE notes (id INT NOT NULL PRIMARY KEY, body TEXT)",
    );
    run(&mut adapter, "INSERT INTO notes VALUES (1, 'n')");

    run(&mut adapter, "RENAME TABLE `posts` TO `articles`");
    run(&mut adapter, "INSERT INTO articles (title) VALUES ('c')");
    assert_eq!(
        rows(&mut adapter, "SELECT id, title FROM articles ORDER BY id"),
        vec![some(&["1", "a"]), some(&["2", "b"]), some(&["3", "c"])]
    );

    // A swap through a third name, which only works pair by pair.
    run(
        &mut adapter,
        "RENAME TABLE articles TO spare, notes TO articles, spare TO notes;",
    );
    let expected_articles = concat!(
        "CREATE TABLE `articles` (\n",
        "  `id` int NOT NULL,\n",
        "  `body` text,\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    let expected_notes = concat!(
        "CREATE TABLE `notes` (\n",
        "  `id` int NOT NULL AUTO_INCREMENT,\n",
        "  `title` varchar(20) NOT NULL,\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(printed_table(&mut adapter, "articles"), expected_articles);
    assert_eq!(printed_table(&mut adapter, "notes"), expected_notes);

    assert_eq!(
        refused_with(&mut adapter, "RENAME TABLE nope TO z"),
        FrontendErrorKind::MissingObject
    );
    assert_eq!(
        refused_with(&mut adapter, "RENAME TABLE articles TO notes"),
        FrontendErrorKind::DuplicateObject
    );
    assert_eq!(
        refused_with(&mut adapter, "RENAME TABLE articles TO articles"),
        FrontendErrorKind::DuplicateObject
    );
    // The second pair fails, and the first is not left renamed.
    assert_eq!(
        refused_with(&mut adapter, "RENAME TABLE articles TO q, nope TO r"),
        FrontendErrorKind::MissingObject
    );
    let names = table_names(&mut adapter);
    assert!(names.contains(&"articles".to_owned()), "{names:?}");
    assert!(!names.contains(&"q".to_owned()), "{names:?}");

    let mut adapter = reopened(&directory, adapter);
    assert_eq!(printed_table(&mut adapter, "articles"), expected_articles);
    assert_eq!(printed_table(&mut adapter, "notes"), expected_notes);
    run(&mut adapter, "INSERT INTO notes (title) VALUES ('d')");
    assert_eq!(
        rows(&mut adapter, "SELECT MAX(id) FROM notes"),
        vec![some(&["4"])]
    );
}

/// Laravel's `renameIndex` and Rails' `rename_index` write `ALTER TABLE t
/// RENAME INDEX a TO b`.
#[test]
fn rename_index_keeps_the_index_and_its_place() {
    let (directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE i5 (a INT, b INT, c INT, n INT, KEY ka (a), KEY kb (b), KEY kc (c), UNIQUE KEY un (n))",
    );
    run(&mut adapter, "ALTER TABLE i5 RENAME INDEX ka TO kz");
    run(&mut adapter, "alter table `i5` rename key `un` to `ux`");
    let expected = concat!(
        "CREATE TABLE `i5` (\n",
        "  `a` int DEFAULT NULL,\n",
        "  `b` int DEFAULT NULL,\n",
        "  `c` int DEFAULT NULL,\n",
        "  `n` int DEFAULT NULL,\n",
        "  UNIQUE KEY `ux` (`n`),\n",
        "  KEY `kz` (`a`),\n",
        "  KEY `kb` (`b`),\n",
        "  KEY `kc` (`c`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(printed_table(&mut adapter, "i5"), expected);
    // A renamed unique index still refuses a second row with the same value.
    run(&mut adapter, "INSERT INTO i5 (n) VALUES (1)");
    assert!(adapter
        .execute_query("INSERT INTO i5 (n) VALUES (1)")
        .is_err());

    // Two names swapped in one statement, each read against the names the
    // table had before it.
    run(
        &mut adapter,
        "ALTER TABLE i5 RENAME INDEX kb TO kc, RENAME INDEX kc TO kb",
    );
    let keys = rows(&mut adapter, "SHOW INDEX FROM i5")
        .iter()
        .map(|row| (row[2].clone().unwrap(), row[4].clone().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [("ux", "n"), ("kz", "a"), ("kc", "b"), ("kb", "c")]
            .map(|(key, column)| (key.to_owned(), column.to_owned()))
    );

    assert_eq!(
        refused_with(&mut adapter, "ALTER TABLE i5 RENAME INDEX nope TO k3"),
        FrontendErrorKind::KeyDoesNotExist
    );
    assert_eq!(
        refused_with(&mut adapter, "ALTER TABLE i5 RENAME INDEX kz TO ux"),
        FrontendErrorKind::DuplicateKeyName
    );
    assert_eq!(
        refused_with(
            &mut adapter,
            "ALTER TABLE i5 RENAME INDEX kz TO x, RENAME INDEX kz TO y"
        ),
        FrontendErrorKind::KeyDoesNotExist
    );

    let mut adapter = reopened(&directory, adapter);
    let shown = rows(&mut adapter, "SHOW INDEX FROM i5");
    assert_eq!(shown[1][2].as_deref(), Some("kz"));
}

/// Rails' `change_column_default` writes `ALTER TABLE t ALTER COLUMN c SET
/// DEFAULT ...`, and `DROP DEFAULT` for a column that may not be NULL.
#[test]
fn alter_column_sets_and_drops_a_default() {
    let (directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE s1 (id INT NOT NULL PRIMARY KEY, a INT, b INT NOT NULL DEFAULT 3, c VARCHAR(10) DEFAULT 'x', d DECIMAL(8,2), f TEXT)",
    );
    run(&mut adapter, "ALTER TABLE s1 ALTER COLUMN a SET DEFAULT 5");
    run(&mut adapter, "ALTER TABLE `s1` ALTER `b` DROP DEFAULT");
    run(
        &mut adapter,
        "ALTER TABLE s1 ALTER COLUMN c SET DEFAULT 'it''s'",
    );
    let expected = concat!(
        "CREATE TABLE `s1` (\n",
        "  `id` int NOT NULL,\n",
        "  `a` int DEFAULT '5',\n",
        "  `b` int NOT NULL,\n",
        "  `c` varchar(10) DEFAULT 'it''s',\n",
        "  `d` decimal(8,2) DEFAULT NULL,\n",
        "  `f` text,\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    assert_eq!(printed_table(&mut adapter, "s1"), expected);
    assert_eq!(
        defaults(&mut adapter, "s1"),
        vec![
            vec![Some("id".to_owned()), None],
            some(&["a", "5"]),
            vec![Some("b".to_owned()), None],
            some(&["c", "it's"]),
            vec![Some("d".to_owned()), None],
            vec![Some("f".to_owned()), None],
        ]
    );
    run(
        &mut adapter,
        "ALTER TABLE s1 ALTER COLUMN a SET DEFAULT NULL",
    );
    assert!(printed_table(&mut adapter, "s1").contains("  `a` int DEFAULT NULL,\n"));
    run(&mut adapter, "INSERT INTO s1 (id, b) VALUES (1, 1)");
    assert_eq!(
        rows(&mut adapter, "SELECT a, b, c, d FROM s1"),
        vec![vec![
            None,
            Some("1".to_owned()),
            Some("it's".to_owned()),
            None
        ]]
    );

    for sql in [
        // Measured: 1067, 1067, 1101 and 1054.
        "ALTER TABLE s1 ALTER COLUMN a SET DEFAULT 'abc'",
        "ALTER TABLE s1 ALTER COLUMN b SET DEFAULT NULL",
        "ALTER TABLE s1 ALTER COLUMN f SET DEFAULT 'x'",
        "ALTER TABLE s1 ALTER COLUMN nope SET DEFAULT 1",
        // Taken by MySQL. A `DECIMAL` column restated in place reads back as
        // one no `SELECT` here takes, so its default is not changed that way.
        "ALTER TABLE s1 ALTER COLUMN d SET DEFAULT '4.5'",
        // Taken by MySQL, which then prints the column with no default at
        // all, a shape this cannot keep.
        "ALTER TABLE s1 ALTER COLUMN a DROP DEFAULT",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }

    let mut adapter = reopened(&directory, adapter);
    assert!(printed_table(&mut adapter, "s1").contains("  `b` int NOT NULL,\n"));
}
