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
