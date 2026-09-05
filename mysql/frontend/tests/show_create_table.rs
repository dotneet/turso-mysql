// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Every expected string here was read off the pinned MySQL 8.4.11 golden
//! bytes, so these tests are the compatibility contract, not a description of
//! what the renderer happens to produce.

use std::sync::Arc;

use turso_core::storage::database::DatabaseFile;
use turso_core::{Database, MemoryIO, OpenFlags, OpenOptions, IO};
use turso_mysql::schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext};
use turso_mysql::{MySqlConnection, MySqlDatabaseCatalog, MySqlDialect, MySqlShowCreateTableError};
use turso_mysql_parser::MySqlTableName;

const TRAILER: &str = " DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

#[test]
fn renders_the_supported_types_the_way_mysql_prints_them() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_types (a TINYINT, b SMALLINT, c MEDIUMINT, d INT, e INTEGER, f BIGINT, g TEXT, h BLOB)")
        .unwrap();
    assert_eq!(
        show(&connection, "sc_types"),
        format!(
            "CREATE TABLE `sc_types` (\n  \
             `a` tinyint DEFAULT NULL,\n  \
             `b` smallint DEFAULT NULL,\n  \
             `c` mediumint DEFAULT NULL,\n  \
             `d` int DEFAULT NULL,\n  \
             `e` int DEFAULT NULL,\n  \
             `f` bigint DEFAULT NULL,\n  \
             `g` text,\n  \
             `h` blob\n\
             ) ENGINE=InnoDB{TRAILER}"
        )
    );
    connection.close().unwrap();
}

#[test]
fn not_null_comes_before_default_and_suppresses_the_null_default() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_notnull (a INT NOT NULL, b TEXT NOT NULL, c BIGINT NOT NULL)")
        .unwrap();
    assert_eq!(
        show(&connection, "sc_notnull"),
        format!(
            "CREATE TABLE `sc_notnull` (\n  \
             `a` int NOT NULL,\n  \
             `b` text NOT NULL,\n  \
             `c` bigint NOT NULL\n\
             ) ENGINE=InnoDB{TRAILER}"
        )
    );
    connection.close().unwrap();
}

#[test]
fn default_literals_are_single_quoted_even_when_they_are_numbers() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_default (a INT DEFAULT 0, b INT DEFAULT -1, c BIGINT DEFAULT 9223372036854775807, d INT NOT NULL DEFAULT 42, e TINYINT DEFAULT 1)")
        .unwrap();
    assert_eq!(
        show(&connection, "sc_default"),
        format!(
            "CREATE TABLE `sc_default` (\n  \
             `a` int DEFAULT '0',\n  \
             `b` int DEFAULT '-1',\n  \
             `c` bigint DEFAULT '9223372036854775807',\n  \
             `d` int NOT NULL DEFAULT '42',\n  \
             `e` tinyint DEFAULT '1'\n\
             ) ENGINE=InnoDB{TRAILER}"
        )
    );
    connection.close().unwrap();
}

#[test]
fn a_primary_key_moves_to_its_own_line_without_a_key_name() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_pk (id INT PRIMARY KEY, label TEXT)")
        .unwrap();
    assert_eq!(
        show(&connection, "sc_pk"),
        format!(
            "CREATE TABLE `sc_pk` (\n  \
             `id` int NOT NULL,\n  \
             `label` text,\n  \
             PRIMARY KEY (`id`)\n\
             ) ENGINE=InnoDB{TRAILER}"
        )
    );
    connection.close().unwrap();
}

#[test]
fn a_missing_table_and_a_view_are_told_apart() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_pk (id INT PRIMARY KEY, label TEXT)")
        .unwrap();
    connection
        .execute("CREATE VIEW sc_view AS SELECT id FROM sc_pk")
        .unwrap();
    assert!(matches!(
        connection.show_create_table(&MySqlTableName::parse("sc_missing").unwrap()),
        Err(MySqlShowCreateTableError::MissingTable)
    ));
    assert!(matches!(
        connection.show_create_table(&MySqlTableName::parse("sc_view").unwrap()),
        Err(MySqlShowCreateTableError::NotTable)
    ));
    assert!(matches!(
        connection.show_create_table(&MySqlTableName::parse("sqlite_schema").unwrap()),
        Err(MySqlShowCreateTableError::MissingTable)
    ));
    connection.close().unwrap();
}

#[test]
fn a_table_this_frontend_cannot_describe_fails_closed() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_indexed (id INT NOT NULL, label TEXT)")
        .unwrap();
    connection
        .execute("CREATE INDEX sc_indexed_label ON sc_indexed (label)")
        .unwrap();
    assert!(matches!(
        connection.show_create_table(&MySqlTableName::parse("sc_indexed").unwrap()),
        Err(MySqlShowCreateTableError::Unsupported)
    ));
    connection.close().unwrap();
}

#[test]
fn a_unique_column_moves_to_its_own_line_after_the_primary_key() {
    let connection = connection();
    connection
        .execute(
            "CREATE TABLE sc_uniq (id INT NOT NULL PRIMARY KEY, label TEXT, code BIGINT UNIQUE)",
        )
        .unwrap();
    assert_eq!(
        show(&connection, "sc_uniq"),
        format!(
            "CREATE TABLE `sc_uniq` (\n  \
             `id` int NOT NULL,\n  \
             `label` text,\n  \
             `code` bigint DEFAULT NULL,\n  \
             PRIMARY KEY (`id`),\n  \
             UNIQUE KEY `code` (`code`)\n\
             ) ENGINE=InnoDB{TRAILER}"
        )
    );
    connection.close().unwrap();
}

#[test]
fn a_constraint_that_cannot_be_printed_is_refused_rather_than_dropped() {
    for ddl in [
        "CREATE TABLE sc_c (a INT, CHECK (a > 0))",
        "CREATE TABLE sc_c (a INT CHECK (a > 0))",
        "CREATE TABLE sc_c (a INT, FOREIGN KEY (a) REFERENCES sc_target (id))",
    ] {
        let connection = connection();
        connection
            .execute("CREATE TABLE sc_target (id INT NOT NULL PRIMARY KEY)")
            .unwrap();
        connection.execute(ddl).unwrap();
        assert!(
            matches!(
                connection.show_create_table(&MySqlTableName::parse("sc_c").unwrap()),
                Err(MySqlShowCreateTableError::Unsupported)
            ),
            "{ddl}"
        );
        connection.close().unwrap();
    }
}

#[test]
fn a_string_default_on_an_integer_column_is_refused() {
    // MySQL never lets a string default onto these columns, so there is no
    // golden to copy. Printing one would also have to escape quotes and
    // newlines the way MySQL does.
    for ddl in [
        "CREATE TABLE sc_s (a INT DEFAULT 'x')",
        "CREATE TABLE sc_s (a INT DEFAULT 'it''s')",
        "CREATE TABLE sc_s (a INT DEFAULT 'one\ntwo')",
    ] {
        let connection = connection();
        connection.execute(ddl).unwrap();
        assert!(
            matches!(
                connection.show_create_table(&MySqlTableName::parse("sc_s").unwrap()),
                Err(MySqlShowCreateTableError::Unsupported)
            ),
            "{ddl}"
        );
        connection.close().unwrap();
    }
}

#[test]
fn a_boolean_default_prints_as_the_number_mysql_stores() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_b (yes INT DEFAULT TRUE, no INT DEFAULT FALSE)")
        .unwrap();
    assert_eq!(
        show(&connection, "sc_b"),
        format!(
            "CREATE TABLE `sc_b` (\n  \
             `yes` int DEFAULT '1',\n  \
             `no` int DEFAULT '0'\n\
             ) ENGINE=InnoDB{TRAILER}"
        )
    );
    connection.close().unwrap();
}

#[test]
fn no_internal_schema_marker_reaches_the_output() {
    let connection = connection();
    connection
        .execute("CREATE TABLE sc_marker (id INT NOT NULL PRIMARY KEY, label TEXT)")
        .unwrap();
    let ddl = show(&connection, "sc_marker");
    assert!(!ddl.contains("/*@turso:mysql-schema:"), "{ddl}");
    assert!(!ddl.contains("*/"), "{ddl}");
    assert!(matches!(
        connection.show_create_table(&MySqlTableName::parse("__turso_internal_seq_x").unwrap()),
        Err(MySqlShowCreateTableError::MissingTable)
    ));
    connection.close().unwrap();
}

#[cfg(unix)]
#[test]
fn an_auto_increment_table_prints_the_counter_only_once_it_has_moved() {
    // AUTO_INCREMENT needs a durable database identity, so this one goes
    // through a catalog rather than the in-memory connection above.
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        directory.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    catalog.create("reports").unwrap();
    let mut session = catalog.new_session(session_context());
    session.select_database("reports").unwrap();
    let connection = session.connection().unwrap().clone();
    connection
        .execute("CREATE TABLE sc_ai (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)")
        .unwrap();

    let body = concat!(
        "CREATE TABLE `sc_ai` (\n",
        "  `id` int NOT NULL AUTO_INCREMENT,\n",
        "  `label` text,\n",
        "  PRIMARY KEY (`id`)\n",
        ") ENGINE=InnoDB"
    );
    // MySQL leaves the counter out while it still stands at one.
    assert_eq!(show(&connection, "sc_ai"), format!("{body}{TRAILER}"));

    connection
        .execute("INSERT INTO sc_ai (label) VALUES ('a'), ('b'), ('c')")
        .unwrap();
    let after_three = format!("{body} AUTO_INCREMENT=4{TRAILER}");
    assert_eq!(show(&connection, "sc_ai"), after_three);
    // Reading the counter must not move it.
    assert_eq!(show(&connection, "sc_ai"), after_three);

    connection
        .execute("INSERT INTO sc_ai (label) VALUES ('d')")
        .unwrap();
    assert_eq!(
        show(&connection, "sc_ai"),
        format!("{body} AUTO_INCREMENT=5{TRAILER}")
    );

    // Deleting every row leaves the counter where it is, as InnoDB does.
    connection.execute("DELETE FROM sc_ai").unwrap();
    assert_eq!(
        show(&connection, "sc_ai"),
        format!("{body} AUTO_INCREMENT=5{TRAILER}")
    );

    // A table in the same database with no auto-increment column gets no
    // counter, however many rows it holds. Every other test here runs without
    // an allocator, so this is the only one that reaches the lookup.
    connection
        .execute("CREATE TABLE sc_plain (id INT NOT NULL PRIMARY KEY, label TEXT)")
        .unwrap();
    connection
        .execute("INSERT INTO sc_plain (id, label) VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    assert_eq!(
        show(&connection, "sc_plain"),
        concat!(
            "CREATE TABLE `sc_plain` (\n",
            "  `id` int NOT NULL,\n",
            "  `label` text,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // Two auto-increment tables keep separate counters.
    connection
        .execute("CREATE TABLE sc_ai2 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)")
        .unwrap();
    connection
        .execute("INSERT INTO sc_ai2 (label) VALUES ('x')")
        .unwrap();
    assert!(show(&connection, "sc_ai2").contains(" AUTO_INCREMENT=2 "));
    assert!(show(&connection, "sc_ai").contains(" AUTO_INCREMENT=5 "));
}

fn show(connection: &MySqlConnection, table: &str) -> String {
    let result = connection
        .show_create_table(&MySqlTableName::parse(table).unwrap())
        .unwrap();
    assert_eq!(result.table(), table);
    result.create_statement().to_owned()
}

fn connection() -> MySqlConnection {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "show-create-table.db";
    let file = io.open_file(path, OpenFlags::Create, true).unwrap();
    let database = Database::open(
        io,
        path,
        OpenOptions::new(Arc::new(MySqlDialect))
            .storage(Arc::new(DatabaseFile::new(file)))
            .flags(OpenFlags::Create),
    )
    .unwrap();
    MySqlConnection::new(database.connect().unwrap(), session_context()).unwrap()
}

fn session_context() -> SchemaSqlSessionContext {
    SchemaSqlSessionContext {
        sql_mode: SchemaSqlMode {
            ansi_quotes: false,
            no_backslash_escapes: false,
        },
        character_set_client: CharacterSet::Binary,
        collation_connection: Collation::Binary,
        default_character_set: CharacterSet::Binary,
        default_collation: Collation::Binary,
    }
}
