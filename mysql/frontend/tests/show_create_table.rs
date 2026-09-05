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

const TRAILER: &str = " ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

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
             ){TRAILER}"
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
             ){TRAILER}"
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
             ){TRAILER}"
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
             ){TRAILER}"
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
             ){TRAILER}"
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
             ){TRAILER}"
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
fn an_auto_increment_column_keeps_its_keyword_but_not_the_table_counter() {
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

    let expected = format!(
        "CREATE TABLE `sc_ai` (\n  \
         `id` int NOT NULL AUTO_INCREMENT,\n  \
         `label` text,\n  \
         PRIMARY KEY (`id`)\n\
         ){TRAILER}"
    );
    assert_eq!(show(&connection, "sc_ai"), expected);

    // Rows move the counter. MySQL would start printing AUTO_INCREMENT=<n>
    // here; Turso reserves values in ranges, so it prints nothing instead of
    // a number it cannot stand behind.
    connection
        .execute("INSERT INTO sc_ai (label) VALUES ('a'), ('b'), ('c')")
        .unwrap();
    assert_eq!(show(&connection, "sc_ai"), expected);
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
