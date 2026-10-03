//! A database's own collation, which every table made in it takes when the
//! table names neither a character set nor a collation — what Prisma and
//! Laravel create their database with.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const ACCOUNT: [u8; 32] = [0x63; 32];

fn session(catalog: &Arc<MySqlDatabaseCatalog>) -> Adapter {
    session_of(catalog, RecordingAuthorizer::default())
}

fn session_of(catalog: &Arc<MySqlDatabaseCatalog>, authorizer: RecordingAuthorizer) -> Adapter {
    let mut adapter = AuthorizedDatabaseAdapterFactory::new(
        catalog.clone(),
        binary_context(),
        Arc::new(authorizer),
    )
    .build(AuthenticatedPrincipal::from_account_id_for_testing(
        AccountId::from_bytes(ACCOUNT),
    ))
    .unwrap();
    adapter.authorize_connection().unwrap();
    adapter
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
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

fn row(values: &[&str]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| Some((*value).to_owned()))
        .collect()
}

fn show_create_table(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE {table}"))[0][1]
        .clone()
        .unwrap()
}

fn show_create_database(adapter: &mut Adapter, database: &str) -> String {
    rows(adapter, &format!("SHOW CREATE DATABASE {database}"))[0][1]
        .clone()
        .unwrap()
}

fn created_as(collation: &str) -> String {
    format!(
        "CREATE DATABASE `app` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE {collation} */ /*!80016 DEFAULT ENCRYPTION='N' */"
    )
}

const POSTS: &str = "CREATE TABLE `posts` (\n  `id` int NOT NULL,\n  `title` varchar(100) COLLATE utf8mb4_unicode_ci NOT NULL,\n  `body` text COLLATE utf8mb4_unicode_ci,\n  `kind` enum('a','b') COLLATE utf8mb4_unicode_ci DEFAULT NULL,\n  `code` varchar(5) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin DEFAULT NULL,\n  `n` int DEFAULT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci";

const PLAIN: &str = "CREATE TABLE `plain` (\n  `id` int NOT NULL,\n  `name` varchar(20) DEFAULT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

#[test]
fn a_table_made_in_a_unicode_ci_database_takes_its_collation() {
    let (_directory, catalog, _factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session(&catalog);
    assert_eq!(
        rows(&mut adapter, "SELECT @@collation_database"),
        [row(&["utf8mb4_0900_ai_ci"])]
    );
    run(
        &mut adapter,
        "CREATE DATABASE app CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    );
    run(&mut adapter, "USE app");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT @@collation_database, @@character_set_database"
        ),
        [row(&["utf8mb4_unicode_ci", "utf8mb4"])]
    );

    run(
        &mut adapter,
        "CREATE TABLE posts (id INT NOT NULL PRIMARY KEY, title VARCHAR(100) NOT NULL, body TEXT, kind ENUM('a','b'), code VARCHAR(5) COLLATE utf8mb4_bin, n INT);",
    );
    assert_eq!(show_create_table(&mut adapter, "posts"), POSTS);
    // A key written inside the table takes another path to the engine.
    run(
        &mut adapter,
        "CREATE TABLE tags (id INT PRIMARY KEY, name VARCHAR(20), KEY idx_name (name)) ENGINE=InnoDB -- a trailing comment",
    );
    assert_eq!(
        show_create_table(&mut adapter, "tags"),
        "CREATE TABLE `tags` (\n  `id` int NOT NULL,\n  `name` varchar(20) COLLATE utf8mb4_unicode_ci DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `idx_name` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"
    );
    // The same path takes a table that names the collation itself.
    run(&mut adapter, "USE reports");
    run(
        &mut adapter,
        "CREATE TABLE keyed (id INT PRIMARY KEY, name VARCHAR(20), KEY k (name)) COLLATE=utf8mb4_unicode_ci",
    );
    assert_eq!(
        show_create_table(&mut adapter, "keyed"),
        "CREATE TABLE `keyed` (\n  `id` int NOT NULL,\n  `name` varchar(20) COLLATE utf8mb4_unicode_ci DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `k` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"
    );
    run(&mut adapter, "USE app");
    // A character set named alone gives the table that character set's own
    // default, and a copy takes its source's.
    run(
        &mut adapter,
        "CREATE TABLE plain (id INT PRIMARY KEY, name VARCHAR(20)) DEFAULT CHARSET=utf8mb4",
    );
    assert_eq!(show_create_table(&mut adapter, "plain"), PLAIN);
    run(&mut adapter, "CREATE TABLE copy LIKE plain");
    assert_eq!(
        show_create_table(&mut adapter, "copy"),
        PLAIN.replace("`plain`", "`copy`")
    );

    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, TABLE_COLLATION FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'app' ORDER BY 1"
        ),
        [
            row(&["copy", "utf8mb4_0900_ai_ci"]),
            row(&["plain", "utf8mb4_0900_ai_ci"]),
            row(&["posts", "utf8mb4_unicode_ci"]),
            row(&["tags", "utf8mb4_unicode_ci"]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, COLUMN_NAME, COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'app' AND COLLATION_NAME IS NOT NULL ORDER BY 1, ORDINAL_POSITION"
        ),
        [
            row(&["copy", "name", "utf8mb4_0900_ai_ci"]),
            row(&["plain", "name", "utf8mb4_0900_ai_ci"]),
            row(&["posts", "title", "utf8mb4_unicode_ci"]),
            row(&["posts", "body", "utf8mb4_unicode_ci"]),
            row(&["posts", "kind", "utf8mb4_unicode_ci"]),
            row(&["posts", "code", "utf8mb4_bin"]),
            row(&["tags", "name", "utf8mb4_unicode_ci"]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA ORDER BY 1"
        ),
        [
            row(&["app", "utf8mb4", "utf8mb4_unicode_ci"]),
            row(&["reports", "utf8mb4", "utf8mb4_0900_ai_ci"]),
        ]
    );

    // U+2090 weighs the same as `a` under Unicode 9 and has a weight of its
    // own under Unicode 4.0.0, so only the table made under Unicode 9 finds
    // it.
    run(
        &mut adapter,
        "INSERT INTO posts VALUES (1, 'xₐy', NULL, 'a', 'Q', 1)",
    );
    run(&mut adapter, "INSERT INTO plain VALUES (1, 'xₐy')");
    assert!(rows(&mut adapter, "SELECT id FROM posts WHERE title = 'xay'").is_empty());
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM plain WHERE name = 'xay'"),
        [row(&["1"])]
    );

    run(
        &mut adapter,
        "ALTER TABLE posts ADD COLUMN extra VARCHAR(5)",
    );
    assert!(show_create_table(&mut adapter, "posts")
        .contains("  `extra` varchar(5) COLLATE utf8mb4_unicode_ci DEFAULT NULL,\n"));

    // MySQL gives each column of such a table its source column's collation,
    // which is not kept here.
    assert_eq!(
        adapter.execute_query("CREATE TABLE picked AS SELECT id, name FROM plain"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[test]
fn create_database_reads_every_spelling_prisma_laravel_and_mysqldump_write() {
    let (_directory, catalog, _factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session(&catalog);
    for (sql, name) in [
        (
            "CREATE DATABASE `prisma` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            "prisma",
        ),
        (
            "create database `laravel` default character set `utf8mb4` default collate `utf8mb4_unicode_ci`",
            "laravel",
        ),
        (
            "CREATE DATABASE /*!32312 IF NOT EXISTS*/ `dumped` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci */ /*!80016 DEFAULT ENCRYPTION='N' */",
            "dumped",
        ),
        ("CREATE SCHEMA only_collation COLLATE 'UTF8MB4_UNICODE_CI'", "only_collation"),
        (
            "CREATE DATABASE collation_first DEFAULT COLLATE = utf8mb4_unicode_ci CHARSET = 'utf8mb4'",
            "collation_first",
        ),
        (
            "CREATE DATABASE last_wins COLLATE utf8mb4_bin COLLATE utf8mb4_unicode_ci",
            "last_wins",
        ),
    ] {
        run(&mut adapter, sql);
        assert_eq!(
            show_create_database(&mut adapter, name),
            created_as("utf8mb4_unicode_ci").replace("`app`", &format!("`{name}`")),
            "{sql}"
        );
    }
    run(
        &mut adapter,
        "CREATE DATABASE charset_alone CHARSET utf8mb4",
    );
    assert_eq!(
        show_create_database(&mut adapter, "charset_alone"),
        created_as("utf8mb4_0900_ai_ci").replace("`app`", "`charset_alone`")
    );

    for (sql, error) in [
        (
            "CREATE DATABASE e1 COLLATE nope_ci",
            FrontendErrorKind::UnknownCollation,
        ),
        (
            "CREATE DATABASE e2 CHARACTER SET nope",
            FrontendErrorKind::UnknownCharacterSet,
        ),
        (
            "CREATE DATABASE e3 CHARACTER SET latin1 COLLATE utf8mb4_bin",
            FrontendErrorKind::CollationOfAnotherCharacterSet,
        ),
        (
            "CREATE DATABASE e4 COLLATE utf8mb4_bin COLLATE latin1_swedish_ci",
            FrontendErrorKind::CollationOfAnotherCharacterSet,
        ),
        (
            "CREATE DATABASE e5 COLLATE utf8mb4_unicode_ci CHARACTER SET latin1",
            FrontendErrorKind::ConflictingCharacterSets,
        ),
        (
            "CREATE DATABASE e6 CHARACTER SET utf8mb4 CHARACTER SET utf8",
            FrontendErrorKind::ConflictingCharacterSets,
        ),
        // Each is an error before the database is looked for.
        (
            "CREATE DATABASE app COLLATE nope_ci",
            FrontendErrorKind::UnknownCollation,
        ),
        (
            "CREATE DATABASE e7 ENCRYPTION 'Y' COLLATE nope_ci",
            FrontendErrorKind::UnknownCollation,
        ),
        // MySQL takes each of these, and a table here cannot keep what it
        // names.
        (
            "CREATE DATABASE u1 CHARACTER SET latin1",
            FrontendErrorKind::Unsupported,
        ),
        (
            "CREATE DATABASE u2 COLLATE utf8mb4_0900_as_cs",
            FrontendErrorKind::Unsupported,
        ),
        (
            "CREATE DATABASE u3 COLLATE utf8mb4_general_ci",
            FrontendErrorKind::Unsupported,
        ),
        (
            "CREATE DATABASE u4 COLLATE utf8_general_ci",
            FrontendErrorKind::Unsupported,
        ),
        (
            "CREATE DATABASE u5 ENCRYPTION 'Y'",
            FrontendErrorKind::Unsupported,
        ),
        (
            "CREATE DATABASE s1 CHARACTER SET = DEFAULT",
            FrontendErrorKind::Syntax,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(error), "{sql}");
    }
    let listed = rows(&mut adapter, "SHOW DATABASES");
    for name in [
        "e1", "e2", "e3", "e4", "e5", "e6", "e7", "u1", "u2", "u3", "u4", "u5", "s1",
    ] {
        assert!(!listed.contains(&row(&[name])), "{name}: {listed:?}");
    }
}

#[test]
fn show_create_database_prints_the_name_as_it_was_written() {
    let (_directory, catalog, _factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session(&catalog);
    run(
        &mut adapter,
        "CREATE DATABASE app COLLATE utf8mb4_unicode_ci",
    );
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SHOW CREATE SCHEMA IF NOT EXISTS APP")
    else {
        panic!("SHOW CREATE DATABASE must answer a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags
            ))
            .collect::<Vec<_>>(),
        [
            (
                "Database",
                MYSQL_TYPE_VAR_STRING,
                256,
                31,
                MYSQL_NOT_NULL_FLAG
            ),
            (
                "Create Database",
                MYSQL_TYPE_VAR_STRING,
                4096,
                31,
                MYSQL_NOT_NULL_FLAG
            ),
        ]
    );
    assert_eq!(
        result.rows,
        [[
            Some(b"APP".to_vec()),
            Some(
                b"CREATE DATABASE /*!32312 IF NOT EXISTS*/ `APP` /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci */ /*!80016 DEFAULT ENCRYPTION='N' */"
                    .to_vec()
            ),
        ]]
    );
    assert_eq!(
        show_create_database(&mut adapter, "reports"),
        created_as("utf8mb4_0900_ai_ci").replace("`app`", "`reports`")
    );
    assert_eq!(
        adapter.execute_query("SHOW CREATE DATABASE nope"),
        Err(FrontendErrorKind::UnknownDatabase)
    );
}

#[test]
fn alter_database_changes_only_the_tables_made_afterwards() {
    let (_directory, catalog, _factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut first = session(&catalog);
    let mut second = session(&catalog);
    run(&mut first, "CREATE DATABASE app");
    run(&mut first, "USE app");
    run(&mut second, "USE app");
    run(
        &mut first,
        "CREATE TABLE plain (id INT PRIMARY KEY, name VARCHAR(20))",
    );

    run(&mut first, "ALTER DATABASE app COLLATE utf8mb4_unicode_ci");
    assert_eq!(
        show_create_database(&mut first, "app"),
        created_as("utf8mb4_unicode_ci")
    );
    assert_eq!(
        rows(&mut first, "SELECT @@collation_database"),
        [row(&["utf8mb4_unicode_ci"])]
    );
    // The other session reads the collation the database had when it was
    // selected until it selects it again, but a table it makes takes the new
    // one straight away.
    assert_eq!(
        rows(&mut second, "SELECT @@collation_database"),
        [row(&["utf8mb4_0900_ai_ci"])]
    );
    run(&mut second, "CREATE TABLE later (id INT NOT NULL PRIMARY KEY, title VARCHAR(100) NOT NULL, body TEXT, kind ENUM('a','b'), code VARCHAR(5) COLLATE utf8mb4_bin, n INT)");
    assert_eq!(
        show_create_table(&mut second, "later"),
        POSTS.replace("`posts`", "`later`")
    );
    run(&mut second, "USE app");
    assert_eq!(
        rows(&mut second, "SELECT @@collation_database"),
        [row(&["utf8mb4_unicode_ci"])]
    );
    assert_eq!(show_create_table(&mut first, "plain"), PLAIN);

    // A character set named alone takes the database back to its default.
    run(&mut second, "ALTER SCHEMA CHARACTER SET utf8mb4");
    assert_eq!(
        show_create_database(&mut first, "app"),
        created_as("utf8mb4_0900_ai_ci")
    );
    assert_eq!(
        rows(&mut second, "SELECT @@collation_database"),
        [row(&["utf8mb4_0900_ai_ci"])]
    );
    run(
        &mut first,
        "ALTER DATABASE /*!40100 DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci */",
    );
    run(&mut first, "ALTER DATABASE app ENCRYPTION 'N'");
    assert_eq!(
        show_create_database(&mut first, "app"),
        created_as("utf8mb4_unicode_ci")
    );

    for (sql, error) in [
        (
            "ALTER DATABASE nope COLLATE utf8mb4_unicode_ci",
            FrontendErrorKind::NoDatabaseToAlter,
        ),
        ("ALTER DATABASE app", FrontendErrorKind::Syntax),
        (
            "ALTER DATABASE app COLLATE nope_ci",
            FrontendErrorKind::UnknownCollation,
        ),
        (
            "ALTER DATABASE app READ ONLY = 1",
            FrontendErrorKind::Unsupported,
        ),
        (
            "ALTER DATABASE app COLLATE utf8mb4_0900_as_cs",
            FrontendErrorKind::Unsupported,
        ),
    ] {
        assert_eq!(first.execute_query(sql), Err(error), "{sql}");
    }
    let mut unselected = session(&catalog);
    assert_eq!(
        unselected.execute_query("ALTER DATABASE COLLATE utf8mb4_unicode_ci"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    assert_eq!(
        show_create_database(&mut first, "app"),
        created_as("utf8mb4_unicode_ci")
    );
}

/// Gitea gives an empty database the first case-sensitive collation the
/// server lists — `utf8mb4_bin` here, where MySQL lists `utf8mb4_0900_as_cs`
/// first — and every table it makes afterwards takes it, which is what its
/// `TestDatabaseCollation` checks: a unique key tells `main` from `Main`.
/// Measured on MySQL 8.4.11 over the same statements.
#[test]
fn a_utf8mb4_bin_database_makes_the_tables_made_in_it_case_sensitive() {
    let (_directory, catalog, _factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session(&catalog);
    run(&mut adapter, "CREATE DATABASE app");
    run(&mut adapter, "USE app");
    assert_eq!(
        rows(
            &mut adapter,
            "SHOW COLLATION WHERE (Collation = 'utf8mb4_bin') OR (Collation LIKE '%\\_as\\_cs%')"
        )
        .into_iter()
        .map(|row| row[0].clone().unwrap())
        .collect::<Vec<_>>(),
        ["utf8mb4_bin"]
    );
    run(
        &mut adapter,
        "ALTER DATABASE CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT @@collation_database, @@character_set_database"
        ),
        [row(&["utf8mb4_bin", "utf8mb4"])]
    );
    assert_eq!(
        show_create_database(&mut adapter, "app"),
        created_as("utf8mb4_bin")
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = 'app'"
        ),
        [row(&["utf8mb4", "utf8mb4_bin"])]
    );

    run(
        &mut adapter,
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10), body TEXT, kind ENUM('a','b'), code VARCHAR(5) CHARACTER SET utf8mb4, u VARCHAR(5) COLLATE utf8mb4_unicode_ci, UNIQUE KEY uq (name))",
    );
    assert_eq!(
        show_create_table(&mut adapter, "t"),
        "CREATE TABLE `t` (\n  `id` int NOT NULL,\n  `name` varchar(10) COLLATE utf8mb4_bin DEFAULT NULL,\n  `body` text COLLATE utf8mb4_bin,\n  `kind` enum('a','b') COLLATE utf8mb4_bin DEFAULT NULL,\n  `code` varchar(5) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci DEFAULT NULL,\n  `u` varchar(5) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `uq` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin"
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COLUMN_NAME, COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'app' AND TABLE_NAME = 't' AND COLLATION_NAME IS NOT NULL ORDER BY ORDINAL_POSITION"
        ),
        [
            row(&["name", "utf8mb4_bin"]),
            row(&["body", "utf8mb4_bin"]),
            row(&["kind", "utf8mb4_bin"]),
            row(&["code", "utf8mb4_0900_ai_ci"]),
            row(&["u", "utf8mb4_unicode_ci"]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_COLLATION FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'app' AND TABLE_NAME = 't'"
        ),
        [row(&["utf8mb4_bin"])]
    );

    run(&mut adapter, "INSERT INTO t (id, name) VALUES (1, 'main')");
    run(&mut adapter, "INSERT INTO t (id, name) VALUES (2, 'Main')");
    // `utf8mb4_bin` pads with spaces, so a trailing space is no difference.
    for duplicate in ["main", "main "] {
        assert_eq!(
            adapter.execute_query(&format!(
                "INSERT INTO t (id, name) VALUES (3, '{duplicate}')"
            )),
            Err(FrontendErrorKind::ConstraintViolation),
            "{duplicate}"
        );
    }
    run(
        &mut adapter,
        "INSERT INTO t (id, name) VALUES (5, 'B'), (6, 'a'), (7, 'b'), (8, 'A'), (9, 'é'), (10, 'E')",
    );
    let ids = |adapter: &mut Adapter, sql: &str| {
        rows(adapter, sql)
            .into_iter()
            .map(|row| row[0].clone().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&mut adapter, "SELECT id, name FROM t ORDER BY name"),
        ["8", "5", "10", "2", "6", "7", "1", "9"]
    );
    assert!(ids(&mut adapter, "SELECT id FROM t WHERE name = 'MAIN'").is_empty());
    assert_eq!(
        ids(&mut adapter, "SELECT id FROM t WHERE name = 'main '"),
        ["1"]
    );
    // `LIKE` matches each character only to itself, without padding.
    assert_eq!(
        ids(&mut adapter, "SELECT id FROM t WHERE name LIKE 'm%'"),
        ["1"]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM t WHERE name NOT LIKE '%a%' ORDER BY id"
        ),
        ["5", "7", "8", "9", "10"]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM t WHERE LOWER(name) LIKE 'e' ORDER BY id"
        ),
        ["10"]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM t WHERE LOWER(name) LIKE 'm%' ORDER BY id"
        ),
        ["1", "2"]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM t WHERE name IN ('a', 'E') ORDER BY id"
        ),
        ["6", "10"]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM t WHERE name > 'Z' ORDER BY id"
        ),
        ["1", "6", "7", "9"]
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT name, COUNT(*) FROM t GROUP BY name ORDER BY name"
        ),
        ["A", "B", "E", "Main", "a", "b", "main", "é"]
    );

    // What xorm's `Sync` writes for Gitea's test table.
    for sql in [
        "CREATE TABLE IF NOT EXISTS `test_collation_tbl` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `txt` VARCHAR(10) NULL) ROW_FORMAT=DYNAMIC",
        "CREATE UNIQUE INDEX `UQE_test_collation_tbl_txt` ON `test_collation_tbl` (`txt`)",
        "INSERT INTO test_collation_tbl (txt) VALUES ('main')",
        "INSERT INTO test_collation_tbl (txt) VALUES ('Main')",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        adapter.execute_query("INSERT INTO test_collation_tbl (txt) VALUES ('main')"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT count(*) FROM `test_collation_tbl`"),
        [row(&["2"])]
    );

    // A table naming the collation itself takes it the same way, and one
    // naming only the character set takes that character set's default.
    run(
        &mut adapter,
        "CREATE TABLE named_tbl (id INT, n VARCHAR(3)) COLLATE=utf8mb4_bin",
    );
    assert_eq!(
        show_create_table(&mut adapter, "named_tbl"),
        "CREATE TABLE `named_tbl` (\n  `id` int DEFAULT NULL,\n  `n` varchar(3) COLLATE utf8mb4_bin DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin"
    );
    run(
        &mut adapter,
        "CREATE TABLE plain_cs (id INT) CHARSET=utf8mb4",
    );
    assert_eq!(
        show_create_table(&mut adapter, "plain_cs"),
        "CREATE TABLE `plain_cs` (\n  `id` int DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
    );
    run(&mut adapter, "ALTER TABLE t ADD COLUMN extra VARCHAR(4)");
    assert!(show_create_table(&mut adapter, "t")
        .contains("  `extra` varchar(4) COLLATE utf8mb4_bin DEFAULT NULL,\n"));

    // Gitea names the database when it converts one.
    run(
        &mut adapter,
        "ALTER DATABASE `app` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT @@collation_database"),
        [row(&["utf8mb4_0900_ai_ci"])]
    );
    run(&mut adapter, "ALTER DATABASE app COLLATE utf8mb4_bin");
    assert_eq!(
        rows(&mut adapter, "SELECT @@collation_database"),
        [row(&["utf8mb4_bin"])]
    );
}

#[test]
fn a_database_keeps_its_collation_across_a_restart() {
    let (directory, catalog, factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session(&catalog);
    run(
        &mut adapter,
        "CREATE DATABASE app COLLATE utf8mb4_unicode_ci",
    );
    run(&mut adapter, "CREATE DATABASE altered");
    run(
        &mut adapter,
        "ALTER DATABASE altered COLLATE utf8mb4_unicode_ci",
    );
    run(
        &mut adapter,
        "CREATE DATABASE back COLLATE utf8mb4_unicode_ci",
    );
    run(
        &mut adapter,
        "ALTER DATABASE back COLLATE utf8mb4_0900_ai_ci",
    );
    run(
        &mut adapter,
        "CREATE DATABASE binary_words COLLATE utf8mb4_bin",
    );
    drop(adapter);
    drop(factory);
    drop(catalog);

    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    let mut adapter = session(&catalog);
    for (database, collation) in [
        ("app", "utf8mb4_unicode_ci"),
        ("altered", "utf8mb4_unicode_ci"),
        ("back", "utf8mb4_0900_ai_ci"),
        ("binary_words", "utf8mb4_bin"),
        ("reports", "utf8mb4_0900_ai_ci"),
    ] {
        assert_eq!(
            show_create_database(&mut adapter, database),
            created_as(collation).replace("`app`", &format!("`{database}`")),
        );
    }
    run(&mut adapter, "USE altered");
    assert_eq!(
        rows(&mut adapter, "SELECT @@collation_database"),
        [row(&["utf8mb4_unicode_ci"])]
    );
    run(&mut adapter, "CREATE TABLE posts (id INT NOT NULL PRIMARY KEY, title VARCHAR(100) NOT NULL, body TEXT, kind ENUM('a','b'), code VARCHAR(5) COLLATE utf8mb4_bin, n INT)");
    assert_eq!(show_create_table(&mut adapter, "posts"), POSTS);
}

/// A trigger reports the collation its database had when it was made, and
/// keeps reporting it after the database is altered.
#[test]
fn a_trigger_keeps_the_collation_its_database_had() {
    let (directory, catalog, factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session_of(
        &catalog,
        RecordingAuthorizer::with_schema_creator("app_owner"),
    );
    run(
        &mut adapter,
        "CREATE DATABASE app COLLATE utf8mb4_unicode_ci",
    );
    run(&mut adapter, "USE app");
    for sql in [
        "CREATE TABLE posts (id INT PRIMARY KEY, title VARCHAR(20))",
        "CREATE TABLE pages (id INT PRIMARY KEY, title VARCHAR(20))",
        "CREATE TABLE audit (note VARCHAR(20))",
        "CREATE TRIGGER posts_audit AFTER INSERT ON posts FOR EACH ROW BEGIN INSERT INTO audit (note) VALUES (NEW.title); END",
        "ALTER DATABASE app COLLATE utf8mb4_0900_ai_ci",
        "CREATE TRIGGER pages_audit AFTER INSERT ON pages FOR EACH ROW BEGIN INSERT INTO audit (note) VALUES (NEW.title); END",
    ] {
        run(&mut adapter, sql);
    }
    let reported = |adapter: &mut Adapter| {
        let mut triggers = rows(adapter, "SHOW TRIGGERS")
            .into_iter()
            .map(|trigger| (trigger[0].clone().unwrap(), trigger[10].clone().unwrap()))
            .collect::<Vec<_>>();
        triggers.sort();
        triggers
    };
    let expected = [
        ("pages_audit".to_owned(), "utf8mb4_0900_ai_ci".to_owned()),
        ("posts_audit".to_owned(), "utf8mb4_unicode_ci".to_owned()),
    ];
    assert_eq!(reported(&mut adapter), expected);
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TRIGGER posts_audit")[0][5],
        Some("utf8mb4_unicode_ci".to_owned())
    );
    drop(adapter);
    drop(factory);
    drop(catalog);

    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    let mut adapter = session(&catalog);
    run(&mut adapter, "USE app");
    assert_eq!(reported(&mut adapter), expected);
}

/// Gitea lists an organisation's teams with the owners first, `ORDER BY CASE
/// WHEN name LIKE 'Owners' THEN '' ELSE name END`, which in a `utf8mb4_bin`
/// database orders by code point. Measured on MySQL 8.4.11 over the same
/// rows: a `CASE` answers the collation of the columns among its branches and
/// a call over a column the collation of that column, so each orders under
/// `utf8mb4_bin`, with its spaces padded.
#[test]
fn an_ordering_over_words_follows_the_collation_of_its_columns() {
    let (_directory, catalog, _factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session(&catalog);
    run(&mut adapter, "CREATE DATABASE ord COLLATE utf8mb4_bin");
    run(&mut adapter, "USE ord");
    run(
        &mut adapter,
        "CREATE TABLE team (id INT PRIMARY KEY, org_id INT, lower_name VARCHAR(255), name VARCHAR(255), description VARCHAR(255))",
    );
    run(
        &mut adapter,
        "INSERT INTO team VALUES (1, 3, 'owners', 'Owners', 'x'), (2, 3, 'beta', 'beta', 'B'), (3, 3, 'alpha', 'Alpha', 'a'), (4, 3, 'alpha2', 'alpha2', 'é'), (5, 3, 'zeta', 'Zeta', 'E'), (6, 3, 'émile', 'Émile', NULL), (7, 3, 'b ', 'b ', NULL)",
    );
    for (sql, ids) in [
        (
            "SELECT id FROM team WHERE (org_id=3) ORDER BY CASE WHEN name LIKE 'Owners' THEN '' ELSE name END",
            ["1", "3", "5", "4", "7", "2", "6"].as_slice(),
        ),
        (
            "SELECT id FROM team WHERE (org_id=3) ORDER BY CASE WHEN name='Owners' THEN '' ELSE lower_name END",
            &["1", "3", "4", "7", "2", "5", "6"],
        ),
        // The engine changes the case of ASCII letters alone, so `LOWER` and
        // `UPPER` are refused over a word holding any other letter; the order
        // they give is read over the rest.
        (
            "SELECT id FROM team WHERE id <> 6 ORDER BY LOWER(name), id",
            &["3", "4", "7", "2", "1", "5"],
        ),
        (
            "SELECT id FROM team WHERE id <> 6 ORDER BY UPPER(name) DESC, id",
            &["5", "1", "2", "7", "4", "3"],
        ),
        (
            "SELECT id FROM team ORDER BY CONCAT(name, 'x'), id",
            &["3", "1", "5", "4", "7", "2", "6"],
        ),
        (
            "SELECT id FROM team WHERE (lower_name LIKE '%a%' OR LOWER(description) LIKE '%e%') AND org_id=3 ORDER BY CASE WHEN name='Owners' THEN '' ELSE lower_name END",
            &["3", "4", "2", "5"],
        ),
    ] {
        assert_eq!(
            rows(&mut adapter, sql)
                .into_iter()
                .map(|row| row[0].clone().unwrap())
                .collect::<Vec<_>>(),
            ids,
            "{sql}"
        );
    }
    // Gitea's other readings of a team, over a join, and its milestone and
    // LFS lock lookups, which compare a call's answer with a word under the
    // collation of the column the call reads: `LOWER(name) IN ('V1.0')` finds
    // nothing in a `utf8mb4_bin` table.
    for sql in [
        "ALTER TABLE team ADD COLUMN visibility INT DEFAULT 0",
        "CREATE TABLE team_repo (id INT PRIMARY KEY, org_id INT, team_id INT, repo_id INT)",
        "CREATE TABLE team_user (id INT PRIMARY KEY, org_id INT, team_id INT, uid INT)",
        "INSERT INTO team_repo VALUES (1, 3, 1, 1), (2, 3, 2, 1), (3, 3, 3, 1), (4, 3, 5, 1)",
        "INSERT INTO team_user VALUES (1, 3, 1, 1), (2, 3, 4, 1), (3, 3, 7, 1)",
        "CREATE TABLE milestone (id INT PRIMARY KEY, name VARCHAR(255))",
        "INSERT INTO milestone VALUES (1, 'V1.0'), (2, 'v1.0'), (3, 'v2.0'), (5, 'v1.0 ')",
        "CREATE TABLE lfs_lock (id INT PRIMARY KEY, repo_id INT, path TEXT)",
        "INSERT INTO lfs_lock VALUES (1, 1, 'Dir/File.bin'), (2, 1, 'dir/file.bin'), (3, 2, 'dir/file.bin')",
    ] {
        run(&mut adapter, sql);
    }
    for (sql, ids) in [
        (
            "SELECT team.id FROM team INNER JOIN team_repo ON team_repo.team_id = team.id WHERE (team.org_id = 3) AND (team_repo.repo_id=1) ORDER BY CASE WHEN name LIKE 'Owners' THEN '' ELSE name END",
            ["1", "3", "5", "2"].as_slice(),
        ),
        (
            "SELECT team.id FROM team LEFT JOIN team_user ON team_user.team_id = team.id AND team_user.uid = 1 WHERE team.org_id=3 AND (team_user.uid=1 OR team.visibility IN (0)) ORDER BY CASE WHEN name='Owners' THEN '' ELSE lower_name END LIMIT 30",
            &["1", "3", "4", "7", "2", "5", "6"],
        ),
        (
            "SELECT id FROM milestone WHERE LOWER(name) IN ('v1.0') ORDER BY id",
            &["1", "2", "5"],
        ),
        (
            "SELECT id FROM milestone WHERE LOWER(name) IN ('V1.0') ORDER BY id",
            &[],
        ),
        (
            "SELECT id FROM milestone WHERE LOWER(name) IN ('v1.0', 'v2.0') ORDER BY id",
            &["1", "2", "3", "5"],
        ),
        (
            "SELECT id FROM lfs_lock WHERE (lower(path) = 'dir/file.bin') AND repo_id = 1 ORDER BY id",
            &["1", "2"],
        ),
        (
            "SELECT id FROM lfs_lock WHERE (lower(path) = 'Dir/File.bin') AND repo_id = 1 ORDER BY id",
            &[],
        ),
    ] {
        assert_eq!(
            rows(&mut adapter, sql)
                .into_iter()
                .map(|row| row[0].clone().unwrap())
                .collect::<Vec<_>>(),
            ids,
            "{sql}"
        );
    }
    // MySQL takes a `CASE` choosing between columns of two collations, and
    // which order it gives the answer has not been worked out.
    run(
        &mut adapter,
        "CREATE TABLE mixed (id INT PRIMARY KEY, a VARCHAR(5), b VARCHAR(5) COLLATE utf8mb4_0900_ai_ci)",
    );
    assert_eq!(
        adapter.execute_query(
            "SELECT id FROM mixed ORDER BY CASE WHEN id = 1 THEN '' WHEN id = 2 THEN a ELSE b END"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// Gitea converts a database and every table in it with `ALTER DATABASE ...
/// COLLATE c` and `ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4 COLLATE c`.
/// Measured on MySQL 8.4.11 over the same statements: every column of words,
/// `ENUM` or `SET` takes the collation and so does the table, a `JSON` column
/// keeps its own, and a unique key that would hold two equal values is 1062
/// and leaves the table as it was.
#[test]
fn convert_to_gives_the_table_and_every_column_of_words_the_collation() {
    let (_directory, catalog, _factory) = catalog_factory(Arc::new(RecordingAuthorizer::default()));
    let mut adapter = session(&catalog);
    run(&mut adapter, "CREATE DATABASE conv");
    run(&mut adapter, "USE conv");
    run(
        &mut adapter,
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10), body TEXT, kind ENUM('a','b'), js JSON, n INT, UNIQUE KEY uq (name))",
    );
    run(
        &mut adapter,
        "INSERT INTO t (id, name, js) VALUES (1, 'main', '[1]'), (2, 'Main2', NULL)",
    );
    let converted_to = |collation: &str| {
        let columns = if collation == "utf8mb4_0900_ai_ci" {
            String::new()
        } else {
            format!(" COLLATE {collation}")
        };
        format!(
            "CREATE TABLE `t` (\n  `id` int NOT NULL,\n  `name` varchar(10){columns} DEFAULT NULL,\n  `body` text{columns},\n  `kind` enum('a','b'){columns} DEFAULT NULL,\n  `js` json DEFAULT NULL,\n  `n` int DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `uq` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE={collation}"
        )
    };

    run(
        &mut adapter,
        "ALTER TABLE `t` CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
    );
    assert_eq!(
        show_create_table(&mut adapter, "t"),
        converted_to("utf8mb4_bin")
    );
    run(&mut adapter, "INSERT INTO t (id, name) VALUES (3, 'MAIN')");
    assert_eq!(
        adapter.execute_query(
            "ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"
        ),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        show_create_table(&mut adapter, "t"),
        converted_to("utf8mb4_bin")
    );
    run(&mut adapter, "DELETE FROM t WHERE id = 3");
    run(
        &mut adapter,
        "ALTER TABLE t CONVERT TO CHARSET 'UTF8MB4' COLLATE 'UTF8MB4_UNICODE_CI'",
    );
    assert_eq!(
        show_create_table(&mut adapter, "t"),
        converted_to("utf8mb4_unicode_ci")
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO t (id, name) VALUES (4, 'MAIN')"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    // A character set named alone gives its own default, whatever the
    // database's collation is, and `DEFAULT` the database's.
    run(&mut adapter, "ALTER DATABASE conv COLLATE utf8mb4_bin");
    run(
        &mut adapter,
        "ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4",
    );
    assert_eq!(
        show_create_table(&mut adapter, "t"),
        converted_to("utf8mb4_0900_ai_ci")
    );
    run(
        &mut adapter,
        "ALTER TABLE t CONVERT TO CHARACTER SET DEFAULT",
    );
    assert_eq!(
        show_create_table(&mut adapter, "t"),
        converted_to("utf8mb4_bin")
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, name, js FROM t ORDER BY id"),
        [
            vec![
                Some("1".to_owned()),
                Some("main".to_owned()),
                Some("[1]".to_owned())
            ],
            vec![Some("2".to_owned()), Some("Main2".to_owned()), None],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COLUMN_NAME, COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'conv' AND TABLE_NAME = 't' AND COLLATION_NAME IS NOT NULL ORDER BY ORDINAL_POSITION"
        ),
        [
            row(&["name", "utf8mb4_bin"]),
            row(&["body", "utf8mb4_bin"]),
            row(&["kind", "utf8mb4_bin"]),
        ]
    );
    run(&mut adapter, "ALTER TABLE t ADD COLUMN extra VARCHAR(3)");
    assert!(show_create_table(&mut adapter, "t")
        .contains("  `extra` varchar(3) COLLATE utf8mb4_bin DEFAULT NULL,\n"));

    for (sql, error) in [
        (
            "ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4 COLLATE nope_ci",
            FrontendErrorKind::UnknownCollation,
        ),
        (
            "ALTER TABLE t CONVERT TO CHARACTER SET latin1 COLLATE utf8mb4_bin",
            FrontendErrorKind::CollationOfAnotherCharacterSet,
        ),
        (
            "ALTER TABLE nope CONVERT TO CHARACTER SET utf8mb4",
            FrontendErrorKind::MissingObject,
        ),
        // MySQL takes each of these, and a table here cannot keep them.
        (
            "ALTER TABLE t CONVERT TO CHARACTER SET latin1",
            FrontendErrorKind::Unsupported,
        ),
        (
            "ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci",
            FrontendErrorKind::Unsupported,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(error), "{sql}");
    }
    assert_eq!(
        show_create_table(&mut adapter, "t"),
        converted_to("utf8mb4_bin").replace(
            "  `n` int DEFAULT NULL,\n",
            "  `n` int DEFAULT NULL,\n  `extra` varchar(3) COLLATE utf8mb4_bin DEFAULT NULL,\n"
        )
    );
}
