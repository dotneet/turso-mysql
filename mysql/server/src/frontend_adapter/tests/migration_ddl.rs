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
        "CREATE TABLE notes (id INT NOT NULL PRIMARY KEY, body TEXT) COMMENT='notes table'",
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
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='notes table'"
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
    run(
        &mut adapter,
        "ALTER TABLE s1 ALTER COLUMN d SET DEFAULT '4.5'",
    );
    let expected = concat!(
        "CREATE TABLE `s1` (\n",
        "  `id` int NOT NULL,\n",
        "  `a` int DEFAULT '5',\n",
        "  `b` int NOT NULL,\n",
        "  `c` varchar(10) DEFAULT 'it''s',\n",
        "  `d` decimal(8,2) DEFAULT '4.50',\n",
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
            some(&["d", "4.50"]),
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
            Some("4.50".to_owned())
        ]]
    );

    for sql in [
        // Measured: 1067, 1067, 1101 and 1054.
        "ALTER TABLE s1 ALTER COLUMN a SET DEFAULT 'abc'",
        "ALTER TABLE s1 ALTER COLUMN b SET DEFAULT NULL",
        "ALTER TABLE s1 ALTER COLUMN f SET DEFAULT 'x'",
        "ALTER TABLE s1 ALTER COLUMN nope SET DEFAULT 1",
        // Taken by MySQL, which then prints the column with no default at
        // all, a shape this cannot keep.
        "ALTER TABLE s1 ALTER COLUMN a DROP DEFAULT",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }

    let mut adapter = reopened(&directory, adapter);
    assert!(printed_table(&mut adapter, "s1").contains("  `b` int NOT NULL,\n"));
}

/// A `DECIMAL` column restated by `MODIFY` in the same form is read by the
/// session that changed it, not only once the database is opened again.
///
/// Measured on MySQL 8.4.11: after `MODIFY d DECIMAL(12,3)` a stored 1.5 reads
/// `1.500`, every row written again in the new form. The engine keeps a
/// `DECIMAL` as the text it was written as, so a change of size or sign, or
/// into or out of a `DECIMAL`, is refused.
#[test]
fn a_decimal_column_restated_by_modify_is_read_by_the_session_that_changed_it() {
    let (directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE s2 (id INT NOT NULL PRIMARY KEY, d DECIMAL(8,2), n INT)",
    );
    run(&mut adapter, "INSERT INTO s2 VALUES (1, 1.5, 2)");
    run(
        &mut adapter,
        "ALTER TABLE s2 MODIFY d DECIMAL(8,2) NOT NULL",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT d FROM s2"),
        vec![some(&["1.50"])]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM s2"),
        vec![some(&["1", "1.50", "2"])]
    );
    for sql in [
        "ALTER TABLE s2 MODIFY d DECIMAL(12,3)",
        "ALTER TABLE s2 MODIFY d DECIMAL(8,2) UNSIGNED",
        "ALTER TABLE s2 CHANGE d e INT",
        "ALTER TABLE s2 MODIFY n DECIMAL(8,2)",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    let mut adapter = reopened(&directory, adapter);
    assert_eq!(
        rows(&mut adapter, "SELECT d FROM s2"),
        vec![some(&["1.50"])]
    );
}

/// A `DECIMAL` and a `BIGINT UNSIGNED` are kept in an encoded form, and a
/// default is encoded once for every row that takes it: an insert of several
/// rows used to encode the first row's default again for the second and fail.
/// Measured on MySQL 8.4.11 with the same statements.
#[test]
fn every_row_of_an_insert_takes_the_encoded_defaults() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE v4 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, \
         balance DECIMAL(10,2) NOT NULL DEFAULT '0.00', spare DECIMAL(10,2) DEFAULT '1.50', \
         big BIGINT UNSIGNED DEFAULT 18446744073709551615)",
    );
    run(&mut adapter, "INSERT INTO v4 (name) VALUES ('a'), ('b')");
    run(
        &mut adapter,
        "CREATE TABLE v5 (id INT NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL, \
         balance DECIMAL(10,2) NOT NULL DEFAULT '0.00')",
    );
    run(
        &mut adapter,
        "INSERT INTO v5 (id, name) VALUES (1, 'a'), (2, 'b')",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT * FROM v4 ORDER BY id"),
        vec![
            some(&["1", "a", "0.00", "1.50", "18446744073709551615"]),
            some(&["2", "b", "0.00", "1.50", "18446744073709551615"]),
        ]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, balance FROM v5 ORDER BY id"),
        vec![some(&["1", "0.00"]), some(&["2", "0.00"])]
    );
}

/// Laravel's `comment()` on a table writes `alter table t comment = 'x'` and
/// Rails writes `ALTER TABLE t COMMENT 'x'`.
#[test]
fn a_table_keeps_its_comment() {
    let (directory, mut adapter) = adapter();
    run(&mut adapter, "CREATE TABLE c1 (id INT) COMMENT='hello'");
    run(
        &mut adapter,
        "CREATE TABLE c2 (id INT NOT NULL PRIMARY KEY) ENGINE=InnoDB COMMENT 'it''s a \\\\ back' DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
    );
    run(
        &mut adapter,
        "CREATE TABLE c3 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, n INT) COMMENT=''",
    );
    run(&mut adapter, "alter table `c3` comment = 'x'");
    run(&mut adapter, "ALTER TABLE c1 COMMENT 'y'");
    run(&mut adapter, "ALTER TABLE c1 ADD COLUMN n INT");
    run(&mut adapter, "CREATE INDEX c1_n ON c1 (n)");

    let expected_c1 = concat!(
        "CREATE TABLE `c1` (\n",
        "  `id` int DEFAULT NULL,\n",
        "  `n` int DEFAULT NULL,\n",
        "  KEY `c1_n` (`n`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='y'"
    );
    let expected_c2 = concat!(
        "CREATE TABLE `c2` (\n",
        "  `id` int NOT NULL,\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='it''s a \\\\ back'"
    );
    let expected_c3 = concat!(
        "CREATE TABLE `c3` (\n",
        "  `id` int NOT NULL AUTO_INCREMENT,\n",
        "  `n` int DEFAULT NULL,\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='x'"
    );
    for reopen in [false, true] {
        if reopen {
            adapter = reopened(&directory, adapter);
        }
        let adapter = &mut adapter;
        assert_eq!(printed_table(adapter, "c1"), expected_c1);
        assert_eq!(printed_table(adapter, "c2"), expected_c2);
        assert_eq!(printed_table(adapter, "c3"), expected_c3);
        assert_eq!(
            rows(
                adapter,
                "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME LIKE 'c%' ORDER BY TABLE_NAME"
            ),
            vec![
                some(&["c1", "y"]),
                some(&["c2", "it's a \\ back"]),
                some(&["c3", "x"]),
            ]
        );
        let status = rows(adapter, "SHOW TABLE STATUS LIKE 'c%'");
        assert_eq!(
            status
                .iter()
                .map(|row| row[17].clone().unwrap())
                .collect::<Vec<_>>(),
            ["y", "it's a \\ back", "x"]
        );
    }

    run(&mut adapter, "ALTER TABLE c1 COMMENT ''");
    assert!(printed_table(&mut adapter, "c1").ends_with("COLLATE=utf8mb4_0900_ai_ci"));
    run(&mut adapter, "INSERT INTO c3 (n) VALUES (1)");
    assert_eq!(
        rows(&mut adapter, "SELECT id, n FROM c3"),
        vec![some(&["1", "1"])]
    );
}

/// Django writes `double precision` for every `FloatField`.
#[test]
fn double_precision_and_real_are_doubles() {
    let (directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE f1 (a DOUBLE PRECISION, b REAL, c DOUBLE PRECISION UNSIGNED, d REAL NOT NULL DEFAULT 1.5, e FLOAT4, f FLOAT8)",
    );
    run(
        &mut adapter,
        "INSERT INTO f1 (a, b, c, d) VALUES (0.1, 0.1, 0.1, 0.1)",
    );
    let expected = concat!(
        "CREATE TABLE `f1` (\n",
        "  `a` double DEFAULT NULL,\n",
        "  `b` double DEFAULT NULL,\n",
        "  `c` double unsigned DEFAULT NULL,\n",
        "  `d` double NOT NULL DEFAULT '1.5',\n",
        "  `e` float DEFAULT NULL,\n",
        "  `f` double DEFAULT NULL\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    for reopen in [false, true] {
        if reopen {
            adapter = reopened(&directory, adapter);
        }
        let adapter = &mut adapter;
        assert_eq!(printed_table(adapter, "f1"), expected);
        assert_eq!(
            rows(adapter, "SELECT a, b, c, d FROM f1"),
            vec![some(&["0.1", "0.1", "0.1", "0.1"])]
        );
        assert_eq!(
            rows(
                adapter,
                "SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'f1' ORDER BY ORDINAL_POSITION"
            ),
            vec![
                some(&["a", "double", "double"]),
                some(&["b", "double", "double"]),
                some(&["c", "double", "double unsigned"]),
                some(&["d", "double", "double"]),
                some(&["e", "float", "float"]),
                some(&["f", "double", "double"]),
            ]
        );
    }
}

fn raw_rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<Vec<u8>>>> {
    let result = adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    let CommandExecutionResult::ResultSet(result) = result else {
        panic!("{sql} must return a result set");
    };
    result.rows
}

/// The null bitmap, the new-parameters flag and one TINY parameter, which is
/// how Connector/J binds a Java `boolean` on the server.
fn one_tiny(value: u8) -> Vec<u8> {
    vec![0, 1, MYSQL_TYPE_TINY, 0, value]
}

/// Hibernate 6 maps a Java `Boolean` to `bit` on MySQL, and writes and reads it
/// as 0 and 1.
#[test]
fn a_hibernate_boolean_is_a_bit() {
    let (directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "create table settings (id bigint not null auto_increment, active bit not null, flag bit(1) default b'1', spare bit default 0, primary key (id)) engine=InnoDB",
    );
    run(
        &mut adapter,
        "insert into settings (active) values (1), (0), (TRUE), (FALSE)",
    );
    let statement = adapter
        .execute_stmt_prepare("insert into settings (active, spare) values (?, ?)")
        .unwrap();
    let mut payload = vec![0, 1, MYSQL_TYPE_TINY, 0, MYSQL_TYPE_LONGLONG, 0, 1];
    payload.extend_from_slice(&1_i64.to_le_bytes());
    adapter
        .execute_stmt_execute(statement.statement_id, &payload)
        .unwrap();
    run(
        &mut adapter,
        "update settings set active = true where id = 2",
    );

    // Measured on MySQL 8.4.11: a bit crosses the text protocol as the one
    // byte that holds it, a `bit(1)` column reporting the BIT type, a length
    // of 1, the binary collation and the unsigned flag.
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("select active, flag, spare from settings order by id")
        .unwrap()
    else {
        panic!("a SELECT answers rows");
    };
    let column = &result.columns[0];
    assert_eq!(column.column_type, MYSQL_TYPE_BIT);
    assert_eq!(column.column_length, 1);
    assert_eq!(column.character_set, MYSQL_BINARY_COLLATION);
    assert_eq!(
        column.flags & (MYSQL_UNSIGNED_FLAG | MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG),
        MYSQL_UNSIGNED_FLAG | MYSQL_NOT_NULL_FLAG
    );
    let bits = |byte: u8| Some(vec![byte]);
    assert_eq!(
        result.rows,
        vec![
            vec![bits(1), bits(1), bits(0)],
            vec![bits(1), bits(1), bits(0)],
            vec![bits(1), bits(1), bits(0)],
            vec![bits(0), bits(1), bits(0)],
            vec![bits(1), bits(1), bits(1)],
        ]
    );

    // The binary protocol sends the same byte, length-encoded.
    let statement = adapter
        .execute_stmt_prepare("select active from settings where id = 4")
        .unwrap();
    assert_eq!(statement.columns[0].column_type, MYSQL_TYPE_BIT);
    let PreparedStatementExecutionResult::ResultSet(binary) = adapter
        .execute_stmt_execute(statement.statement_id, &[])
        .unwrap()
    else {
        panic!("a prepared SELECT answers rows");
    };
    assert_eq!(binary.rows, vec![vec![BinaryResultValue::Blob(vec![0])]]);

    for (sql, expected) in [
        (
            "select id from settings where active = 1 order by id",
            ["1", "2", "3", "5"].as_slice(),
        ),
        (
            "select id from settings where active = true order by id",
            &["1", "2", "3", "5"],
        ),
        (
            "select id from settings where active = 0 order by id",
            &["4"],
        ),
        (
            "select id from settings where active = false order by id",
            &["4"],
        ),
        (
            "select id from settings where active <> 0 order by id",
            &["1", "2", "3", "5"],
        ),
        (
            "select id from settings where active in (0) order by id",
            &["4"],
        ),
        ("select id from settings where active = 2 order by id", &[]),
    ] {
        let found = rows(&mut adapter, sql)
            .into_iter()
            .map(|row| row[0].clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(found, expected, "{sql}");
    }

    // Measured: 2, -1, 1.5 and a word are each 1406, a byte of `'1'` being
    // wider than the one bit.
    for sql in [
        "insert into settings (active) values (2)",
        "insert into settings (active) values (-1)",
        "insert into settings (active) values ('1')",
        "update settings set active = 2 where id = 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    let statement = adapter
        .execute_stmt_prepare("insert into settings (active) values (?)")
        .unwrap();
    assert!(adapter
        .execute_stmt_execute(statement.statement_id, &one_tiny(2))
        .is_err());
    // A bound value compared against a bit is refused: nothing says what kind
    // of value it is until it binds.
    assert!(adapter
        .execute_stmt_prepare("select id from settings where active = ?")
        .is_err());

    // A call or an aggregate over a bit has not been measured.
    for sql in [
        "select active + 0 from settings",
        "select max(active) from settings",
        "select ifnull(flag, 0) from settings",
        // Taken by MySQL; a bit written as a bit literal is not read here.
        "insert into settings (active) values (b'1')",
        "select id from settings where active = b'1'",
        "create table wide (b bit(8))",
        "create table defaulted (b bit default 2)",
        "create table defaulted (b bit default '1')",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }

    let expected_columns = [
        "  `active` bit(1) NOT NULL,\n",
        "  `flag` bit(1) DEFAULT b'1',\n",
        "  `spare` bit(1) DEFAULT b'0',\n",
    ];
    for reopen in [false, true] {
        if reopen {
            adapter = reopened(&directory, adapter);
        }
        let adapter = &mut adapter;
        let printed = printed_table(adapter, "settings");
        for line in expected_columns {
            assert!(printed.contains(line), "{printed}");
        }
        let shown = rows(adapter, "SHOW COLUMNS FROM settings");
        assert_eq!(shown[1][1].as_deref(), Some("bit(1)"));
        assert_eq!(shown[2][4].as_deref(), Some("b'1'"));
        assert_eq!(
            rows(
                adapter,
                "SELECT COLUMN_NAME, COLUMN_DEFAULT, DATA_TYPE, COLUMN_TYPE, NUMERIC_PRECISION, NUMERIC_SCALE \
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'settings' \
                 ORDER BY ORDINAL_POSITION"
            )[1..],
            [
                vec![Some("active".to_owned()), None, Some("bit".to_owned()), Some("bit(1)".to_owned()), Some("1".to_owned()), None],
                vec![Some("flag".to_owned()), Some("b'1'".to_owned()), Some("bit".to_owned()), Some("bit(1)".to_owned()), Some("1".to_owned()), None],
                vec![Some("spare".to_owned()), Some("b'0'".to_owned()), Some("bit".to_owned()), Some("bit(1)".to_owned()), Some("1".to_owned()), None],
            ]
        );
        assert_eq!(
            raw_rows(adapter, "select active from settings where id = 5"),
            vec![vec![Some(vec![1])]]
        );
    }
}

/// `ALTER TABLE t ENGINE=InnoDB`, which Django and Rails write in some
/// migrations, names the engine every table here has. Measured on MySQL
/// 8.4.11: the table is rebuilt and nothing a client can see changes — the
/// counter of a table whose top rows were deleted stays where it stood — and
/// the statement commits what came before it, so a `ROLLBACK` after it keeps
/// the row written before it. `ENGINE=MyISAM` is taken there and printed back,
/// and is refused here; a table that is not there is 1146.
#[test]
fn restating_the_engine_changes_nothing_but_commits() {
    let (_directory, mut adapter) = adapter();
    let adapter = &mut adapter;
    run(
        adapter,
        "CREATE TABLE ai (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)",
    );
    run(adapter, "INSERT INTO ai (v) VALUES (1), (2), (3), (4), (5)");
    run(adapter, "DELETE FROM ai WHERE id >= 4");
    run(adapter, "ALTER TABLE ai ENGINE=InnoDB");
    assert!(printed_table(adapter, "ai").contains(") ENGINE=InnoDB AUTO_INCREMENT=6 "));

    run(adapter, "BEGIN");
    run(adapter, "INSERT INTO ai (id, v) VALUES (500, 1)");
    run(adapter, "ALTER TABLE ai ENGINE = InnoDB");
    run(adapter, "ROLLBACK");
    assert_eq!(
        rows(adapter, "SELECT COUNT(*) FROM ai WHERE id = 500"),
        [[Some("1".to_owned())]]
    );

    assert!(adapter
        .execute_query("ALTER TABLE ai ENGINE=MyISAM")
        .is_err());
    assert_eq!(
        adapter.execute_query("ALTER TABLE missing ENGINE=InnoDB"),
        Err(FrontendErrorKind::MissingObject)
    );
}

/// `ALTER TABLE t AUTO_INCREMENT = n` says where the numbering goes on from.
/// Measured on MySQL 8.4.11: the next row takes `n`, or one past the highest
/// id the table holds when `n` is not past it — 3 over ids up to 6 leaves 7
/// next, 100 makes 100 next, and 50 after that leaves 101 — and a table that
/// counts nothing takes the statement and changes nothing. MySQL also moves
/// the counter back when the rows above it are gone, which the allocator
/// cannot, so that is refused; so is a number past the column's type, which
/// MySQL takes and then answers 1467 for at the next row.
#[test]
fn the_counter_moves_where_mysql_moves_it() {
    let (_directory, mut adapter) = adapter();
    let adapter = &mut adapter;
    run(
        adapter,
        "CREATE TABLE ai (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)",
    );
    run(
        adapter,
        "INSERT INTO ai (v) VALUES (1), (2), (3), (4), (5), (6)",
    );
    let next_id = |adapter: &mut Adapter| {
        run(adapter, "INSERT INTO ai (v) VALUES (0)");
        rows(adapter, "SELECT LAST_INSERT_ID()")[0][0]
            .clone()
            .unwrap()
    };
    run(adapter, "ALTER TABLE ai AUTO_INCREMENT = 3");
    assert_eq!(next_id(adapter), "7");
    run(adapter, "ALTER TABLE ai AUTO_INCREMENT = 100");
    assert_eq!(next_id(adapter), "100");
    run(adapter, "ALTER TABLE ai AUTO_INCREMENT = 50");
    assert_eq!(next_id(adapter), "101");
    assert!(printed_table(adapter, "ai").contains(") ENGINE=InnoDB AUTO_INCREMENT=102 "));

    run(adapter, "DELETE FROM ai WHERE id >= 7");
    assert!(adapter
        .execute_query("ALTER TABLE ai AUTO_INCREMENT = 7")
        .is_err());
    assert!(adapter
        .execute_query("ALTER TABLE ai AUTO_INCREMENT = 3000000000")
        .is_err());
    assert!(adapter
        .execute_query("ALTER TABLE ai AUTO_INCREMENT = '7'")
        .is_err());
    assert_eq!(next_id(adapter), "102");

    run(adapter, "CREATE TABLE plain (a INT)");
    run(adapter, "ALTER TABLE plain AUTO_INCREMENT = 10");
    assert!(!printed_table(adapter, "plain").contains("AUTO_INCREMENT"));
    assert_eq!(
        adapter.execute_query("ALTER TABLE missing AUTO_INCREMENT = 10"),
        Err(FrontendErrorKind::MissingObject)
    );
}
