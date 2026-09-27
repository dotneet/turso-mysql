//! A database's own collation, which every table made in it takes when the
//! table names neither a character set nor a collation — what Prisma and
//! Laravel create their database with.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const ACCOUNT: [u8; 32] = [0x63; 32];

fn session(catalog: &Arc<MySqlDatabaseCatalog>) -> Adapter {
    let mut adapter = AuthorizedDatabaseAdapterFactory::new(
        catalog.clone(),
        binary_context(),
        Arc::new(RecordingAuthorizer::default()),
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
            "CREATE DATABASE u2 COLLATE utf8mb4_bin",
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
            "ALTER DATABASE app COLLATE utf8mb4_bin",
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
    drop(adapter);
    drop(factory);
    drop(catalog);

    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    let mut adapter = session(&catalog);
    for (database, collation) in [
        ("app", "utf8mb4_unicode_ci"),
        ("altered", "utf8mb4_unicode_ci"),
        ("back", "utf8mb4_0900_ai_ci"),
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
