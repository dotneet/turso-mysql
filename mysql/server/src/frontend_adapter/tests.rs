//! Tests for the frontend adapter.
//!
//! They live apart from the code they exercise only because there are so
//! many of them: the adapter is where every checked statement's wire
//! metadata is finally decided, so nearly every measured MySQL shape is
//! pinned here.

#[cfg(unix)]
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

#[cfg(unix)]
use super::catalog_results::{
    database_list_column, information_schema_columns_columns, information_schema_schemata_column,
    show_column_default_value, show_column_extra, show_columns_columns, show_tables_column,
};
use super::*;
#[cfg(unix)]
use crate::AccountId;
use crate::{
    dispatch_command_frame, AuthenticationResponse, ClassicConnection,
    ClientHandshakeResponseConfig, ConnectionState, InitialAuthenticationResult,
    InitialHandshakeSettings, PacketCodec, TextRowValue, TransportSecurity,
    CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_CONNECT_WITH_DB, CLIENT_FOUND_ROWS, COMMAND_SEQUENCE_ID,
    COM_INIT_DB, COM_QUERY, DEFAULT_UTF8MB4_COLLATION,
    REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
};
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use turso_core::{
    storage::database::DatabaseFile, Database, DatabaseOpts, MemoryIO, OpenFlags, OpenOptions, IO,
};
use turso_mysql::{
    schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext},
    MySqlDialect,
};
#[cfg(unix)]
use turso_mysql::{MySqlDatabaseCatalog, MySqlPreparedStatementAuthority};
#[cfg(unix)]
use turso_mysql_parser::MySqlTableName;

fn binary_context() -> SchemaSqlSessionContext {
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

#[test]
fn varchar_columns_answer_what_mysql_8_4_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([9; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE v (id INT NOT NULL PRIMARY KEY, name VARCHAR(4) NOT NULL, note VARCHAR(10), tag CHAR(2), ratio DOUBLE, live BOOLEAN, seen DATETIME)",
        )
        .unwrap();

    // Measured on MySQL 8.4.11: the length counts characters, so four
    // multi-byte characters fit a VARCHAR(4) and five do not.
    adapter
        .execute_query("INSERT INTO v (id, name) VALUES (1, 'abcd')")
        .unwrap();
    adapter
        .execute_query("INSERT INTO v (id, name) VALUES (3, 'あいうえ')")
        .unwrap();
    for sql in [
        "INSERT INTO v (id, name) VALUES (2, 'abcde')",
        "INSERT INTO v (id, name) VALUES (4, 'あいうえお')",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::DataTooLong),
            "{sql}"
        );
    }

    // A CHAR column is held to its length the same way.
    assert_eq!(
        adapter.execute_query("INSERT INTO v (id, name, tag) VALUES (5, 'ab', 'xyz')"),
        Err(FrontendErrorKind::DataTooLong)
    );

    // A DOUBLE keeps its value: MySQL's DOUBLE and the engine's REAL are
    // both IEEE 754 binary64. A fractional value that meets an integer
    // column is refused with 1366 instead; MySQL rounds it away from zero,
    // measured, which a validator cannot do after the record is built.
    adapter
        .execute_query("INSERT INTO v (id, name, ratio) VALUES (6, 'x', 1.5)")
        .unwrap();
    assert_eq!(
        adapter.execute_query("INSERT INTO v (id, name, ratio) VALUES (1.5, 'y', 2.5)"),
        Err(FrontendErrorKind::IncorrectValue)
    );
    let CommandExecutionResult::ResultSet(ratio) = adapter
        .execute_query("SELECT ratio FROM v WHERE id = 6")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        String::from_utf8(ratio.rows[0][0].clone().unwrap()).unwrap(),
        "1.5"
    );

    // A BOOLEAN is a TINYINT, so it takes a TINYINT's range and refuses a
    // value outside it.
    adapter
        .execute_query("INSERT INTO v (id, name, live) VALUES (7, 'z', 1)")
        .unwrap();
    assert!(adapter
        .execute_query("INSERT INTO v (id, name, live) VALUES (8, 'w', 999)")
        .is_err());

    // A DATETIME keeps the text it was given, and the calendar is checked:
    // measured on MySQL 8.4.11, February the thirtieth is 1292 there too.
    adapter
        .execute_query("INSERT INTO v (id, name, seen) VALUES (9, 'q', '2026-09-06 01:02:03')")
        .unwrap();
    for sql in [
        "INSERT INTO v (id, name, seen) VALUES (10, 'r', '2026-02-30 00:00:00')",
        "INSERT INTO v (id, name, seen) VALUES (11, 's', 'not a date')",
        "INSERT INTO v (id, name, seen) VALUES (12, 't', '2026-9-6 1:2:3')",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::IncorrectTemporalValue),
            "{sql}"
        );
    }
    let CommandExecutionResult::ResultSet(seen) = adapter
        .execute_query("SELECT seen FROM v WHERE id = 9")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        String::from_utf8(seen.rows[0][0].clone().unwrap()).unwrap(),
        "2026-09-06 01:02:03"
    );

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE v").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `v` (\n",
            "  `id` int NOT NULL,\n",
            "  `name` varchar(4) NOT NULL,\n",
            "  `note` varchar(10) DEFAULT NULL,\n",
            "  `tag` char(2) DEFAULT NULL,\n",
            "  `ratio` double DEFAULT NULL,\n",
            "  `live` tinyint(1) DEFAULT NULL,\n",
            "  `seen` datetime DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM v").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        columns
            .rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        vec![
            "int",
            "varchar(4)",
            "varchar(10)",
            "char(2)",
            "double",
            "tinyint(1)",
            "datetime"
        ]
    );

    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT id, name, note, tag, ratio, live, seen FROM v")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    // Measured: MySQL reports the declared character count times four, the
    // bytes utf8mb4 reserves for one character.
    assert_eq!(
        selected
            .columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.character_set
            ))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_LONG, 11, MYSQL_BINARY_COLLATION),
            (
                MYSQL_TYPE_VAR_STRING,
                16,
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            ),
            (
                MYSQL_TYPE_VAR_STRING,
                40,
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            ),
            // Measured: a CHAR column reports 254 and carries the same
            // text collation and length rule as a VARCHAR one.
            (MYSQL_TYPE_STRING, 8, u16::from(DEFAULT_UTF8MB4_COLLATION)),
            (MYSQL_TYPE_DOUBLE, 22, MYSQL_BINARY_COLLATION),
            // Measured: a BOOLEAN reports the TINYINT type with the display
            // width from `tinyint(1)`, where a plain TINYINT reports 4.
            (MYSQL_TYPE_TINY, 1, MYSQL_BINARY_COLLATION),
            // Measured: a DATETIME reports the width of its text form and
            // the binary flag, because it carries no collation.
            (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_COLLATION),
        ]
    );
    assert_eq!(
        selected.columns[6].flags & MYSQL_BINARY_FLAG,
        MYSQL_BINARY_FLAG
    );
    // Measured: a DOUBLE column reports 31 decimals, meaning not fixed.
    assert_eq!(selected.columns[4].decimals, NOT_FIXED_DECIMALS);
    assert_eq!(
        selected
            .rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        vec!["abcd", "あいうえ", "x", "z", "q"]
    );
}

#[test]
fn secondary_indexes_reach_the_catalog_the_way_mysql_8_4_reports_them() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([12; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE k (id INT NOT NULL PRIMARY KEY, a VARCHAR(8), b VARCHAR(8))",
        "CREATE INDEX idx_a ON k (a)",
        "CREATE INDEX idx_ab ON k (a, b)",
        "CREATE UNIQUE INDEX uq_b ON k (b)",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    // Byte for byte what MySQL 8.4.11 prints for the same table: the
    // primary key, then the unique keys, then the plain ones, each group in
    // creation order, and a multi-column key with no space after the comma.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE k").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `k` (\n",
            "  `id` int NOT NULL,\n",
            "  `a` varchar(8) DEFAULT NULL,\n",
            "  `b` varchar(8) DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`),\n",
            "  UNIQUE KEY `uq_b` (`b`),\n",
            "  KEY `idx_a` (`a`),\n",
            "  KEY `idx_ab` (`a`,`b`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // Measured: only a leading column carries a key, `UNI` when a
    // single-column unique index makes that column unique and `MUL`
    // otherwise. `b` leads `uq_b` and also sits second in `idx_ab`, and
    // MySQL reports the stronger of the two.
    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM k").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        columns
            .rows
            .iter()
            .map(|row| (
                String::from_utf8(row[0].clone().unwrap()).unwrap(),
                String::from_utf8(row[3].clone().unwrap()).unwrap(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("id".to_owned(), "PRI".to_owned()),
            ("a".to_owned(), "MUL".to_owned()),
            ("b".to_owned(), "UNI".to_owned()),
        ]
    );
}

#[test]
fn an_inline_key_creates_its_index_or_no_table_at_all() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([13; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE k (id INT NOT NULL PRIMARY KEY, a VARCHAR(8), b VARCHAR(8), KEY idx_a (a), KEY idx_ab (a, b))",
        )
        .unwrap();

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE k").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `k` (\n",
            "  `id` int NOT NULL,\n",
            "  `a` varchar(8) DEFAULT NULL,\n",
            "  `b` varchar(8) DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`),\n",
            "  KEY `idx_a` (`a`),\n",
            "  KEY `idx_ab` (`a`,`b`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // `b` only follows `a` in idx_ab, so it carries no key, which is what
    // MySQL 8.4.11 reports for a column that leads nothing.
    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM k").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        columns
            .rows
            .iter()
            .map(|row| String::from_utf8(row[3].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        vec!["PRI".to_owned(), "MUL".to_owned(), String::new()]
    );

    // The statement applies whole or not at all: a key naming a column the
    // table does not have leaves no table behind.
    assert!(adapter
        .execute_query("CREATE TABLE bad (id INT NOT NULL PRIMARY KEY, KEY idx_z (zz))")
        .is_err());
    let CommandExecutionResult::ResultSet(tables) = adapter.execute_query("SHOW TABLES").unwrap()
    else {
        panic!("SHOW TABLES must return a result set");
    };
    assert!(
        !tables.rows.iter().any(|row| row[0]
            .as_ref()
            .is_some_and(|name| name.as_slice() == b"bad")),
        "the failed CREATE TABLE left a table behind"
    );
}

#[test]
fn an_update_or_delete_can_name_the_rows_it_touches() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([14; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE u (id INT NOT NULL PRIMARY KEY, name TEXT, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO u (id, name, n) VALUES (1, 'a', 10), (2, 'b', 20)")
        .unwrap();

    let affected = |adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
                    sql: &str| match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(
        affected(&mut adapter, "UPDATE u SET name = 'z' WHERE id = 1"),
        1
    );
    assert_eq!(affected(&mut adapter, "UPDATE u SET n = 5 WHERE n > 15"), 1);
    assert_eq!(affected(&mut adapter, "DELETE FROM u WHERE id = 2"), 1);

    let CommandExecutionResult::ResultSet(left) =
        adapter.execute_query("SELECT id, name FROM u").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        left.rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        vec!["z".to_owned()]
    );

    // A text comparison runs here for the same reason it runs in a SELECT,
    // and ignores case the same way — see the collation note in COMPAT.md.
    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query("DELETE FROM u WHERE name = 'Z'")
        .unwrap()
    else {
        panic!("DELETE must report affected rows");
    };
    assert_eq!(deleted.affected_rows, 1);

    // A string still cannot meet an integer column, which MySQL answers by
    // coercing the string.
    assert_eq!(
        adapter.execute_query("DELETE FROM u WHERE id = 'z'"),
        Err(FrontendErrorKind::Unsupported)
    );

    // BETWEEN checks both bounds against the column's type.
    adapter
        .execute_query("INSERT INTO u (id, name, n) VALUES (3, 'c', 30), (4, 'd', 40)")
        .unwrap();
    let CommandExecutionResult::ResultSet(between_res) = adapter
        .execute_query("SELECT id FROM u WHERE n BETWEEN 25 AND 35")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(between_res.rows, vec![vec![Some(b"3".to_vec())]]);

    assert_eq!(
        affected(
            &mut adapter,
            "UPDATE u SET name = 'updated' WHERE n BETWEEN 35 AND 45"
        ),
        1
    );
    assert_eq!(
        affected(&mut adapter, "DELETE FROM u WHERE n BETWEEN 25 AND 35"),
        1
    );
    assert_eq!(
        adapter.execute_query("SELECT id FROM u WHERE n BETWEEN 'a' AND 'z'"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[test]
fn count_answers_what_mysql_8_4_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([15; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE c (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO c (id, n) VALUES (1, 10), (2, NULL), (3, 30)")
        .unwrap();

    // Measured on MySQL 8.4.11: LONGLONG, binary collation, length 21,
    // NOT_NULL and BINARY set, no decimals — whatever is counted. The column
    // is named after the call as written, case included and unquoted, and an
    // alias replaces that name. `COUNT(col)` skips NULLs.
    for (sql, name, count) in [
        ("SELECT COUNT(*) FROM c", "COUNT(*)", "3"),
        ("SELECT COUNT(n) FROM c", "COUNT(n)", "2"),
        ("SELECT count(*) FROM c", "count(*)", "3"),
        ("SELECT COUNT(*) AS total FROM c", "total", "3"),
        ("SELECT COUNT(*) FROM c WHERE id = 1", "COUNT(*)", "1"),
    ] {
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query(sql).unwrap_or_else(|error| {
                panic!("{sql}: {error:?}");
            })
        else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(result.columns[0].name, name, "{sql}");
        assert_eq!(
            (
                result.columns[0].column_type,
                result.columns[0].column_length,
                result.columns[0].flags,
                result.columns[0].decimals,
            ),
            (
                MYSQL_TYPE_LONGLONG,
                21,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
                0
            ),
            "{sql}"
        );
        assert_eq!(
            String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap(),
            count,
            "{sql}"
        );
    }

    // Refused: DISTINCT has its own meaning, and an expression argument
    // has no type this can work out.
    for sql in [
        "SELECT COUNT(DISTINCT n) FROM c",
        "SELECT SUM(DISTINCT n) FROM c",
        "SELECT SUM(n + 1) FROM c",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

#[test]
fn decimal_columns_report_what_mysql_8_4_reports() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([17; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE d (id INT NOT NULL PRIMARY KEY, a DECIMAL(10,2), b DECIMAL(5,0), c DECIMAL)",
        )
        .unwrap();
    adapter
        .execute_query("INSERT INTO d (id, a, b, c) VALUES (1, 1.5, 7, 9)")
        .unwrap();

    // A bare DECIMAL means DECIMAL(10,0), which is what MySQL prints for it.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE d").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `d` (\n",
            "  `id` int NOT NULL,\n",
            "  `a` decimal(10,2) DEFAULT NULL,\n",
            "  `b` decimal(5,0) DEFAULT NULL,\n",
            "  `c` decimal(10,0) DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM d").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        columns
            .rows
            .iter()
            .skip(1)
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        vec!["decimal(10,2)", "decimal(5,0)", "decimal(10,0)"]
    );

    // Measured on MySQL 8.4.11: NEWDECIMAL, and a length of the precision
    // plus one for the sign plus one more for the point when the scale is
    // above zero.
    let CommandExecutionResult::ResultSet(selected) =
        adapter.execute_query("SELECT a, b, c FROM d").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        selected
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.decimals))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_NEWDECIMAL, 12, 2),
            (MYSQL_TYPE_NEWDECIMAL, 6, 0),
            (MYSQL_TYPE_NEWDECIMAL, 11, 0),
        ]
    );
    // MySQL renders a DECIMAL at the scale the column declared, so the
    // value it holds as 1.5 reads back as `1.50`.
    assert_eq!(
        String::from_utf8(selected.rows[0][0].clone().unwrap()).unwrap(),
        "1.50"
    );
}

#[test]
fn timestamp_reads_back_the_moment_it_was_given() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([18; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE ts (id INT NOT NULL PRIMARY KEY, dt DATETIME, t TIMESTAMP NULL)",
        )
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO ts (id, dt, t) VALUES (1, '2026-09-06 01:02:03', '2026-09-06 01:02:03')",
        )
        .unwrap();
    // The calendar check is the same one a DATETIME gets.
    assert_eq!(
        adapter.execute_query("INSERT INTO ts (id, t) VALUES (2, '2026-02-30 00:00:00')"),
        Err(FrontendErrorKind::IncorrectTemporalValue)
    );

    // Measured on MySQL 8.4.11: a nullable TIMESTAMP prints its NULL where a
    // nullable DATETIME prints only the DEFAULT.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE ts").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `ts` (\n",
            "  `id` int NOT NULL,\n",
            "  `dt` datetime DEFAULT NULL,\n",
            "  `t` timestamp NULL DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM ts").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        columns
            .rows
            .iter()
            .skip(1)
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        vec!["datetime", "timestamp"]
    );

    // Measured: TIMESTAMP reports type 7 where DATETIME reports 12, both
    // with the width of the text form and the binary flag.
    let CommandExecutionResult::ResultSet(selected) =
        adapter.execute_query("SELECT dt, t FROM ts").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        selected
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length))
            .collect::<Vec<_>>(),
        vec![(MYSQL_TYPE_DATETIME, 19), (MYSQL_TYPE_TIMESTAMP, 19)]
    );
    assert_eq!(
        String::from_utf8(selected.rows[0][1].clone().unwrap()).unwrap(),
        "2026-09-06 01:02:03"
    );
}

/// SHOW WARNINGS reports what the last statement raised, which for this
/// server is the note a DROP TABLE IF EXISTS leaves when the table is not
/// there. Its metadata is measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn show_warnings_reports_the_note_the_last_statement_left() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([29; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();

    // Nothing has warned yet, which MySQL answers with the columns and no
    // row rather than an error.
    let CommandExecutionResult::ResultSet(empty) = adapter.execute_query("SHOW WARNINGS").unwrap()
    else {
        panic!("SHOW WARNINGS must return a result set");
    };
    assert!(empty.rows.is_empty());
    assert_eq!(
        empty
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            ("Level", MYSQL_TYPE_VAR_STRING, 28, MYSQL_NOT_NULL_FLAG),
            (
                "Code",
                MYSQL_TYPE_LONG,
                5,
                MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            ("Message", MYSQL_TYPE_VAR_STRING, 2048, MYSQL_NOT_NULL_FLAG),
        ]
    );

    // Measured: Note 1051, naming the table with its database.
    let CommandExecutionResult::Ok(dropped) = adapter
        .execute_query("DROP TABLE IF EXISTS nosuchtable")
        .unwrap()
    else {
        panic!("DROP TABLE must report an OK");
    };
    assert_eq!(dropped.warnings, 1);
    let CommandExecutionResult::ResultSet(noted) = adapter.execute_query("SHOW WARNINGS").unwrap()
    else {
        panic!("SHOW WARNINGS must return a result set");
    };
    assert_eq!(
        noted.rows,
        vec![vec![
            Some(b"Note".to_vec()),
            Some(b"1051".to_vec()),
            Some(b"Unknown table 'reports.nosuchtable'".to_vec()),
        ]]
    );

    // Reading them does not clear them, and the next statement does.
    let CommandExecutionResult::ResultSet(again) = adapter.execute_query("SHOW WARNINGS").unwrap()
    else {
        panic!("SHOW WARNINGS must return a result set");
    };
    assert_eq!(again.rows.len(), 1);

    // A LIMIT restricts the reported rows without clearing them.
    let CommandExecutionResult::ResultSet(limited) =
        adapter.execute_query("SHOW WARNINGS LIMIT 1").unwrap()
    else {
        panic!("SHOW WARNINGS LIMIT must return a result set");
    };
    assert_eq!(limited.rows.len(), 1);

    let CommandExecutionResult::ResultSet(zero) =
        adapter.execute_query("SHOW WARNINGS LIMIT 0").unwrap()
    else {
        panic!("SHOW WARNINGS LIMIT 0 must return a result set");
    };
    assert!(zero.rows.is_empty());

    let CommandExecutionResult::ResultSet(offset_past) =
        adapter.execute_query("SHOW WARNINGS LIMIT 1, 1").unwrap()
    else {
        panic!("SHOW WARNINGS LIMIT 1, 1 must return a result set");
    };
    assert!(offset_past.rows.is_empty());

    // SHOW ERRORS shares the columns and reports only errors, so it is empty
    // when the last statement only raised a Note.
    let CommandExecutionResult::ResultSet(errors) = adapter.execute_query("SHOW ERRORS").unwrap()
    else {
        panic!("SHOW ERRORS must return a result set");
    };
    assert!(errors.rows.is_empty());
    assert_eq!(errors.columns.len(), 3);
    assert_eq!(errors.columns[0].name, "Level");
    assert_eq!(errors.columns[1].name, "Code");
    assert_eq!(errors.columns[2].name, "Message");

    let CommandExecutionResult::ResultSet(limited_errors) =
        adapter.execute_query("SHOW ERRORS LIMIT 1").unwrap()
    else {
        panic!("SHOW ERRORS LIMIT must return a result set");
    };
    assert!(limited_errors.rows.is_empty());
    adapter
        .execute_query("CREATE TABLE w (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    let CommandExecutionResult::ResultSet(cleared) =
        adapter.execute_query("SHOW WARNINGS").unwrap()
    else {
        panic!("SHOW WARNINGS must return a result set");
    };
    assert!(cleared.rows.is_empty());
}

/// REPLACE deletes the rows a unique key collides with and inserts, which
/// is what the engine's own OR REPLACE does. The rows agree with MySQL;
/// the affected count does not.
#[cfg(unix)]
#[test]
fn replace_into_overwrites_the_row_it_collides_with() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([28; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE r (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();

    // Measured on MySQL 8.4.11: a new row counts 1, a replaced one counts
    // 2 because it is a delete and an insert, and the mixed statement
    // counts 3. The engine does not count the delete, so this counts the
    // inserts alone.
    for (sql, affected) in [
        ("REPLACE INTO r (id, n) VALUES (1, 10)", 1),
        ("REPLACE INTO r (id, n) VALUES (1, 20)", 1),
        ("REPLACE INTO r (id, n) VALUES (2, 30), (1, 40)", 2),
    ] {
        let CommandExecutionResult::Ok(result) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must report affected rows");
        };
        assert_eq!(result.affected_rows, affected, "{sql}");
    }

    // The rows themselves are the ones MySQL leaves behind.
    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id, n FROM r ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rows.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"40".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"30".to_vec())],
        ]
    );
}

/// The scalar calls a client writes most, each answering the shape MySQL
/// answers — measured on 8.4.11.
#[cfg(unix)]
#[test]
fn scalar_calls_answer_the_shape_mysql_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([32; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE s (id INT NOT NULL PRIMARY KEY, v VARCHAR(8), n INT, p VARCHAR(8))",
        )
        .unwrap();
    adapter
        .execute_query("INSERT INTO s (id, v, n, p) VALUES (1, 'aBc', -7, 'xxaBxx')")
        .unwrap();
    // Measured over a VARCHAR(8), which reports length 32: LOWER and
    // UPPER answer a VAR_STRING of that same 32, LENGTH and CHAR_LENGTH a
    // LONGLONG of length 10, and NOW() a NOT NULL DATETIME of 19.
    for (sql, name, column_type, length, flags) in [
        (
            "SELECT LOWER(v) FROM s",
            "LOWER(v)",
            MYSQL_TYPE_VAR_STRING,
            32,
            0,
        ),
        (
            "SELECT UPPER(v) FROM s",
            "UPPER(v)",
            MYSQL_TYPE_VAR_STRING,
            32,
            0,
        ),
        // Measured on MySQL 8.4.11: REPLACE keeps the column's own VAR_STRING width 32.
        (
            "SELECT REPLACE(v, 'B', 'x') FROM s",
            "REPLACE(v, 'B', 'x')",
            MYSQL_TYPE_VAR_STRING,
            32,
            0,
        ),
        // Measured on MySQL 8.4.11: REVERSE keeps the column's own VAR_STRING width 32.
        (
            "SELECT REVERSE(v) FROM s",
            "REVERSE(v)",
            MYSQL_TYPE_VAR_STRING,
            32,
            0,
        ),
        // Measured on MySQL 8.4.11: REPEAT is count * character_length * 4 = 3 * 8 * 4 = 96.
        (
            "SELECT REPEAT(v, 3) FROM s",
            "REPEAT(v, 3)",
            MYSQL_TYPE_VAR_STRING,
            96,
            0,
        ),
        // Measured on MySQL 8.4.11: LPAD / RPAD report length = len * 4 = 6 * 4 = 24.
        (
            "SELECT LPAD(v, 6, '*') FROM s",
            "LPAD(v, 6, '*')",
            MYSQL_TYPE_VAR_STRING,
            24,
            0,
        ),
        (
            "SELECT RPAD(v, 6, '*') FROM s",
            "RPAD(v, 6, '*')",
            MYSQL_TYPE_VAR_STRING,
            24,
            0,
        ),
        // Measured on MySQL 8.4.11: INSTR / LOCATE report LONGLONG, length 11, BINARY NUM flags.
        (
            "SELECT INSTR(v, 'B') FROM s",
            "INSTR(v, 'B')",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT LOCATE('B', v) FROM s",
            "LOCATE('B', v)",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        // Measured on MySQL 8.4.11: HEX reports VAR_STRING of width character_length * 8.
        (
            "SELECT HEX(v) FROM s",
            "HEX(v)",
            MYSQL_TYPE_VAR_STRING,
            64,
            0,
        ),
        (
            "SELECT LENGTH(v) FROM s",
            "LENGTH(v)",
            MYSQL_TYPE_LONGLONG,
            10,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT CHAR_LENGTH(v) FROM s",
            "CHAR_LENGTH(v)",
            MYSQL_TYPE_LONGLONG,
            10,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT NOW() FROM s",
            "NOW()",
            MYSQL_TYPE_DATETIME,
            19,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
        ),
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        let column = &result.columns[0];
        assert_eq!(
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ),
            (name, column_type, length, flags),
            "{sql}"
        );
        // The answer belongs to no table, as MySQL reports it.
        assert_eq!(column.table, "", "{sql}");
    }

    // The values are the ones MySQL answers, LENGTH counting bytes where
    // CHAR_LENGTH counts characters.
    let CommandExecutionResult::ResultSet(values) = adapter
        .execute_query("SELECT LOWER(v), UPPER(v), LENGTH(v), CHAR_LENGTH(v) FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        values.rows,
        vec![vec![
            Some(b"abc".to_vec()),
            Some(b"ABC".to_vec()),
            Some(b"3".to_vec()),
            Some(b"3".to_vec()),
        ]]
    );

    // Measured on MySQL 8.4.11: REPLACE is case-sensitive, matching 'B' but not 'b'.
    let CommandExecutionResult::ResultSet(replaced) = adapter
        .execute_query("SELECT REPLACE(v, 'B', 'XY'), REPLACE(v, 'b', 'XY') FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        replaced.rows,
        vec![vec![
            Some(b"aXYc".to_vec()),
            Some(b"aBc".to_vec()),
        ]]
    );

    // Measured on MySQL 8.4.11: REVERSE reverses characters and REPEAT repeats the string.
    let CommandExecutionResult::ResultSet(rev_rep) = adapter
        .execute_query("SELECT REVERSE(v), REPEAT(v, 3), REPEAT(v, 0) FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rev_rep.rows,
        vec![vec![
            Some(b"cBa".to_vec()),
            Some(b"aBcaBcaBc".to_vec()),
            Some(b"".to_vec()),
        ]]
    );

    // Measured on MySQL 8.4.11: LPAD and RPAD pad with specified string and truncate when needed.
    let CommandExecutionResult::ResultSet(padded) = adapter
        .execute_query("SELECT LPAD(v, 6, '*'), RPAD(v, 6, '*'), LPAD(v, 2, '*') FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        padded.rows,
        vec![vec![
            Some(b"***aBc".to_vec()),
            Some(b"aBc***".to_vec()),
            Some(b"aB".to_vec()),
        ]]
    );

    // Measured on MySQL 8.4.11: HEX answers latin1_swedish_ci (8) and hex encoded string.
    let CommandExecutionResult::ResultSet(hexed) = adapter
        .execute_query("SELECT HEX(v) FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(hexed.columns[0].character_set, MYSQL_LATIN1_SWEDISH_CI_COLLATION);
    assert_eq!(hexed.rows, vec![vec![Some(b"614263".to_vec())]]);

    // Measured on MySQL 8.4.11: HEX over numeric column is unsupported.
    assert!(adapter.execute_query("SELECT HEX(n) FROM s").is_err());

    // Measured on MySQL 8.4.11: LOCATE and INSTR find 1-based substring position or 0.
    let CommandExecutionResult::ResultSet(located) = adapter
        .execute_query("SELECT LOCATE('B', v), INSTR(v, 'B'), LOCATE('z', v), INSTR(v, 'z') FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        located.rows,
        vec![vec![
            Some(b"2".to_vec()),
            Some(b"2".to_vec()),
            Some(b"0".to_vec()),
            Some(b"0".to_vec()),
        ]]
    );

    // Measured on 8.4.11 over an INT of length 11 and a DECIMAL(10,2) of
    // length 12: ABS keeps the column's own width and scale, ROUND, FLOOR
    // answers 21 however wide the argument was, and IFNULL keeps the
    // width and cannot be null.
    for (sql, name, column_type, length, flags) in [
        (
            "SELECT ABS(n) FROM s",
            "ABS(n)",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT ROUND(n) FROM s",
            "ROUND(n)",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT FLOOR(n) FROM s",
            "FLOOR(n)",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT CEIL(n) FROM s",
            "CEIL(n)",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT CEILING(n) FROM s",
            "CEILING(n)",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT IFNULL(n, 0) FROM s",
            "IFNULL(n, 0)",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        let column = &result.columns[0];
        assert_eq!(
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ),
            (name, column_type, length, flags),
            "{sql}"
        );
    }

    // The engine answers ROUND as a float where MySQL answers a whole
    // number, so the rendered SQL casts; without it the row would read as
    // an integer overflow.
    let CommandExecutionResult::ResultSet(rounded) = adapter
        .execute_query(
            "SELECT ABS(n), ROUND(n), FLOOR(n), CEIL(n), CEILING(n), IFNULL(n, 0) FROM s",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rounded.rows,
        vec![vec![
            Some(b"7".to_vec()),
            Some(b"-7".to_vec()),
            Some(b"-7".to_vec()),
            Some(b"-7".to_vec()),
            Some(b"-7".to_vec()),
            Some(b"-7".to_vec()),
        ]]
    );

    // Measured: CONCAT is as wide as its arguments laid end to end, a
    // string literal counting the characters it spells, and LEFT and
    // RIGHT are as wide as the count they were asked for.
    for (sql, name, length) in [
        ("SELECT CONCAT(v, 'z') FROM s", "CONCAT(v, 'z')", 36),
        ("SELECT CONCAT(v, v) FROM s", "CONCAT(v, v)", 64),
        ("SELECT LEFT(v, 2) FROM s", "LEFT(v, 2)", 8),
        ("SELECT RIGHT(v, 2) FROM s", "RIGHT(v, 2)", 8),
        ("SELECT SUBSTRING(v, 1, 2) FROM s", "SUBSTRING(v, 1, 2)", 8),
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        let column = &result.columns[0];
        assert_eq!(
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ),
            (name, MYSQL_TYPE_VAR_STRING, length, 0),
            "{sql}"
        );
    }

    // Measured: every TRIM form over that VARCHAR(8) answers a VAR_STRING of
    // 8 characters with no flags, the same shape LOWER answers.
    let CommandExecutionResult::ResultSet(trimmed) = adapter
        .execute_query(
            "SELECT TRIM(p), TRIM(LEADING 'x' FROM p), TRIM(TRAILING 'x' FROM p), TRIM('x' FROM p) FROM s",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    for column in &trimmed.columns {
        assert_eq!(
            (column.column_type, column.column_length, column.flags),
            (MYSQL_TYPE_VAR_STRING, 32, 0),
            "{}",
            column.name
        );
    }
    assert_eq!(
        trimmed.rows,
        vec![vec![
            Some(b"xxaBxx".to_vec()),
            Some(b"aBxx".to_vec()),
            Some(b"xxaB".to_vec()),
            Some(b"aB".to_vec()),
        ]]
    );

    // MySQL removes whole copies of what it was given where the engine removes
    // any of its characters, and they only agree on one character.
    assert!(adapter
        .execute_query("SELECT TRIM(LEADING 'ax' FROM p) FROM s")
        .is_err());

    // MySQL's CONCAT answers NULL when any argument is, which the engine's
    // own `concat` does not — the rendered SQL uses `||` for that reason.
    let CommandExecutionResult::ResultSet(pieces) = adapter
        .execute_query("SELECT CONCAT(v, 'z'), LEFT(v, 2), RIGHT(v, 2), SUBSTRING(v, 1, 2) FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        pieces.rows,
        vec![vec![
            Some(b"aBcz".to_vec()),
            Some(b"aB".to_vec()),
            Some(b"Bc".to_vec()),
            Some(b"aB".to_vec()),
        ]]
    );

    // Measured: a CASE or an IF is as wide as its widest branch, and NOT
    // NULL because every branch is a literal and there is an ELSE.
    for (sql, name) in [
        (
            "SELECT CASE WHEN n > 1 THEN 'y' ELSE 'n' END FROM s",
            "CASE WHEN n > 1 THEN 'y' ELSE 'n' END",
        ),
        ("SELECT IF(n > 1, 'y', 'n') FROM s", "IF(n > 1, 'y', 'n')"),
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        let column = &result.columns[0];
        assert_eq!(
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ),
            (name, MYSQL_TYPE_VAR_STRING, 4, MYSQL_NOT_NULL_FLAG),
            "{sql}"
        );
        // The row holds -7, so the ELSE branch answers.
        assert_eq!(result.rows, vec![vec![Some(b"n".to_vec())]], "{sql}");
    }

    // A widest branch of three characters answers twelve bytes.
    let CommandExecutionResult::ResultSet(wider) = adapter
        .execute_query("SELECT CASE WHEN n > 1 THEN 'yes' ELSE 'no' END FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(wider.columns[0].column_length, 12);
    assert_eq!(wider.rows, vec![vec![Some(b"no".to_vec())]]);

    // Measured: without an ELSE, and with a NULL branch, the width is still
    // the widest string branch and the NOT_NULL flag is gone.
    for sql in [
        "SELECT CASE WHEN n > 1 THEN 'yes' END FROM s",
        "SELECT CASE WHEN n > 1 THEN 'yes' ELSE NULL END FROM s",
        "SELECT CASE WHEN n > 1 THEN NULL ELSE 'yes' END FROM s",
        "SELECT IF(n > 1, 'yes', NULL) FROM s",
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            (
                result.columns[0].column_type,
                result.columns[0].column_length,
                result.columns[0].flags
            ),
            (MYSQL_TYPE_VAR_STRING, 12, 0),
            "{sql}"
        );
    }
    // The row holds -7, so only the branch naming a string can answer.
    let CommandExecutionResult::ResultSet(unmatched) = adapter
        .execute_query("SELECT CASE WHEN n > 1 THEN 'yes' END FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(unmatched.rows, vec![vec![None]]);

    // Every branch NULL leaves no width to answer with, and a `CASE col WHEN`
    // compares its operand, which raises a coercion question this has not
    // measured.
    assert!(adapter
        .execute_query("SELECT CASE WHEN n > 1 THEN NULL ELSE NULL END FROM s")
        .is_err());
    assert!(adapter
        .execute_query("SELECT CASE n WHEN 1 THEN 'y' ELSE 'n' END FROM s")
        .is_err());

    // MySQL takes these over a number by coercing it, which has not been
    // measured, and an expression argument has no length this can work out.
    assert_eq!(
        adapter.execute_query("SELECT LOWER(id) FROM s"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert!(adapter
        .execute_query("SELECT LOWER(v || 'z') FROM s")
        .is_err());
    assert_eq!(
        adapter.execute_query("SELECT ABS(v) FROM s"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// A CTE names a subquery, and its result columns carry the base column's
/// own metadata under the CTE's name — measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn a_cte_names_a_subquery_and_keeps_its_columns_metadata() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([31; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE f (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO f (id, n) VALUES (1, 7), (2, 8)")
        .unwrap();

    let CommandExecutionResult::ResultSet(plain) = adapter
        .execute_query("WITH c AS (SELECT id, n FROM f) SELECT c.id, c.n FROM c")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        plain.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"7".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"8".to_vec())],
        ]
    );
    // Measured: the column names the CTE as its table and carries the base
    // column's own type and flags.
    assert_eq!(
        plain
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "id",
                "c",
                "f",
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_PRI_KEY_FLAG
                    | MYSQL_PART_KEY_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG
                    | MYSQL_NUM_FLAG
            ),
            ("n", "c", "f", MYSQL_NUM_FLAG),
        ]
    );

    // A CTE can project its table's columns in any order, so the ordinal a
    // result column carries counts through what the CTE projected — not
    // through the table, which would hand each column the other's flags.
    let CommandExecutionResult::ResultSet(reordered) = adapter
        .execute_query("WITH c AS (SELECT n, id FROM f) SELECT c.n, c.id FROM c")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        reordered
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.flags))
            .collect::<Vec<_>>(),
        vec![
            ("n", MYSQL_NUM_FLAG),
            (
                "id",
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_PRI_KEY_FLAG
                    | MYSQL_PART_KEY_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG
                    | MYSQL_NUM_FLAG
            ),
        ]
    );

    // A body this cannot resolve an ordinal through is refused, and so is
    // one naming an internal catalog table.
    assert!(adapter
        .execute_query("WITH c AS (SELECT * FROM f) SELECT c.id FROM c")
        .is_err());
    assert!(adapter
        .execute_query("WITH c AS (SELECT rootpage FROM sqlite_schema) SELECT c.rootpage FROM c")
        .is_err());
}

/// A subquery in a WHERE reads its own table, which is authorized like any
/// other and names none of the result columns.
#[cfg(unix)]
#[test]
fn a_subquery_in_a_where_reads_its_own_table() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([30; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE people (id INT NOT NULL PRIMARY KEY, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE pets (id INT NOT NULL PRIMARY KEY, owner_id INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO people (id, name) VALUES (1, 'ann'), (2, 'bob')")
        .unwrap();
    adapter
        .execute_query("INSERT INTO pets (id, owner_id) VALUES (10, 1)")
        .unwrap();

    let CommandExecutionResult::ResultSet(members) = adapter
        .execute_query("SELECT id FROM people WHERE id IN (SELECT owner_id FROM pets)")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(members.rows, vec![vec![Some(b"1".to_vec())]]);
    // The result column is the outer one, with the table metadata it would
    // have had without the subquery.
    assert_eq!(members.columns[0].original_table, "people");
    assert_eq!(members.columns[0].column_type, MYSQL_TYPE_LONG);

    let CommandExecutionResult::ResultSet(absent) = adapter
        .execute_query("SELECT id FROM people WHERE id NOT IN (SELECT owner_id FROM pets)")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(absent.rows, vec![vec![Some(b"2".to_vec())]]);

    let CommandExecutionResult::ResultSet(any) = adapter
        .execute_query("SELECT id FROM people WHERE EXISTS (SELECT id FROM pets) ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        any.rows,
        vec![vec![Some(b"1".to_vec())], vec![Some(b"2".to_vec())]]
    );

    // The two columns have to be the same kind, since MySQL coerces one to
    // the other and the engine compares them by affinity.
    assert_eq!(
        adapter.execute_query("SELECT id FROM people WHERE name IN (SELECT owner_id FROM pets)"),
        Err(FrontendErrorKind::Unsupported)
    );
    // And the subquery's table cannot hide from the catalog rule.
    assert!(adapter
        .execute_query("SELECT id FROM people WHERE id IN (SELECT rootpage FROM sqlite_schema)")
        .is_err());
}

/// A UNION reads two branches, and its result columns belong to neither
/// table. Measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn a_union_answers_both_branches_and_names_no_table() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([27; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE ua (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE ub (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO ua (id, n) VALUES (1, 7), (2, 8)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO ub (id, n) VALUES (2, 9)")
        .unwrap();

    // A bare UNION drops the repeat; UNION ALL keeps it.
    let CommandExecutionResult::ResultSet(distinct) = adapter
        .execute_query("SELECT id FROM ua UNION SELECT id FROM ub ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        distinct.rows,
        vec![vec![Some(b"1".to_vec())], vec![Some(b"2".to_vec())]]
    );
    let CommandExecutionResult::ResultSet(all) = adapter
        .execute_query("SELECT id FROM ua UNION ALL SELECT id FROM ub ORDER BY id LIMIT 2")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        all.rows,
        vec![vec![Some(b"1".to_vec())], vec![Some(b"2".to_vec())]]
    );

    // Measured: the column keeps its type and length and names no table,
    // and carries none of the source column's key facts.
    assert_eq!(distinct.columns[0].name, "id");
    assert_eq!(distinct.columns[0].column_type, MYSQL_TYPE_LONG);
    assert_eq!(distinct.columns[0].column_length, 11);
    assert_eq!(distinct.columns[0].table, "");
    assert_eq!(distinct.columns[0].original_table, "");
    // Measured: a numeric result carries NUM whatever else it carries.
    assert_eq!(distinct.columns[0].flags, MYSQL_NUM_FLAG);

    // Both branches are read, so both are authorized and neither can hide
    // an internal catalog table behind the other.
    assert!(adapter
        .execute_query("SELECT id FROM ua UNION SELECT rootpage FROM sqlite_schema")
        .is_err());
}

/// Every column type this frontend answers has to cross the binary
/// protocol too, not just the text one.
///
/// CHAR, DECIMAL, DATETIME and TIMESTAMP each landed with a text-protocol
/// answer and no binary one, so a prepared SELECT of any of them failed.
/// MySQL sends a CHAR and a DECIMAL as length-encoded text and a temporal
/// value as fields, which is what these now do.
#[cfg(unix)]
#[test]
fn every_column_type_crosses_the_binary_protocol() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([26; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE b (id INT NOT NULL PRIMARY KEY, c CHAR(4), d DECIMAL(10,2), ",
            "t DATETIME, s TIMESTAMP NULL, v VARCHAR(4), r DOUBLE, f FLOAT, n BIGINT)"
        ))
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO b (id, c, d, t, s, v, r, f, n) VALUES ",
            "(1, 'ab', 1.25, '2026-09-06 01:02:03', '2026-09-06 00:00:00', 'zz', 2.5, 1.5, 9), ",
            // The second row's DECIMAL needs padding to its declared scale,
            // which the binary protocol does as the text one does.
            "(2, 'ab', 1.5, '2026-09-06 01:02:03', '2026-09-06 00:00:00', 'zz', 2.5, 1.5, 9)"
        ))
        .unwrap();

    let mut binary = |sql: &str| {
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        let executed = prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &[])
                .unwrap(),
        );
        executed.rows[0][0].clone()
    };
    assert_eq!(
        binary("SELECT c FROM b WHERE id = 1"),
        BinaryResultValue::Text("ab".to_owned())
    );
    assert_eq!(
        binary("SELECT d FROM b WHERE id = 1"),
        BinaryResultValue::Text("1.25".to_owned())
    );
    assert_eq!(
        binary("SELECT d FROM b WHERE id = 2"),
        BinaryResultValue::Text("1.50".to_owned())
    );
    assert_eq!(
        binary("SELECT v FROM b WHERE id = 1"),
        BinaryResultValue::Text("zz".to_owned())
    );
    assert_eq!(
        binary("SELECT r FROM b WHERE id = 1"),
        BinaryResultValue::Real(2.5)
    );
    assert_eq!(
        binary("SELECT f FROM b WHERE id = 1"),
        BinaryResultValue::Real(1.5)
    );
    assert_eq!(
        binary("SELECT n FROM b WHERE id = 1"),
        BinaryResultValue::Integer(9)
    );
    assert_eq!(
        binary("SELECT t FROM b WHERE id = 1"),
        BinaryResultValue::DateTime {
            year: 2026,
            month: 9,
            day: 6,
            hour: 1,
            minute: 2,
            second: 3,
        }
    );
    // Midnight is the same value; MySQL's own client sends only the date
    // for it, which is what the encoder does.
    assert_eq!(
        binary("SELECT s FROM b WHERE id = 1"),
        BinaryResultValue::DateTime {
            year: 2026,
            month: 9,
            day: 6,
            hour: 0,
            minute: 0,
            second: 0,
        }
    );
}

/// MySQL reads a `HAVING` with no `GROUP BY` over one implicit group of every
/// row. Measured on MySQL 8.4.11 over rows (1,'a',10), (2,'a',30), (3,'b',20):
/// `SELECT COUNT(*) FROM t HAVING COUNT(*) > 1` answers one row holding 3, and
/// `... > 5` answers no rows at all. The result column is the one `COUNT(*)`
/// reports on its own — LONGLONG, length 21, NOT_NULL BINARY NUM — so the
/// HAVING changes which rows come back and nothing about their shape.
#[cfg(unix)]
#[test]
fn a_having_without_a_group_by_filters_the_one_implicit_group() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([28; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, team VARCHAR(20), n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, team, n) VALUES (1, 'a', 10), (2, 'a', 30), (3, 'b', 20)")
        .unwrap();

    let CommandExecutionResult::ResultSet(kept) = adapter
        .execute_query("SELECT COUNT(*) FROM t HAVING COUNT(*) > 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(kept.rows, vec![vec![Some(b"3".to_vec())]]);
    assert_eq!(kept.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(kept.columns[0].column_length, 21);
    assert_eq!(
        kept.columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    // The implicit group is filtered out whole, so no row comes back at all.
    let CommandExecutionResult::ResultSet(dropped) = adapter
        .execute_query("SELECT COUNT(*) FROM t HAVING COUNT(*) > 5")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert!(dropped.rows.is_empty());
    assert_eq!(dropped.columns[0].column_type, MYSQL_TYPE_LONGLONG);

    // A WHERE narrows the group before the HAVING weighs it.
    let CommandExecutionResult::ResultSet(narrowed) = adapter
        .execute_query("SELECT COUNT(*) FROM t WHERE n > 15 HAVING COUNT(*) > 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(narrowed.rows, vec![vec![Some(b"2".to_vec())]]);

    // MySQL answers 1140 for a bare column in the projection of an aggregated
    // statement and 1054 for one in the HAVING; both are refused here.
    assert!(adapter
        .execute_query("SELECT team FROM t HAVING COUNT(*) > 1")
        .is_err());
    assert!(adapter
        .execute_query("SELECT COUNT(*) FROM t HAVING team = 'a'")
        .is_err());
}

/// `CHECK TABLE` verifies that the stored data reads back. Measured on MySQL
/// 8.4.11: one row of `<database>.<table>`, `check`, `status`, `OK`, over the
/// same four columns `ANALYZE TABLE` answers.
#[cfg(unix)]
#[test]
fn check_table_verifies_the_storage_and_says_so() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([44; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, n) VALUES (1, 10), (2, 20)")
        .unwrap();

    let CommandExecutionResult::ResultSet(checked) =
        adapter.execute_query("CHECK TABLE t").unwrap()
    else {
        panic!("CHECK TABLE must return a result set");
    };
    assert_eq!(
        checked
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Table", "Op", "Msg_type", "Msg_text"]
    );
    assert_eq!(
        checked.rows,
        vec![vec![
            Some(b"reports.t".to_vec()),
            Some(b"check".to_vec()),
            Some(b"status".to_vec()),
            Some(b"OK".to_vec()),
        ]]
    );

    assert!(adapter.execute_query("CHECK TABLE nosuch").is_err());
    assert!(adapter.execute_query("CHECK TABLE t QUICK").is_err());
}

/// `ANALYZE TABLE` refreshes the planner's statistics and reports what it did.
/// Measured on MySQL 8.4.11: one row of `<database>.<table>`, `analyze`,
/// `status`, `OK`, over four latin1 columns of length 128, 10, 10 and 393216.
#[cfg(unix)]
#[test]
fn analyze_table_refreshes_the_statistics_and_says_so() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([43; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, n) VALUES (1, 10), (2, 20)")
        .unwrap();

    let CommandExecutionResult::ResultSet(analyzed) =
        adapter.execute_query("ANALYZE TABLE t").unwrap()
    else {
        panic!("ANALYZE TABLE must return a result set");
    };
    let names = analyzed
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["Table", "Op", "Msg_type", "Msg_text"]);
    assert_eq!(analyzed.columns[0].column_length, 128);
    assert_eq!(analyzed.columns[3].column_type, MYSQL_TYPE_BLOB);
    assert_eq!(analyzed.columns[3].column_length, 393_216);
    assert_eq!(
        analyzed.rows,
        vec![vec![
            Some(b"reports.t".to_vec()),
            Some(b"analyze".to_vec()),
            Some(b"status".to_vec()),
            Some(b"OK".to_vec()),
        ]]
    );

    // The table is looked up first, so a name that is not there answers rather
    // than analysing everything quietly.
    assert!(adapter.execute_query("ANALYZE TABLE nosuch").is_err());
    // One table at a time, and none of the options.
    assert!(adapter.execute_query("ANALYZE TABLE t, t").is_err());
}

/// `INSERT ... SELECT` reads rows rather than listing them. Measured on MySQL
/// 8.4.11 over rows (1,10),(2,20),(3,30):
/// `INSERT INTO dst (id, n) SELECT id, n FROM src WHERE n > 15` writes two rows
/// and counts 2.
///
/// The table the SELECT reads has to be authorized and checked against the
/// internal catalog like any other read, which is what the statement's read
/// tables carry.
#[cfg(unix)]
#[test]
fn insert_select_writes_the_rows_it_reads_and_names_the_table_it_read() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([42; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE src (id INT, n INT)")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE dst (id INT, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO src (id, n) VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();

    let CommandExecutionResult::Ok(copied) = adapter
        .execute_query("INSERT INTO dst (id, n) SELECT id, n FROM src WHERE n > 15")
        .unwrap()
    else {
        panic!("INSERT must return an OK packet");
    };
    assert_eq!(copied.affected_rows, 2);

    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id, n FROM dst ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rows.rows,
        vec![
            vec![Some(b"2".to_vec()), Some(b"20".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"30".to_vec())],
        ]
    );

    // The read table is checked against the internal catalog, which a plain
    // SELECT of it is too. Without the statement naming what it reads, this
    // would have gone through.
    assert!(adapter
        .execute_query("INSERT INTO dst (id, n) SELECT id, n FROM sqlite_schema")
        .is_err());

    // A column list is required, because the SELECT's own columns are not
    // matched against the table's here.
    assert!(adapter
        .execute_query("INSERT INTO dst SELECT id, n FROM src")
        .is_err());
}

/// `SHOW TABLE STATUS` describes each table. Measured on MySQL 8.4.11 for the
/// eighteen column shapes; the values are answered about this server, which
/// means NULL for every storage figure InnoDB keeps and this does not. NULL is
/// a shape MySQL produces here too, for a view.
///
/// The row count is counted rather than estimated: MySQL's is an InnoDB
/// estimate, and a real count is the more useful answer and the only one this
/// can give.
#[cfg(unix)]
#[test]
fn show_table_status_answers_what_it_knows_and_nulls_the_rest() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([41; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id) VALUES (1), (2), (3)")
        .unwrap();

    let CommandExecutionResult::ResultSet(status) =
        adapter.execute_query("SHOW TABLE STATUS").unwrap()
    else {
        panic!("SHOW TABLE STATUS must return a result set");
    };
    let names = status
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "Name",
            "Engine",
            "Version",
            "Row_format",
            "Rows",
            "Avg_row_length",
            "Data_length",
            "Max_data_length",
            "Index_length",
            "Data_free",
            "Auto_increment",
            "Create_time",
            "Update_time",
            "Check_time",
            "Collation",
            "Checksum",
            "Create_options",
            "Comment",
        ]
    );
    assert_eq!(status.columns[4].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(status.columns[4].column_length, 21);
    assert_eq!(status.columns[11].column_type, MYSQL_TYPE_TIMESTAMP);
    assert_eq!(status.columns[17].column_type, MYSQL_TYPE_BLOB);

    let row = status
        .rows
        .iter()
        .find(|row| row[0] == Some(b"t".to_vec()))
        .expect("the created table is described");
    assert_eq!(row[1], Some(b"InnoDB".to_vec()));
    // Counted, not estimated.
    assert_eq!(row[4], Some(b"3".to_vec()));
    assert_eq!(row[14], Some(b"utf8mb4_0900_ai_ci".to_vec()));
    // Every storage figure InnoDB keeps and this does not.
    for ordinal in [2, 3, 5, 6, 7, 8, 9, 11, 12, 13, 15] {
        assert_eq!(row[ordinal], None, "column {ordinal}");
    }

    // The filters are not read yet, so they are refused rather than ignored.
    assert!(adapter
        .execute_query("SHOW TABLE STATUS LIKE 't'")
        .is_err());
}

/// A dumped schema spells out the charset and collation on every text column,
/// so refusing them stops a mysqldump from being restored. Naming the one this
/// server has is taken; naming another is refused, because it is a claim about
/// ordering and case this cannot keep.
///
/// Measured on MySQL 8.4.11: `SHOW CREATE TABLE` echoes the clause back even
/// when it names the table default. This does not — the engine has no place to
/// keep the words — so the column's DDL comes back without them. COMPAT.md
/// records that.
#[cfg(unix)]
#[test]
fn a_column_charset_and_collation_are_taken_when_they_name_this_server() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([40; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();

    adapter
        .execute_query(
            "CREATE TABLE c (a VARCHAR(10) CHARACTER SET utf8mb4, \
             b VARCHAR(10) COLLATE utf8mb4_0900_ai_ci, \
             d VARCHAR(10) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci)",
        )
        .unwrap();
    adapter
        .execute_query("INSERT INTO c (a, b, d) VALUES ('x', 'y', 'z')")
        .unwrap();
    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query("SELECT a, b, d FROM c")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.rows,
        vec![vec![
            Some(b"x".to_vec()),
            Some(b"y".to_vec()),
            Some(b"z".to_vec())
        ]]
    );

    // A collation this server does not have is refused rather than ignored:
    // utf8mb4_bin compares case-sensitively and this does not.
    assert!(adapter
        .execute_query("CREATE TABLE n (a VARCHAR(10) COLLATE utf8mb4_bin)")
        .is_err());
    assert!(adapter
        .execute_query("CREATE TABLE n (a VARCHAR(10) CHARACTER SET latin1)")
        .is_err());
}

/// `START TRANSACTION READ ONLY` is a promise MySQL keeps. Measured on MySQL
/// 8.4.11: a read inside one works, a write answers 1792, and `READ WRITE` is
/// the default spelled out. A DDL statement is not held to it, because it
/// commits what came before and so leaves the read-only transaction first —
/// measured, `START TRANSACTION READ ONLY; CREATE TABLE u (...)` is taken.
#[cfg(unix)]
#[test]
fn a_read_only_transaction_refuses_a_write_and_keeps_reading() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([39; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id) VALUES (1)")
        .unwrap();

    adapter
        .execute_query("START TRANSACTION READ ONLY")
        .unwrap();
    let CommandExecutionResult::ResultSet(read) =
        adapter.execute_query("SELECT id FROM t").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(read.rows, vec![vec![Some(b"1".to_vec())]]);
    assert!(adapter
        .execute_query("INSERT INTO t (id) VALUES (2)")
        .is_err());
    adapter.execute_query("COMMIT").unwrap();

    // The promise ends with the transaction.
    adapter
        .execute_query("INSERT INTO t (id) VALUES (2)")
        .unwrap();

    // READ WRITE is the default spelled out, so it changes nothing.
    adapter
        .execute_query("START TRANSACTION READ WRITE")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id) VALUES (3)")
        .unwrap();
    adapter.execute_query("COMMIT").unwrap();
    let CommandExecutionResult::ResultSet(all) = adapter
        .execute_query("SELECT id FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(all.rows.len(), 3);
}

/// `VARBINARY(n)` holds bytes rather than characters. Measured on MySQL
/// 8.4.11: it reports VAR_STRING with length 255 for `VARBINARY(255)` — the
/// declared count itself, not four bytes for each of them — the binary
/// collation, and the BINARY flag. `SHOW COLUMNS` prints `varbinary(255)`.
///
/// `BINARY(n)` is refused. Measured on the same server, it pads a shorter value
/// with NUL bytes to the declared width: `'ab'` in a `BINARY(16)` reads back
/// sixteen bytes long. The engine has no padding, so taking it would store a
/// different value than MySQL stores.
#[cfg(unix)]
#[test]
fn varbinary_holds_bytes_and_binary_is_refused_for_its_padding() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([38; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE b (id INT NOT NULL PRIMARY KEY, v VARBINARY(255))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO b (id, v) VALUES (1, 'cd')")
        .unwrap();

    let CommandExecutionResult::ResultSet(read) =
        adapter.execute_query("SELECT v FROM b").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let column = &read.columns[0];
    assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(column.column_length, 255);
    assert_eq!(column.character_set, MYSQL_BINARY_COLLATION);
    assert_eq!(column.flags & MYSQL_BINARY_FLAG, MYSQL_BINARY_FLAG);
    assert_eq!(read.rows, vec![vec![Some(b"cd".to_vec())]]);

    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM b").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        String::from_utf8(columns.rows[1][1].clone().unwrap()).unwrap(),
        "varbinary(255)"
    );

    // The declared count is bytes, so a longer value is refused.
    adapter
        .execute_query("CREATE TABLE n (id INT NOT NULL PRIMARY KEY, v VARBINARY(2))")
        .unwrap();
    assert!(adapter
        .execute_query("INSERT INTO n (id, v) VALUES (1, 'abc')")
        .is_err());

    // BINARY(n) pads, and the engine does not.
    assert!(adapter
        .execute_query("CREATE TABLE f (id INT NOT NULL PRIMARY KEY, v BINARY(16))")
        .is_err());
}

/// A `FOREIGN KEY` is refused rather than taken, and this pins that it is
/// refused at the door rather than accepted and left unenforced.
///
/// The parser can translate one — its own tests cover that — so the refusal is
/// the frontend's, and it is the right one. The engine runs with
/// `PRAGMA foreign_keys` off, so a constraint taken here would not be enforced,
/// where MySQL answers 1452 for a child row whose parent does not exist
/// (measured on 8.4.11). Taking the syntax before the enforcement exists would
/// hand a client a guarantee it does not have.
#[cfg(unix)]
#[test]
fn a_foreign_key_is_refused_rather_than_taken_unenforced() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([37; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE parent (id INT NOT NULL PRIMARY KEY)")
        .unwrap();

    assert!(adapter
        .execute_query(
            "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT, \
             FOREIGN KEY (parent_id) REFERENCES parent (id))",
        )
        .is_err());
    // The inline spelling is refused too. MySQL parses that one and ignores it
    // — measured on 8.4.11, `SHOW CREATE TABLE` shows no key and an orphan row
    // inserts — so taking it would mean writing a constraint MySQL does not.
    assert!(adapter
        .execute_query(
            "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT REFERENCES parent (id))",
        )
        .is_err());
}

/// MySQL takes several operations in one `ALTER TABLE` and the engine takes
/// one, so the statement becomes several run inside a transaction. Measured on
/// MySQL 8.4.11: `ADD COLUMN a, ADD COLUMN b` adds both, and
/// `ADD COLUMN c, ADD COLUMN a` against a table that already has `a` answers
/// 1060 and adds neither — `c` is not there afterwards.
#[cfg(unix)]
#[test]
fn a_multi_operation_alter_table_applies_all_of_it_or_none() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([36; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT, keep INT)")
        .unwrap();

    fn column_names(adapter: &mut impl AuthenticatedCommandExecutor) -> Vec<String> {
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SHOW COLUMNS FROM t").unwrap()
        else {
            panic!("SHOW COLUMNS must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect()
    }

    adapter
        .execute_query("ALTER TABLE t ADD COLUMN a INT, ADD COLUMN b INT")
        .unwrap();
    assert_eq!(column_names(&mut adapter), vec!["id", "keep", "a", "b"]);

    // The second operation fails, so neither is applied and `c` is not there.
    assert!(adapter
        .execute_query("ALTER TABLE t ADD COLUMN c INT, ADD COLUMN a INT")
        .is_err());
    assert_eq!(column_names(&mut adapter), vec!["id", "keep", "a", "b"]);
}

/// `SHOW ENGINES` answers with the one storage engine this server has.
///
/// MySQL 8.4.11 lists eleven, most unavailable; naming MyISAM or CSV here would
/// claim engines that do not exist. The column shapes are measured from that
/// server — six VAR_STRING columns of length 64, 8, 80, 3, 3 and 3, latin1
/// collation, the first three NOT NULL — and the last three values describe
/// this server rather than MySQL's InnoDB row, which says YES to all three.
#[cfg(unix)]
#[test]
fn show_engines_answers_the_one_engine_and_says_what_it_does_not_do() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([35; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();

    let CommandExecutionResult::ResultSet(engines) =
        adapter.execute_query("SHOW ENGINES").unwrap()
    else {
        panic!("SHOW ENGINES must return a result set");
    };
    let names = engines
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "Engine",
            "Support",
            "Comment",
            "Transactions",
            "XA",
            "Savepoints"
        ]
    );
    for (ordinal, length, not_null) in [
        (0, 64, true),
        (1, 8, true),
        (2, 80, true),
        (3, 3, false),
        (4, 3, false),
        (5, 3, false),
    ] {
        let column = &engines.columns[ordinal];
        assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING, "{ordinal}");
        assert_eq!(column.column_length, length, "{ordinal}");
        assert_eq!(column.character_set, 8, "{ordinal}");
        assert_eq!(
            column.flags & MYSQL_NOT_NULL_FLAG,
            if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
            "{ordinal}"
        );
    }

    // One row, and it does not claim the XA and savepoint support MySQL's
    // InnoDB row claims.
    assert_eq!(engines.rows.len(), 1);
    let row = &engines.rows[0];
    assert_eq!(row[0], Some(b"InnoDB".to_vec()));
    assert_eq!(row[1], Some(b"DEFAULT".to_vec()));
    assert_eq!(row[3], Some(b"YES".to_vec()));
    assert_eq!(row[4], Some(b"NO".to_vec()));
    assert_eq!(row[5], Some(b"NO".to_vec()));
}

/// A temporary table lives for the connection and shadows a permanent table of
/// the same name. Measured on MySQL 8.4.11: after `CREATE TEMPORARY TABLE t`
/// over an existing `t`, a `SELECT` reads the temporary one, and `SHOW TABLES`
/// lists only the permanent one.
#[cfg(unix)]
#[test]
fn a_temporary_table_shadows_the_permanent_one_and_stays_out_of_show_tables() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([34; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT, note VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, note) VALUES (1, 'perm')")
        .unwrap();
    adapter
        .execute_query("CREATE TEMPORARY TABLE t (id INT, note VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, note) VALUES (2, 'temp')")
        .unwrap();

    let CommandExecutionResult::ResultSet(read) =
        adapter.execute_query("SELECT id, note FROM t").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.rows,
        vec![vec![Some(b"2".to_vec()), Some(b"temp".to_vec())]]
    );

    let CommandExecutionResult::ResultSet(listed) = adapter.execute_query("SHOW TABLES").unwrap()
    else {
        panic!("SHOW TABLES must return a result set");
    };
    let names = listed
        .rows
        .iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect::<Vec<_>>();
    // The permanent `t` is listed once and the temporary one not at all, which
    // is what MySQL does.
    assert_eq!(names.iter().filter(|name| *name == "t").count(), 1);
}

/// `ON DUPLICATE KEY UPDATE` is an upsert: it writes the row, or updates the
/// one already there. Measured on MySQL 8.4.11 over a table holding (1, 10):
/// inserting (2, 30) counts 1, updating row 1 to a different value counts 2,
/// and an update that leaves the row identical counts 0. `VALUES(v)` names the
/// value the row was offered.
#[cfg(unix)]
#[test]
fn on_duplicate_key_update_writes_or_updates_the_row() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([33; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE k (id INT NOT NULL PRIMARY KEY, v INT, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO k (id, v, n) VALUES (1, 10, 100)")
        .unwrap();

    // A row that collides is updated, and the columns the clause does not name
    // are left alone. MySQL counts this 2 — the attempted insert and the
    // update — where the engine counts the changed row once.
    let CommandExecutionResult::Ok(updated) = adapter
        .execute_query("INSERT INTO k (id, v) VALUES (1, 20) ON DUPLICATE KEY UPDATE v = 20")
        .unwrap()
    else {
        panic!("INSERT must return an OK packet");
    };
    assert_eq!(updated.affected_rows, 1);
    // A row that does not collide is written.
    let CommandExecutionResult::Ok(inserted) = adapter
        .execute_query("INSERT INTO k (id, v) VALUES (2, 30) ON DUPLICATE KEY UPDATE v = 30")
        .unwrap()
    else {
        panic!("INSERT must return an OK packet");
    };
    assert_eq!(inserted.affected_rows, 1);
    // VALUES(v) is the value the row was offered.
    adapter
        .execute_query("INSERT INTO k (id, v) VALUES (1, 99) ON DUPLICATE KEY UPDATE v = VALUES(v)")
        .unwrap();

    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id, v, n FROM k ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rows.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"99".to_vec()),
                Some(b"100".to_vec())
            ],
            vec![Some(b"2".to_vec()), Some(b"30".to_vec()), None],
        ]
    );

    // An update that leaves the row identical counts 0 in MySQL and 1 here:
    // the engine's upsert rewrites the row whether or not the value moved, so
    // the changed-row counter sees a write. Recorded in COMPAT.md.
    let CommandExecutionResult::Ok(unchanged) = adapter
        .execute_query("INSERT INTO k (id, v) VALUES (1, 99) ON DUPLICATE KEY UPDATE v = 99")
        .unwrap()
    else {
        panic!("INSERT must return an OK packet");
    };
    assert_eq!(unchanged.affected_rows, 1);

    // The allocator reserves before the upsert can turn a row into an update,
    // so an AUTO_INCREMENT table refuses the clause.
    adapter
        .execute_query("CREATE TABLE ka (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)")
        .unwrap();
    assert!(adapter
        .execute_query("INSERT INTO ka (v) VALUES (1) ON DUPLICATE KEY UPDATE v = 2")
        .is_err());
}

/// `INSERT IGNORE` skips a colliding row instead of failing the statement.
/// Measured on MySQL 8.4.11 over a table already holding row 1: inserting row 1
/// again leaves the stored row alone and counts 0, and a two-row statement
/// where only the second is new counts 1.
///
/// What MySQL also does under IGNORE — coerce a value it would otherwise
/// refuse — is not done here. Measured: `INSERT IGNORE` of NULL into a NOT NULL
/// INT stores 0, and of 99999999999999 into an INT stores 2147483647. Both are
/// refused here, so a client sees an error rather than a row it did not ask
/// for.
#[cfg(unix)]
#[test]
fn insert_ignore_skips_a_colliding_row_and_still_refuses_a_coerced_value() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([32; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE g (id INT NOT NULL PRIMARY KEY, v INT NOT NULL)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO g (id, v) VALUES (1, 10)")
        .unwrap();

    // The colliding row is skipped and the stored one is left alone.
    let CommandExecutionResult::Ok(collided) = adapter
        .execute_query("INSERT IGNORE INTO g (id, v) VALUES (1, 20)")
        .unwrap()
    else {
        panic!("INSERT must return an OK packet");
    };
    assert_eq!(collided.affected_rows, 0);

    // Only the new row of the two is written.
    let CommandExecutionResult::Ok(mixed) = adapter
        .execute_query("INSERT IGNORE INTO g (id, v) VALUES (1, 20), (3, 30)")
        .unwrap()
    else {
        panic!("INSERT must return an OK packet");
    };
    assert_eq!(mixed.affected_rows, 1);

    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id, v FROM g ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rows.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"10".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"30".to_vec())],
        ]
    );

    // A value MySQL would coerce under IGNORE is still refused, so no row
    // appears holding a value the client never wrote. The range check is the
    // frontend's own and fires whatever the verb; the NULL is refused by the
    // parser, because the engine's OR IGNORE would skip the row where MySQL
    // stores a coerced 0.
    assert!(adapter
        .execute_query("INSERT IGNORE INTO g (id, v) VALUES (4, NULL)")
        .is_err());
    assert!(adapter
        .execute_query("INSERT IGNORE INTO g (id, v) VALUES (5, 99999999999999)")
        .is_err());
    let CommandExecutionResult::ResultSet(after) = adapter
        .execute_query("SELECT id FROM g ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        after.rows,
        vec![vec![Some(b"1".to_vec())], vec![Some(b"3".to_vec())]]
    );

    // The allocator reserves its range before the rows are written, so IGNORE
    // is refused on an AUTO_INCREMENT table rather than left to interact with
    // it unmeasured.
    adapter
        .execute_query("CREATE TABLE ga (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)")
        .unwrap();
    assert!(adapter
        .execute_query("INSERT IGNORE INTO ga (v) VALUES (1)")
        .is_err());
}

/// MySQL's `INSERT ... SET` writes the row the column-list form writes.
/// Measured on MySQL 8.4.11: `INSERT INTO s SET id = 1, a = 2, b = 'x'` stores
/// the same row, and a column the SET leaves out takes its default.
#[cfg(unix)]
#[test]
fn the_insert_set_form_writes_the_row_the_column_list_form_writes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([31; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE s (id INT NOT NULL PRIMARY KEY, a INT, b VARCHAR(10))")
        .unwrap();

    adapter
        .execute_query("INSERT INTO s SET id = 1, a = 2, b = 'x'")
        .unwrap();
    // A column the SET leaves out takes its default.
    adapter.execute_query("INSERT INTO s SET id = 2").unwrap();

    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id, a, b FROM s ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rows.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"2".to_vec()),
                Some(b"x".to_vec())
            ],
            vec![Some(b"2".to_vec()), None, None],
        ]
    );

    // An AUTO_INCREMENT table is where the two forms would diverge, because
    // the allocator only understands the column-list one. It is refused rather
    // than let through to number itself.
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)")
        .unwrap();
    assert!(adapter.execute_query("INSERT INTO t SET v = 1").is_err());
}

/// An unsigned integer column reports the same wire type its signed
/// counterpart does, one digit narrower, with the UNSIGNED flag. Measured on
/// MySQL 8.4.11: TINY/3, SHORT/5, INT24/8 and LONG/10, all binary collation,
/// decimals 0, flags UNSIGNED and NUM. The signed widths are 4, 6, 9 and 11,
/// so the difference is the character an unsigned column does not spend on a
/// sign.
#[cfg(unix)]
#[test]
fn unsigned_integer_columns_report_their_measured_mysql_shapes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([30; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE u (id INT NOT NULL PRIMARY KEY, a TINYINT UNSIGNED, \
             b SMALLINT UNSIGNED, c MEDIUMINT UNSIGNED, d INT UNSIGNED)",
        )
        .unwrap();
    adapter
        .execute_query("INSERT INTO u (id, a, b, c, d) VALUES (1, 255, 65535, 16777215, 4294967295)")
        .unwrap();

    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT a, b, c, d FROM u")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    for (ordinal, column_type, column_length) in [
        (0, MYSQL_TYPE_TINY, 3),
        (1, MYSQL_TYPE_SHORT, 5),
        (2, MYSQL_TYPE_INT24, 8),
        (3, MYSQL_TYPE_LONG, 10),
    ] {
        let column = &selected.columns[ordinal];
        assert_eq!(column.column_type, column_type, "column {ordinal}");
        assert_eq!(column.column_length, column_length, "column {ordinal}");
        assert_eq!(column.decimals, 0, "column {ordinal}");
        assert_eq!(
            column.flags & MYSQL_UNSIGNED_FLAG,
            MYSQL_UNSIGNED_FLAG,
            "column {ordinal}"
        );
    }
    // The top value of each type reads back whole.
    assert_eq!(
        selected.rows,
        vec![vec![
            Some(b"255".to_vec()),
            Some(b"65535".to_vec()),
            Some(b"16777215".to_vec()),
            Some(b"4294967295".to_vec()),
        ]]
    );

    // Measured on MySQL 8.4.11: SHOW COLUMNS prints the sign as a second
    // lowercase word.
    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM u").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    let types = columns
        .rows
        .iter()
        .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        vec![
            "int",
            "tinyint unsigned",
            "smallint unsigned",
            "mediumint unsigned",
            "int unsigned",
        ]
    );

    // Measured on MySQL 8.4.11: one past the top value answers 1264, and so
    // does a negative.
    assert!(adapter
        .execute_query("INSERT INTO u (id, d) VALUES (2, 4294967296)")
        .is_err());
    assert!(adapter
        .execute_query("INSERT INTO u (id, d) VALUES (3, -1)")
        .is_err());

    // The reason the type matters at all: MySQL schemas spell an
    // auto-increment primary key `INT UNSIGNED`. Measured on MySQL 8.4.11:
    // two inserts answer 1 and 2, and LAST_INSERT_ID reports the first of the
    // pair.
    adapter
        .execute_query("CREATE TABLE ai (id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO ai (v) VALUES (1), (2)")
        .unwrap();
    let CommandExecutionResult::ResultSet(numbered) =
        adapter.execute_query("SELECT id, v FROM ai ORDER BY id").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        numbered.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"1".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"2".to_vec())],
        ]
    );
    assert_eq!(
        numbered.columns[0].flags & (MYSQL_UNSIGNED_FLAG | MYSQL_AUTO_INCREMENT_FLAG),
        MYSQL_UNSIGNED_FLAG | MYSQL_AUTO_INCREMENT_FLAG
    );

    // Moving the counter past a key above i32::MAX but inside INT UNSIGNED's
    // range. The ceiling the allocator is held to is the column's own type, so
    // this is allowed here and refused on a signed INT, where 3000000000 is
    // out of range.
    adapter
        .execute_query("UPDATE ai SET id = 3000000000 WHERE id = 2")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE si (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)")
        .unwrap();
    adapter.execute_query("INSERT INTO si (v) VALUES (1)").unwrap();
    assert!(adapter
        .execute_query("UPDATE si SET id = 3000000000 WHERE id = 1")
        .is_err());
}

/// MySQL's EXCEPT and INTERSECT arrived in 8.0.31. Measured on MySQL 8.4.11
/// over (1),(2),(3) against (2),(3),(4): EXCEPT answers 1, INTERSECT answers 2
/// and 3. Their result columns name no table, exactly as a UNION's do not.
#[cfg(unix)]
#[test]
fn except_and_intersect_answer_the_rows_mysql_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([29; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE ea (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE eb (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO ea (id) VALUES (1), (2), (3)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO eb (id) VALUES (2), (3), (4)")
        .unwrap();

    for (sql, expected) in [
        (
            "SELECT id FROM ea EXCEPT SELECT id FROM eb ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM ea INTERSECT SELECT id FROM eb ORDER BY id",
            vec!["2", "3"],
        ),
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("SELECT must return a result set: {sql}");
        };
        let ids = result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, expected, "{sql}");
        // Measured on MySQL 8.4.11: a compound query's result column names no
        // table, the same as a UNION's.
        assert!(result.columns[0].table.is_empty(), "{sql}");
        assert!(result.columns[0].original_table.is_empty(), "{sql}");
    }

    // Both branches are authorized, not just the first.
    assert!(adapter
        .execute_query("SELECT id FROM ea EXCEPT SELECT id FROM nosuch")
        .is_err());

    // The ALL forms keep duplicates, which the engine cannot spell.
    assert!(adapter
        .execute_query("SELECT id FROM ea EXCEPT ALL SELECT id FROM eb")
        .is_err());
}

/// MySQL compares an `IN` list member by member under the column's own
/// collation. Measured on MySQL 8.4.11 over rows (1,'b'), (2,'A'), (3,'c'):
/// `name IN ('a','C')` answers 2 and 3, `id IN (1, NULL)` answers 1, and
/// `id NOT IN (1, NULL)` answers nothing.
#[cfg(unix)]
#[test]
fn an_in_list_matches_each_member_the_way_mysql_does() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([27; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, name) VALUES (1, 'b'), (2, 'A'), (3, 'c')")
        .unwrap();

    for (sql, expected) in [
        ("SELECT id FROM t WHERE name IN ('a', 'C') ORDER BY id", vec!["2", "3"]),
        ("SELECT id FROM t WHERE id IN (1, 3) ORDER BY id", vec!["1", "3"]),
        ("SELECT id FROM t WHERE id IN (1, NULL)", vec!["1"]),
        ("SELECT id FROM t WHERE id NOT IN (1, NULL)", vec![]),
        ("SELECT id FROM t WHERE id NOT IN (1) ORDER BY id", vec!["2", "3"]),
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("SELECT must return a result set: {sql}");
        };
        let ids = result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, expected, "{sql}");
    }

    // A member has to fit the column, the same way the right side of a `=`
    // does, so an integer list over a text column is refused.
    assert!(adapter
        .execute_query("SELECT id FROM t WHERE name IN (1, 2)")
        .is_err());
}

/// MySQL reads a bare positive integer in `ORDER BY` as the nth projected
/// column. Measured on MySQL 8.4.11: with rows `(1,'b'), (2,'A'), (3,'c')`,
/// `SELECT id, name FROM t ORDER BY 2` answers 2, 1, 3 — the default collation
/// ignores case, so `A` sorts before `b`.
#[cfg(unix)]
#[test]
fn an_order_by_ordinal_sorts_by_the_column_it_names() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([26; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, name) VALUES (1, 'b'), (2, 'A'), (3, 'c')")
        .unwrap();

    let CommandExecutionResult::ResultSet(by_ordinal) = adapter
        .execute_query("SELECT id, name FROM t ORDER BY 2")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        by_ordinal.rows,
        vec![
            vec![Some(b"2".to_vec()), Some(b"A".to_vec())],
            vec![Some(b"1".to_vec()), Some(b"b".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"c".to_vec())],
        ]
    );

    // Written out, the same order: an ordinal must not be a second, blunter
    // way of ordering.
    let CommandExecutionResult::ResultSet(by_name) = adapter
        .execute_query("SELECT id, name FROM t ORDER BY name")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(by_ordinal.rows, by_name.rows);

    let CommandExecutionResult::ResultSet(descending) = adapter
        .execute_query("SELECT id, name FROM t ORDER BY 2 DESC")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        descending.rows,
        vec![
            vec![Some(b"3".to_vec()), Some(b"c".to_vec())],
            vec![Some(b"1".to_vec()), Some(b"b".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"A".to_vec())],
        ]
    );

    // MySQL answers 1054 for an ordinal past the projection; this refuses it.
    assert!(adapter
        .execute_query("SELECT id, name FROM t ORDER BY 3")
        .is_err());
}

/// A FLOAT is binary32 in MySQL and binary64 in the engine, so the value
/// is rounded to binary32 wherever a client can see it. Its metadata is
/// measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn a_float_reads_back_as_the_binary32_mysql_would_have_kept() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([25; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE f (id INT NOT NULL PRIMARY KEY, ratio FLOAT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO f (id, ratio) VALUES (1, 0.1), (2, 1.5)")
        .unwrap();

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE f").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert!(String::from_utf8(created.rows[0][1].clone().unwrap())
        .unwrap()
        .contains("`ratio` float DEFAULT NULL"));

    // Measured: a FLOAT column reports 12 where a DOUBLE reports 22.
    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT ratio FROM f ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(selected.columns[0].column_type, MYSQL_TYPE_FLOAT);
    assert_eq!(selected.columns[0].column_length, 12);
    assert_eq!(selected.columns[0].decimals, NOT_FIXED_DECIMALS);
    // The engine holds 0.1 as a binary64; rounding to binary32 is what
    // makes it read back as MySQL writes it rather than as 0.100000001.
    assert_eq!(
        selected.rows,
        vec![vec![Some(b"0.1".to_vec())], vec![Some(b"1.5".to_vec())]]
    );

    // The binary protocol carries four bytes for it, not eight.
    let prepared = adapter
        .execute_stmt_prepare("SELECT ratio FROM f WHERE id = 2")
        .unwrap();
    assert_eq!(prepared.columns[0].column_type, MYSQL_TYPE_FLOAT);
    let executed = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(executed.rows, vec![vec![BinaryResultValue::Real(1.5)]]);
}

/// A join reads two tables, and every result column has to say which one
/// it came from.
#[cfg(unix)]
#[test]
fn a_join_reports_each_column_against_its_own_table() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([24; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE owners (id INT NOT NULL PRIMARY KEY, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE pets (id INT NOT NULL PRIMARY KEY, owner_id INT, age INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO owners (id, name) VALUES (1, 'ann'), (2, 'bob')")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO pets (id, owner_id, age) VALUES (10, 1, 3), (11, 1, 5), (12, 2, 7)",
        )
        .unwrap();

    let CommandExecutionResult::ResultSet(joined) = adapter
        .execute_query(
            "SELECT o.name, p.age FROM owners AS o JOIN pets AS p ON o.id = p.owner_id ORDER BY p.age",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        joined.rows,
        vec![
            vec![Some(b"ann".to_vec()), Some(b"3".to_vec())],
            vec![Some(b"ann".to_vec()), Some(b"5".to_vec())],
            vec![Some(b"bob".to_vec()), Some(b"7".to_vec())],
        ]
    );
    // Each column carries the table it actually came from, under the alias
    // the statement used, which is what MySQL reports.
    assert_eq!(
        joined
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.column_type
            ))
            .collect::<Vec<_>>(),
        vec![
            ("name", "o", "owners", MYSQL_TYPE_VAR_STRING),
            ("age", "p", "pets", MYSQL_TYPE_LONG),
        ]
    );

    // An aggregate over a joined column finds it in whichever table has it.
    let CommandExecutionResult::ResultSet(grouped) = adapter
        .execute_query(
            "SELECT o.name, MAX(age) FROM owners AS o JOIN pets AS p ON o.id = p.owner_id GROUP BY name ORDER BY name",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(grouped.columns[1].column_type, MYSQL_TYPE_LONG);
    assert_eq!(
        grouped.rows,
        vec![
            vec![Some(b"ann".to_vec()), Some(b"5".to_vec())],
            vec![Some(b"bob".to_vec()), Some(b"7".to_vec())],
        ]
    );

    // A name both tables carry cannot be resolved from a name alone.
    assert_eq!(
        adapter.execute_query(
            "SELECT o.name, MAX(id) FROM owners AS o JOIN pets AS p ON o.id = p.owner_id GROUP BY name"
        ),
        Err(FrontendErrorKind::Unsupported)
    );

    // An outer join keeps the rows with no match, and the side that can go
    // missing loses its NOT NULL flag while keeping its key flags —
    // measured on MySQL 8.4.11.
    adapter
        .execute_query("INSERT INTO owners (id, name) VALUES (3, 'cat')")
        .unwrap();
    let CommandExecutionResult::ResultSet(outer) = adapter
        .execute_query(
            "SELECT o.id, p.id FROM owners AS o LEFT JOIN pets AS p ON o.id = p.owner_id ORDER BY o.id, p.id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        outer.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"10".to_vec())],
            vec![Some(b"1".to_vec()), Some(b"11".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"12".to_vec())],
            vec![Some(b"3".to_vec()), None],
        ]
    );
    assert_eq!(
        outer.columns[0].flags & MYSQL_NOT_NULL_FLAG,
        MYSQL_NOT_NULL_FLAG
    );
    assert_eq!(outer.columns[1].flags & MYSQL_NOT_NULL_FLAG, 0);
    assert_eq!(
        outer.columns[1].flags & MYSQL_PRI_KEY_FLAG,
        MYSQL_PRI_KEY_FLAG
    );

    // A RIGHT JOIN is the mirror image: the first table is the one that
    // can go missing.
    let CommandExecutionResult::ResultSet(mirrored) = adapter
        .execute_query(
            "SELECT p.id, o.id FROM pets AS p RIGHT JOIN owners AS o ON o.id = p.owner_id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(mirrored.columns[0].flags & MYSQL_NOT_NULL_FLAG, 0);
    assert_eq!(
        mirrored.columns[1].flags & MYSQL_NOT_NULL_FLAG,
        MYSQL_NOT_NULL_FLAG
    );
}

/// A `USING` join matches on the named column and reports it once, against
/// the side of the join that cannot go missing.
#[cfg(unix)]
#[test]
fn a_using_join_merges_the_named_column() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([25; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE a (id INT NOT NULL, x VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE b (id INT NOT NULL, y VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO a (id, x) VALUES (1, 'p'), (2, 'q')")
        .unwrap();
    adapter
        .execute_query("INSERT INTO b (id, y) VALUES (1, 'r'), (3, 's')")
        .unwrap();

    let CommandExecutionResult::ResultSet(inner) = adapter
        .execute_query("SELECT id, a.x, b.y FROM a JOIN b USING (id)")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        inner.rows,
        vec![vec![
            Some(b"1".to_vec()),
            Some(b"p".to_vec()),
            Some(b"r".to_vec()),
        ]]
    );
    // Measured on MySQL 8.4.11: the merged column is reported once, against
    // the left table, and keeps its NOT NULL.
    assert_eq!(
        inner
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.original_table.as_str()))
            .collect::<Vec<_>>(),
        vec![("id", "a"), ("x", "a"), ("y", "b")]
    );
    assert_eq!(
        inner.columns[0].flags & MYSQL_NOT_NULL_FLAG,
        MYSQL_NOT_NULL_FLAG
    );

    let CommandExecutionResult::ResultSet(outer) = adapter
        .execute_query("SELECT id, a.x, b.y FROM a LEFT JOIN b USING (id)")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        outer.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"p".to_vec()),
                Some(b"r".to_vec()),
            ],
            vec![Some(b"2".to_vec()), Some(b"q".to_vec()), None],
        ]
    );
    assert_eq!(outer.columns[0].original_table, "a");
    assert_eq!(
        outer.columns[0].flags & MYSQL_NOT_NULL_FLAG,
        MYSQL_NOT_NULL_FLAG
    );

    // Measured on MySQL 8.4.11: a RIGHT JOIN reports the merged column
    // against the right table, the side that keeps every row.
    let CommandExecutionResult::ResultSet(mirrored) = adapter
        .execute_query("SELECT id, a.x, b.y FROM a RIGHT JOIN b USING (id)")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        mirrored.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"p".to_vec()),
                Some(b"r".to_vec()),
            ],
            vec![Some(b"3".to_vec()), None, Some(b"s".to_vec())],
        ]
    );
    assert_eq!(mirrored.columns[0].original_table, "b");

    // A name no `USING` merges still has to say which table it came from.
    assert_eq!(
        adapter.execute_query("SELECT id, a.x, b.y FROM a JOIN b ON a.id = b.id"),
        Err(FrontendErrorKind::Syntax)
    );
    assert_eq!(
        adapter.execute_query("SELECT id, a.x FROM a JOIN b USING (y)"),
        Err(FrontendErrorKind::Syntax)
    );
}

/// A GROUP BY groups by whole columns and is held to ONLY_FULL_GROUP_BY,
/// which is in MySQL 8.4's default sql_mode.
#[cfg(unix)]
#[test]
fn group_by_groups_and_refuses_a_projection_outside_the_grouping() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([23; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE g (id INT NOT NULL PRIMARY KEY, team INT, score INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO g (id, team, score) VALUES (1, 7, 10), (2, 7, 30), (3, 9, 50)")
        .unwrap();

    let CommandExecutionResult::ResultSet(grouped) = adapter
        .execute_query("SELECT team, COUNT(*), MAX(score) FROM g GROUP BY team ORDER BY team")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Some(b"7".to_vec()),
                Some(b"2".to_vec()),
                Some(b"30".to_vec())
            ],
            vec![
                Some(b"9".to_vec()),
                Some(b"1".to_vec()),
                Some(b"50".to_vec())
            ],
        ]
    );
    // The grouping column keeps its own metadata and the aggregates keep
    // theirs, exactly as they do without a GROUP BY.
    assert_eq!(
        grouped
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.column_type, column.flags))
            .collect::<Vec<_>>(),
        vec![
            ("team", MYSQL_TYPE_LONG, MYSQL_NUM_FLAG),
            (
                "COUNT(*)",
                MYSQL_TYPE_LONGLONG,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "MAX(score)",
                MYSQL_TYPE_LONG,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );

    // A grouped query orders by what it selected, aggregate or column, and
    // filters the groups with HAVING.
    let CommandExecutionResult::ResultSet(ranked) = adapter
        .execute_query(
            "SELECT team, COUNT(*) FROM g GROUP BY team HAVING COUNT(*) > 1 ORDER BY COUNT(*) DESC",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        ranked.rows,
        vec![vec![Some(b"7".to_vec()), Some(b"2".to_vec())]]
    );
    let CommandExecutionResult::ResultSet(by_total) = adapter
        .execute_query("SELECT team FROM g GROUP BY team HAVING SUM(score) > 45 ORDER BY team")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    // Team 7 totals 40 and team 9 totals 50.
    assert_eq!(by_total.rows, vec![vec![Some(b"9".to_vec())]]);

    // ONLY_FULL_GROUP_BY: `score` lands in one row of several, which MySQL
    // answers 1055 for.
    assert!(adapter
        .execute_query("SELECT team, score FROM g GROUP BY team")
        .is_err());
    assert!(adapter
        .execute_query("SELECT * FROM g GROUP BY team")
        .is_err());
}

/// DISTINCT compares the projected values, which the two engines agree
/// about for numbers and disagree about for text, because of the collation.
#[cfg(unix)]
#[test]
fn distinct_drops_repeats_and_keeps_the_column_metadata() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([22; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE d (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO d (id, n, name) VALUES (1, 7, 'abc'), (2, 7, 'ABC'), (3, 9, 'zz')",
        )
        .unwrap();

    let CommandExecutionResult::ResultSet(numbers) = adapter
        .execute_query("SELECT DISTINCT n FROM d ORDER BY n")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        numbers.rows,
        vec![vec![Some(b"7".to_vec())], vec![Some(b"9".to_vec())]]
    );
    // The projection's metadata is the column's, DISTINCT or not.
    assert_eq!(numbers.columns[0].column_type, MYSQL_TYPE_LONG);
    assert_eq!(numbers.columns[0].original_name, "n");

    // A `?` against a text column binds a string, because the statement is
    // rendered with the collation once the column's type is known.
    let prepared = adapter
        .execute_stmt_prepare("SELECT id FROM d WHERE name = ? ORDER BY id")
        .unwrap();
    let bound = prepared_result_set(
        adapter
            .execute_stmt_execute(
                prepared.statement_id,
                &[0, 1, MYSQL_TYPE_VAR_STRING, 0, 3, b'A', b'B', b'C'],
            )
            .unwrap(),
    );
    assert_eq!(
        bound.rows,
        vec![
            vec![BinaryResultValue::Integer(1)],
            vec![BinaryResultValue::Integer(2)],
        ]
    );

    // MySQL orders text without regard to case, so 'ABC' sorts beside
    // 'abc' rather than before every lowercase name.
    let CommandExecutionResult::ResultSet(ordered) = adapter
        .execute_query("SELECT name FROM d ORDER BY name, id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        ordered
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["abc", "ABC", "zz"]
    );

    // MySQL's collation collapses 'abc' and 'ABC' into one row; the engine
    // compares them byte for byte and keeps both.
    let CommandExecutionResult::ResultSet(text) = adapter
        .execute_query("SELECT DISTINCT name FROM d")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(text.rows.len(), 3);
}

/// Integer arithmetic reports a type worked out from its operands, all of
/// it measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn arithmetic_reports_the_shape_its_operands_give_it() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([21; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE a (req INT NOT NULL PRIMARY KEY, opt INT, big BIGINT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO a (req, opt, big) VALUES (1, 2, 20)")
        .unwrap();

    for (sql, name, column_type, length, decimals, flags) in [
        (
            "SELECT 1+1 FROM a",
            "1+1",
            MYSQL_TYPE_LONGLONG,
            3,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT req + 1 FROM a",
            "req + 1",
            MYSQL_TYPE_LONGLONG,
            12,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
        ),
        // A nullable operand makes the answer nullable.
        (
            "SELECT opt + 1 FROM a",
            "opt + 1",
            MYSQL_TYPE_LONGLONG,
            12,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT req - big FROM a",
            "req - big",
            MYSQL_TYPE_LONGLONG,
            21,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT req * 1000000 FROM a",
            "req * 1000000",
            MYSQL_TYPE_LONGLONG,
            18,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
        ),
        // A division is decimal and is never NOT NULL, because dividing by
        // zero answers NULL.
        (
            "SELECT 3/2 FROM a",
            "3/2",
            MYSQL_TYPE_NEWDECIMAL,
            7,
            4,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT req / 2 FROM a",
            "req / 2",
            MYSQL_TYPE_NEWDECIMAL,
            16,
            4,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        let column = &result.columns[0];
        assert_eq!(
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags
            ),
            (name, column_type, length, decimals, flags),
            "{sql}"
        );
    }

    // MySQL's division is decimal with a scale of four, so this answers
    // 1.5000 rather than the 1 an integer division would give.
    let CommandExecutionResult::ResultSet(divided) =
        adapter.execute_query("SELECT 3/2 FROM a").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(divided.rows, vec![vec![Some(b"1.5000".to_vec())]]);

    // Measured on MySQL 8.4.11: an integer result that leaves BIGINT's
    // range answers 1690 / 22003. The engine turns the same sum into a
    // float, which is how this sees it.
    adapter
        .execute_query("INSERT INTO a (req, opt, big) VALUES (2, 2, 9223372036854775807)")
        .unwrap();
    assert_eq!(
        adapter.execute_query("SELECT big + big FROM a"),
        Err(FrontendErrorKind::NumericOverflow)
    );
    let prepared = adapter
        .execute_stmt_prepare("SELECT big + big FROM a")
        .unwrap();
    assert_eq!(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .map(|_| ()),
        Err(FrontendErrorKind::NumericOverflow)
    );

    // An expression over literals alone reads no table, so it must not
    // need one either.
    let CommandExecutionResult::ResultSet(bare) = adapter.execute_query("SELECT 1+1").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(bare.columns[0].name, "1+1");
    assert_eq!(bare.columns[0].column_length, 3);
    assert_eq!(bare.rows, vec![vec![Some(b"2".to_vec())]]);

    // A column needs the table, so an expression naming one outside a FROM
    // is refused rather than answered with a made-up width.
    assert!(adapter.execute_query("SELECT req + 1").is_err());
}

/// Each aggregate answers a type worked out from its argument column, and
/// is nullable whatever the column is because an empty table gives NULL.
/// All of it is measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn an_aggregate_reports_the_column_it_named() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([20; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE m (id INT NOT NULL PRIMARY KEY, big BIGINT, price DECIMAL(10,2), rate DOUBLE, label VARCHAR(8))",
        )
        .unwrap();

    // Empty first, because that is where the nullability shows.
    let CommandExecutionResult::ResultSet(empty) = adapter
        .execute_query("SELECT MIN(id), MAX(big) FROM m")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        empty
            .columns
            .iter()
            .map(|column| (
                column.name.clone(),
                column.column_type,
                column.column_length,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "MIN(id)".to_owned(),
                MYSQL_TYPE_LONG,
                11,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "MAX(big)".to_owned(),
                MYSQL_TYPE_LONGLONG,
                20,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );
    assert_eq!(empty.rows, vec![vec![None, None]]);

    // SUM widens the argument's decimal precision by 22 and keeps its
    // scale; AVG widens precision by 4 and scale by 4. Over a DOUBLE both
    // answer DOUBLE. Every length here is measured.
    let CommandExecutionResult::ResultSet(shapes) = adapter
        .execute_query(
            "SELECT SUM(id), SUM(big), SUM(price), AVG(id), AVG(price), SUM(rate) FROM m",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        shapes
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.decimals))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_NEWDECIMAL, 33, 0),
            (MYSQL_TYPE_NEWDECIMAL, 42, 0),
            (MYSQL_TYPE_NEWDECIMAL, 34, 2),
            (MYSQL_TYPE_NEWDECIMAL, 16, 4),
            (MYSQL_TYPE_NEWDECIMAL, 16, 6),
            (MYSQL_TYPE_DOUBLE, 23, 31),
        ]
    );

    // MySQL sums a text column by coercing it, which this has not measured.
    assert_eq!(
        adapter.execute_query("SELECT SUM(label) FROM m"),
        Err(FrontendErrorKind::Unsupported)
    );

    adapter
        .execute_query("INSERT INTO m (id, big) VALUES (3, 30), (1, 10)")
        .unwrap();
    let CommandExecutionResult::ResultSet(summed) =
        adapter.execute_query("SELECT SUM(id) FROM m").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(summed.rows, vec![vec![Some(b"4".to_vec())]]);
    // An AVG answers a DECIMAL with a scale of four, and a decimal is
    // rendered at the scale it declares, which is what MySQL answers.
    let CommandExecutionResult::ResultSet(averaged) =
        adapter.execute_query("SELECT AVG(id) FROM m").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(averaged.rows, vec![vec![Some(b"2.0000".to_vec())]]);
    let CommandExecutionResult::ResultSet(filled) = adapter
        .execute_query("SELECT MIN(id), MAX(big) FROM m")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        filled.rows,
        vec![vec![Some(b"1".to_vec()), Some(b"30".to_vec())]]
    );

    // The binary protocol is why this had to wait for a type: it encodes
    // each value by the column type it announced, so MYSQL_TYPE_NULL over
    // a real integer would put the wrong bytes on the wire.
    let prepared = adapter
        .execute_stmt_prepare("SELECT MAX(id) FROM m")
        .unwrap();
    assert_eq!(prepared.columns[0].name, "MAX(id)");
    assert_eq!(prepared.columns[0].column_type, MYSQL_TYPE_LONG);
    assert_eq!(prepared.columns[0].column_length, 11);
    assert_eq!(
        prepared.columns[0].flags,
        MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
    let executed = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(executed.columns[0].column_type, MYSQL_TYPE_LONG);
    assert_eq!(executed.rows, vec![vec![BinaryResultValue::Integer(3)]]);
}

/// MySQL's default collation ignores both case and accents. A comparison
/// asks the engine for NOCASE and a LIKE needs nothing, and both reproduce
/// the case half and not the accent half; measured on 8.4.11.
#[cfg(unix)]
#[test]
fn a_text_where_ignores_case_but_not_accents() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([19; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE people (id INT NOT NULL PRIMARY KEY, name VARCHAR(32))")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO people (id, name) VALUES (1, 'abc'), (2, 'ABC'), (3, 'Abc'), (4, 'B'), (5, 'cafe'), (6, 'caf\u{e9}')",
        )
        .unwrap();

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    // Measured: MySQL answers 1, 2 and 3 here, where byte order answers 2.
    assert_eq!(
        ids("SELECT id FROM people WHERE name = 'ABC'"),
        ["1", "2", "3"]
    );
    // Ordering goes through the same collation as equality. Measured:
    // 'B' > 'a' is true in MySQL and false byte for byte, so byte order
    // would answer 1 alone where MySQL and NOCASE answer all four.
    assert_eq!(
        ids("SELECT id FROM people WHERE name > 'a' AND name < 'ca'"),
        ["1", "2", "3", "4"]
    );
    // Measured: MySQL answers 5 and 6, because its collation ignores the
    // accent too. NOCASE does not, so this answers 5 alone.
    assert_eq!(ids("SELECT id FROM people WHERE name = 'cafe'"), ["5"]);
    // LIKE needs no collation of its own: the engine already matches it
    // without regard to ASCII case, which is what MySQL's default
    // collation does.
    assert_eq!(
        ids("SELECT id FROM people WHERE name LIKE 'A%'"),
        ["1", "2", "3"]
    );
    assert_eq!(
        ids("SELECT id FROM people WHERE name NOT LIKE '%c'"),
        ["4", "5", "6"]
    );
    assert_eq!(
        ids("SELECT id FROM people WHERE name LIKE '_bc'"),
        ["1", "2", "3"]
    );

    // An UPDATE and a DELETE go through the same renderer and the same
    // rule, so the rows a WHERE names cannot depend on the statement.
    assert_eq!(ids("SELECT id FROM people WHERE name = 'b'"), ["4"]);
    adapter
        .execute_query("UPDATE people SET name = 'done' WHERE name = 'b'")
        .unwrap();
    adapter
        .execute_query("DELETE FROM people WHERE name = 'ABC'")
        .unwrap();
    let CommandExecutionResult::ResultSet(left) = adapter
        .execute_query("SELECT id FROM people WHERE name = 'DONE'")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(left.rows, vec![vec![Some(b"4".to_vec())]]);

    // A string still cannot meet an integer column, which MySQL answers by
    // coercing the string.
    assert_eq!(
        adapter.execute_query("SELECT id FROM people WHERE id = 'abc'"),
        Err(FrontendErrorKind::Unsupported)
    );
    // A backslash means an escape in MySQL and a literal byte in the
    // engine, so a pattern carrying one is refused rather than mismatched.
    assert_eq!(
        adapter.execute_query("SELECT id FROM people WHERE name LIKE 'a\\%'"),
        Err(FrontendErrorKind::Syntax)
    );
}

fn adapter() -> MySqlCommandAdapter {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    static NEXT_DATABASE: AtomicUsize = AtomicUsize::new(0);
    let path = format!(
        "mysql-server-frontend-adapter-{}.db",
        NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let file = io.open_file(&path, OpenFlags::Create, true).unwrap();
    let database = Database::open(
        io,
        &path,
        OpenOptions::new(Arc::new(MySqlDialect))
            .storage(Arc::new(DatabaseFile::new(file)))
            .flags(OpenFlags::Create)
            .db_opts(DatabaseOpts::new().with_vacuum(true).with_views(true)),
    )
    .unwrap();
    let inner = database.connect().unwrap();
    let frontend = MySqlConnection::new(inner.clone(), binary_context()).unwrap();
    frontend
        .execute("CREATE TABLE `result_values` (`id` INTEGER, `payload` BLOB)")
        .unwrap();
    inner
        .execute("INSERT INTO result_values VALUES (1, X'00ff'), (2, NULL)")
        .unwrap();
    frontend
        .execute("CREATE TABLE `many_rows` (`id` INTEGER)")
        .unwrap();
    frontend
        .execute("CREATE TABLE `wide_values` (`left_value` BLOB, `right_value` BLOB)")
        .unwrap();
    inner
        .execute(
            "WITH RECURSIVE ids(id) AS (VALUES(1) UNION ALL SELECT id + 1 FROM ids WHERE id <= 4096) INSERT INTO many_rows SELECT id FROM ids",
        )
        .unwrap();
    inner
        .execute("INSERT INTO wide_values VALUES (zeroblob(2048), zeroblob(2048))")
        .unwrap();
    MySqlCommandAdapter::new(frontend)
}

#[cfg(unix)]
fn auto_increment_adapter() -> (tempfile::TempDir, MySqlCommandAdapter) {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    catalog.create("reset").unwrap();
    let mut session = catalog.new_session(binary_context());
    session.select_database("reset").unwrap();
    let connection = session.connection().unwrap().clone();
    connection
        .execute(
            "CREATE TABLE generated_records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
        )
        .unwrap();
    drop(session);
    drop(catalog);
    (directory, MySqlCommandAdapter::new(connection))
}

#[test]
fn bootstrap_settings_round_positive_idle_durations_up_to_seconds() {
    assert_eq!(
        MySqlBootstrapSettings::new(4096, Duration::from_secs(7)).wait_timeout_seconds(),
        7
    );
    assert_eq!(
        MySqlBootstrapSettings::new(4096, Duration::from_millis(500)).wait_timeout_seconds(),
        1
    );
    assert_eq!(
        MySqlBootstrapSettings::new(4096, Duration::from_millis(1500)).wait_timeout_seconds(),
        2
    );
}

#[test]
fn direct_adapter_serves_the_typed_driver_bootstrap_result() {
    let mut adapter =
        adapter().with_bootstrap_settings(MAX_COMMAND_PAYLOAD_LENGTH, Duration::from_millis(1500));
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT @@max_allowed_packet,@@wait_timeout")
        .unwrap()
    else {
        panic!("driver bootstrap query must produce a result set");
    };

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.column_type))
            .collect::<Vec<_>>(),
        vec![
            ("@@max_allowed_packet", MYSQL_TYPE_LONGLONG),
            ("@@wait_timeout", MYSQL_TYPE_LONGLONG),
        ]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Some(MAX_COMMAND_PAYLOAD_LENGTH.to_string().into_bytes()),
            Some(b"2".to_vec()),
        ]]
    );
    assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
}

#[test]
fn unknown_system_variables_do_not_enter_the_bootstrap_path() {
    let mut adapter = adapter();
    assert_eq!(
        adapter.execute_query("SELECT @@socket,@@wait_timeout"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert!(matches!(
        adapter.execute_query("SELECT '@@socket'"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
}

#[test]
fn direct_adapter_orders_and_limits_text_and_prepared_results() {
    let mut adapter = adapter();
    adapter
        .execute_query("CREATE TABLE records (id INT, label TEXT)")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO records (id, label) VALUES (3, 'b'), (1, 'A'), (2, 'a'), (4, NULL)",
        )
        .unwrap();
    for sql in [
        "SELECT id AS ranked, label FROM records ORDER BY label ASC, id DESC LIMIT 2 OFFSET 1",
        "SELECT id AS ranked, label FROM records ORDER BY label ASC, id DESC LIMIT 1, 2",
    ] {
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(sql).unwrap() else {
            panic!("ordered SELECT must return rows");
        };
        // NULL sorts first, and MySQL's collation makes 'A' and 'a' equal,
        // so `id DESC` breaks their tie: the whole order is 4, 2, 1, 3.
        assert_eq!(
            result.rows,
            vec![
                vec![Some(b"2".to_vec()), Some(b"a".to_vec())],
                vec![Some(b"1".to_vec()), Some(b"A".to_vec())]
            ]
        );
        assert_eq!(result.columns[0].name, "ranked");
        assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONG);
    }
    let prepared = adapter
        .execute_stmt_prepare(
            "SELECT id AS ranked, ? AS marker FROM records ORDER BY ranked DESC LIMIT 2",
        )
        .unwrap();
    assert_eq!(prepared.parameters.len(), 1);
    assert_eq!(prepared.columns[0].name, "ranked");
    assert_eq!(prepared.columns[0].column_type, MYSQL_TYPE_LONG);
    let result = adapter
        .execute_stmt_execute(
            prepared.statement_id,
            &[0, 1, MYSQL_TYPE_VAR_STRING, 0, 1, b'x'],
        )
        .unwrap();
    assert_eq!(
        prepared_result_set(result).rows,
        vec![
            vec![
                BinaryResultValue::Integer(4),
                BinaryResultValue::Text("x".into())
            ],
            vec![
                BinaryResultValue::Integer(3),
                BinaryResultValue::Text("x".into())
            ]
        ]
    );
}

#[test]
fn direct_adapter_prepares_and_retains_checked_selects() {
    let mut adapter = adapter();

    let first = adapter.execute_stmt_prepare("SELECT ? AS value").unwrap();
    let second = adapter.execute_stmt_prepare("SELECT 1 AS one").unwrap();

    assert_eq!((first.statement_id, second.statement_id), (1, 2));
    assert_eq!(
        first
            .parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect::<Vec<_>>(),
        ["?1"]
    );
    assert_eq!(first.columns.len(), 1);
    assert_eq!(first.columns[0].name, "value");
    // A marker starts generic, the way MySQL 8.4.11 answers a fresh
    // `SELECT ? AS value` before anything has been bound.
    assert_eq!(first.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
}

#[test]
fn direct_adapter_maps_invalid_and_unsupported_prepares() {
    let mut adapter = adapter();

    assert_eq!(
        adapter.execute_stmt_prepare("SELECT FROM"),
        Err(FrontendErrorKind::Syntax)
    );
    let delete = adapter
        .execute_stmt_prepare("DELETE FROM result_values")
        .unwrap();
    assert!(delete.columns.is_empty());
}

#[test]
fn direct_adapter_closes_and_resets_only_known_statements() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();

    assert_eq!(adapter.execute_stmt_reset(prepared.statement_id), Ok(()));
    adapter.execute_stmt_close(prepared.statement_id);
    assert_eq!(
        adapter.execute_stmt_reset(prepared.statement_id),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );
    adapter.execute_stmt_close(prepared.statement_id);
}

#[cfg(unix)]
#[test]
fn direct_adapter_reset_rolls_back_and_clears_session_state() {
    let (_directory, mut adapter) = auto_increment_adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    let prepared_result = adapter
        .execute_stmt_execute(
            prepared.statement_id,
            &[0, 1, MYSQL_TYPE_VAR_STRING, 0, 1, b'x'],
        )
        .unwrap();
    assert_eq!(
        prepared_result_set(prepared_result).rows,
        [vec![BinaryResultValue::Text("x".to_string())]]
    );
    adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"discarded");

    let CommandExecutionResult::Ok(disabled) =
        adapter.execute_query("SET SESSION autocommit = 0").unwrap()
    else {
        panic!("SET autocommit must produce an OK result");
    };
    assert_eq!(disabled.status_flags, 0);
    adapter
        .execute_query("INSERT INTO generated_records (label) VALUES ('discarded')")
        .unwrap();
    assert_eq!(adapter.status_flags(), SERVER_STATUS_IN_TRANS);
    assert_eq!(adapter.connection.last_insert_id(), 1);
    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SELECT LAST_INSERT_ID()").unwrap()
    else {
        panic!("LAST_INSERT_ID must produce a result set");
    };
    assert_eq!(result.rows, [vec![Some(b"1".to_vec())]]);

    adapter.execute_reset_connection().unwrap();

    assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
    assert_eq!(adapter.connection.last_insert_id(), 0);
    assert!(adapter.prepared_types.is_empty());
    assert!(adapter.pending_long_data.values.is_empty());
    assert!(adapter.pending_long_data.errors.is_empty());
    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[0, 0, MYSQL_TYPE_VAR_STRING, 0]),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT id, LAST_INSERT_ID() FROM generated_records")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert!(result.rows.is_empty());
    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SELECT LAST_INSERT_ID()").unwrap()
    else {
        panic!("LAST_INSERT_ID must produce a result set");
    };
    assert_eq!(result.rows, [vec![Some(b"0".to_vec())]]);

    adapter
        .execute_query("INSERT INTO generated_records (label) VALUES ('committed')")
        .unwrap();
    assert_eq!(adapter.connection.last_insert_id(), 2);
    adapter.execute_reset_connection().unwrap();
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT id FROM generated_records")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert_eq!(result.rows, [vec![Some(b"2".to_vec())]]);
}

#[test]
fn direct_adapter_reset_stops_after_a_rollback_failure() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"retained");
    adapter.execute_query("SET SESSION autocommit = 0").unwrap();
    adapter
        .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'discarded')")
        .unwrap();
    adapter.connection.close().unwrap();

    assert!(adapter.execute_reset_connection().is_err());
    assert!(!adapter.connection.session_autocommit());
    assert!(!adapter.connection.is_auto_commit());
    assert!(adapter
        .connection
        .prepared_statement_metadata(prepared.statement_id)
        .is_none());
    assert_eq!(
        adapter
            .pending_long_data
            .values
            .get(&(prepared.statement_id, 0)),
        Some(&b"retained".to_vec())
    );
}

#[test]
fn direct_adapter_executes_binary_parameters_and_reuses_cached_types() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?, ?, ?").unwrap();
    let mut first_payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0, MYSQL_TYPE_VAR_STRING, 0];
    first_payload.extend_from_slice(&[MYSQL_TYPE_BLOB, 0]);
    first_payload.extend_from_slice(&(-7i64).to_le_bytes());
    first_payload.extend_from_slice(&[3, b'A', b'd', b'a']);
    first_payload.extend_from_slice(&[2, 0, 0xff]);

    let first = adapter
        .execute_stmt_execute(prepared.statement_id, &first_payload)
        .unwrap();
    let first = prepared_result_set(first);
    assert_eq!(
        first.rows,
        [vec![
            BinaryResultValue::Integer(-7),
            BinaryResultValue::Text("Ada".to_string()),
            BinaryResultValue::Blob(vec![0, 0xff]),
        ]]
    );
    assert_eq!(
        first
            .columns
            .iter()
            .map(|column| column.column_type)
            .collect::<Vec<_>>(),
        [MYSQL_TYPE_LONGLONG, MYSQL_TYPE_VAR_STRING, MYSQL_TYPE_BLOB]
    );

    let mut second_payload = vec![0, 0];
    second_payload.extend_from_slice(&8i64.to_le_bytes());
    second_payload.extend_from_slice(&[5, b'G', b'r', b'a', b'c', b'e']);
    second_payload.extend_from_slice(&[1, 1]);
    let second = adapter
        .execute_stmt_execute(prepared.statement_id, &second_payload)
        .unwrap();
    let second = prepared_result_set(second);
    assert_eq!(
        second.rows,
        [vec![
            BinaryResultValue::Integer(8),
            BinaryResultValue::Text("Grace".to_string()),
            BinaryResultValue::Blob(vec![1]),
        ]]
    );
    assert_eq!(
        second
            .columns
            .iter()
            .map(|column| column.column_type)
            .collect::<Vec<_>>(),
        [MYSQL_TYPE_LONGLONG, MYSQL_TYPE_VAR_STRING, MYSQL_TYPE_BLOB]
    );
}

#[test]
fn direct_adapter_appends_long_data_and_consumes_it_on_execute() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?, ?").unwrap();
    adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"long ");
    adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"text");
    adapter.execute_stmt_send_long_data(prepared.statement_id, 1, &[0, 0xff]);
    let payload = [0, 1, MYSQL_TYPE_VAR_STRING, 0, MYSQL_TYPE_BLOB, 0];

    let result = adapter
        .execute_stmt_execute(prepared.statement_id, &payload)
        .unwrap();
    assert_eq!(
        prepared_result_set(result).rows,
        [vec![
            BinaryResultValue::Text("long text".to_string()),
            BinaryResultValue::Blob(vec![0, 0xff]),
        ]]
    );

    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[0, 0]),
        Err(FrontendErrorKind::Syntax)
    );
}

#[test]
fn direct_adapter_reset_forgets_long_data_and_send_errors_are_delayed() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"forgotten");
    assert_eq!(adapter.execute_stmt_reset(prepared.statement_id), Ok(()));
    let mut ordinary = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0];
    ordinary.extend_from_slice(&[4, b'k', b'e', b'p', b't']);
    assert_eq!(
        prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &ordinary)
                .unwrap()
        )
        .rows,
        [vec![BinaryResultValue::Text("kept".to_string())]]
    );

    adapter.execute_stmt_send_long_data(prepared.statement_id, 1, b"invalid");
    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[0, 0, 0]),
        Err(FrontendErrorKind::Syntax)
    );
    assert!(adapter.pending_long_data.values.is_empty());
    assert!(adapter.pending_long_data.errors.is_empty());

    adapter.execute_stmt_send_long_data(u32::MAX, 0, b"unknown");
    assert_eq!(
        adapter.execute_stmt_execute(u32::MAX, &[]),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );
    assert!(adapter.pending_long_data.values.is_empty());
    assert!(adapter.pending_long_data.errors.is_empty());
    assert_eq!(adapter.pending_long_data.retained_bytes, 0);
}

#[test]
fn direct_adapter_drops_long_data_for_unknown_statement_flood() {
    let mut adapter = adapter();
    for statement_id in 1..=100_000 {
        adapter.execute_stmt_send_long_data(statement_id, 0, b"unknown");
    }

    assert!(adapter.pending_long_data.values.is_empty());
    assert!(adapter.pending_long_data.errors.is_empty());
    assert_eq!(adapter.pending_long_data.retained_bytes, 0);
    assert_eq!(
        adapter.execute_stmt_execute(100_000, &[]),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );
}

#[test]
fn pending_long_data_limit_fails_without_retaining_the_overflowing_chunk() {
    let mut pending = PendingLongData::default();
    let full = vec![0xaa; MAX_PREPARED_LONG_DATA_BYTES];
    pending.append(1, 0, &full, 1);
    pending.append(1, 0, &[0xbb], 1);
    assert_eq!(pending.retained_bytes, 0);
    let statement = pending.take_statement(1);
    assert_eq!(statement.error, Some(PendingLongDataError::TooLarge));
    assert!(statement.values.is_empty());
    assert_eq!(pending.retained_bytes, 0);
}

#[test]
fn direct_adapter_executes_prepared_insert_update_and_delete_as_ok_results() {
    let mut adapter = adapter();
    let insert = adapter
        .execute_stmt_prepare("INSERT INTO result_values (id, payload) VALUES (?, ?)")
        .unwrap();
    assert!(insert.columns.is_empty());
    let mut insert_payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0, MYSQL_TYPE_BLOB, 0];
    insert_payload.extend_from_slice(&3i64.to_le_bytes());
    insert_payload.extend_from_slice(&[2, 0xaa, 0xbb]);
    assert_eq!(
        adapter
            .execute_stmt_execute(insert.statement_id, &insert_payload)
            .unwrap(),
        PreparedStatementExecutionResult::Ok(CommandOkResult {
            affected_rows: 1,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
            ..CommandOkResult::default()
        })
    );

    let update = adapter
        .execute_stmt_prepare("UPDATE result_values SET payload = ? WHERE TRUE")
        .expect("prepared UPDATE should compile");
    let mut update_payload = vec![0, 1, MYSQL_TYPE_BLOB, 0];
    update_payload.extend_from_slice(&[1, 0xcc]);
    assert!(matches!(
        adapter
            .execute_stmt_execute(update.statement_id, &update_payload)
            .unwrap(),
        PreparedStatementExecutionResult::Ok(CommandOkResult {
            affected_rows: 3,
            ..
        })
    ));

    let delete = adapter
        .execute_stmt_prepare("DELETE FROM result_values WHERE TRUE")
        .unwrap();
    assert!(matches!(
        adapter
            .execute_stmt_execute(delete.statement_id, &[])
            .unwrap(),
        PreparedStatementExecutionResult::Ok(CommandOkResult {
            affected_rows: 3,
            ..
        })
    ));
}

#[test]
fn prepared_result_metadata_matches_unknown_parameter_values() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?, ?, ?, ?").unwrap();
    let mut payload = vec![0, 1];
    payload.extend_from_slice(&[
        MYSQL_TYPE_LONGLONG,
        0,
        MYSQL_TYPE_DOUBLE,
        0,
        MYSQL_TYPE_VAR_STRING,
        0,
        MYSQL_TYPE_BLOB,
        0,
    ]);
    payload.extend_from_slice(&(-7i64).to_le_bytes());
    payload.extend_from_slice(&1.5f64.to_le_bytes());
    payload.extend_from_slice(&[3, b'A', b'd', b'a']);
    payload.extend_from_slice(&[2, 0, 0xff]);

    let result = adapter
        .execute_stmt_execute(prepared.statement_id, &payload)
        .unwrap();
    let result = prepared_result_set(result);

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.column_type)
            .collect::<Vec<_>>(),
        [
            MYSQL_TYPE_LONGLONG,
            MYSQL_TYPE_DOUBLE,
            MYSQL_TYPE_VAR_STRING,
            MYSQL_TYPE_BLOB,
        ]
    );
    assert_eq!(
        result.rows,
        [vec![
            BinaryResultValue::Integer(-7),
            BinaryResultValue::Real(1.5),
            BinaryResultValue::Text("Ada".to_string()),
            BinaryResultValue::Blob(vec![0, 0xff]),
        ]]
    );
}

#[test]
fn a_marker_keeps_the_type_its_first_non_null_value_established() {
    // Measured against MySQL 8.4.11: a marker starts generic, an integer
    // settles it on LONGLONG with its own length and flags, and a later
    // NULL keeps that type rather than returning to generic.
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();

    let generic = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
            .unwrap(),
    );
    assert_eq!(generic.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(generic.columns[0].column_length, 65_532);
    assert_eq!(generic.columns[0].decimals, 31);
    assert_eq!(generic.columns[0].flags, 0);

    let mut integer = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
    integer.extend_from_slice(&7i64.to_le_bytes());
    let typed = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &integer)
            .unwrap(),
    );
    assert_eq!(typed.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(typed.columns[0].column_length, 21);
    assert_eq!(typed.columns[0].decimals, 0);
    assert_eq!(typed.columns[0].flags, MYSQL_BINARY_FLAG);
    assert_eq!(typed.rows, [vec![BinaryResultValue::Integer(7)]]);

    let after_null = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
            .unwrap(),
    );
    assert_eq!(after_null.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(after_null.columns[0].column_length, 21);
    assert_eq!(after_null.rows, [vec![BinaryResultValue::Null]]);
}

#[cfg(unix)]
#[test]
fn a_real_marker_reports_the_double_metadata_mysql_sends() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    let mut real = vec![0, 1, MYSQL_TYPE_DOUBLE, 0];
    real.extend_from_slice(&1.5f64.to_le_bytes());
    let typed = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &real)
            .unwrap(),
    );
    assert_eq!(typed.columns[0].column_type, MYSQL_TYPE_DOUBLE);
    assert_eq!(typed.columns[0].column_length, 23);
    assert_eq!(typed.columns[0].decimals, 31);
    assert_eq!(typed.columns[0].character_set, MYSQL_BINARY_COLLATION);
    assert_eq!(typed.columns[0].flags, MYSQL_BINARY_FLAG);

    let after_null = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
            .unwrap(),
    );
    assert_eq!(after_null.columns[0].column_type, MYSQL_TYPE_DOUBLE);
}

#[cfg(unix)]
#[test]
fn a_prepare_reports_the_same_marker_metadata_an_execute_does() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    let column = &prepared.columns[0];
    assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(column.column_length, 65_532);
    assert_eq!(column.decimals, 31);
    assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
    assert_eq!(column.flags, 0);

    let executed = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
            .unwrap(),
    );
    assert_eq!(executed.columns[0].column_type, column.column_type);
    assert_eq!(executed.columns[0].column_length, column.column_length);
    assert_eq!(executed.columns[0].decimals, column.decimals);
    assert_eq!(executed.columns[0].character_set, column.character_set);
    assert_eq!(executed.columns[0].flags, column.flags);
}

#[test]
fn prepared_result_keeps_known_and_all_null_column_types() {
    let mut adapter = adapter();
    let unknown = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    let null_result = adapter
        .execute_stmt_execute(unknown.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
        .unwrap();
    let null_result = prepared_result_set(null_result);
    // MySQL 8.4.11 answers a marker that has only ever seen NULL with its
    // generic string type, not MYSQL_TYPE_NULL.
    assert_eq!(null_result.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(null_result.columns[0].column_length, 65_532);
    assert_eq!(null_result.columns[0].decimals, 31);
    assert_eq!(null_result.rows, [vec![BinaryResultValue::Null]]);

    let known = adapter
        .execute_stmt_prepare("SELECT id FROM result_values")
        .unwrap();
    let known_result = adapter
        .execute_stmt_execute(known.statement_id, &[])
        .unwrap();
    let known_result = prepared_result_set(known_result);
    assert_eq!(known_result.columns[0].column_type, MYSQL_TYPE_LONG);
    assert_eq!(
        known_result.rows,
        [
            vec![BinaryResultValue::Integer(1)],
            vec![BinaryResultValue::Integer(2)],
        ]
    );
}

#[test]
fn prepared_result_preserves_declared_integer_wire_widths_for_empty_and_null_rows() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute(
            "CREATE TABLE declared_widths (tiny TINYINT, small SMALLINT, int_value INT, integer_value INTEGER, big BIGINT)",
        )
        .unwrap();
    let prepared = adapter
        .execute_stmt_prepare(
            "SELECT tiny, small, int_value, integer_value, big FROM declared_widths",
        )
        .unwrap();
    let expected_types = [
        MYSQL_TYPE_TINY,
        MYSQL_TYPE_SHORT,
        MYSQL_TYPE_LONG,
        MYSQL_TYPE_LONG,
        MYSQL_TYPE_LONGLONG,
    ];
    assert_eq!(
        prepared
            .columns
            .iter()
            .map(|column| column.column_type)
            .collect::<Vec<_>>(),
        expected_types.to_vec()
    );
    assert_eq!(
        prepared
            .columns
            .iter()
            .map(|column| column.column_length)
            .collect::<Vec<_>>(),
        [4, 6, 11, 11, 20].to_vec()
    );

    let empty = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert!(empty.rows.is_empty());
    assert_eq!(
        empty
            .columns
            .iter()
            .map(|column| column.column_type)
            .collect::<Vec<_>>(),
        expected_types.to_vec()
    );

    adapter
        .connection
        .execute(
            "INSERT INTO declared_widths (tiny, small, int_value, integer_value, big) VALUES (NULL, NULL, NULL, NULL, NULL)",
        )
        .unwrap();
    let all_null = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(
        all_null
            .columns
            .iter()
            .map(|column| column.column_type)
            .collect::<Vec<_>>(),
        expected_types.to_vec()
    );
    assert_eq!(
        all_null.rows,
        [vec![
            BinaryResultValue::Null,
            BinaryResultValue::Null,
            BinaryResultValue::Null,
            BinaryResultValue::Null,
            BinaryResultValue::Null,
        ]]
    );

    adapter
        .connection
        .execute(
            "INSERT INTO declared_widths (tiny, small, int_value, integer_value, big) VALUES (1, 2, 3, 4, 5)",
        )
        .unwrap();
    let values = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(
        values.rows,
        [
            vec![
                BinaryResultValue::Null,
                BinaryResultValue::Null,
                BinaryResultValue::Null,
                BinaryResultValue::Null,
                BinaryResultValue::Null,
            ],
            vec![
                BinaryResultValue::Integer(1),
                BinaryResultValue::Integer(2),
                BinaryResultValue::Integer(3),
                BinaryResultValue::Integer(4),
                BinaryResultValue::Integer(5),
            ],
        ]
    );

    let expression = adapter
        .execute_stmt_prepare(
            "SELECT tiny AS tiny_alias, 1 AS literal_expression, NULL AS null_expression FROM declared_widths",
        )
        .unwrap();
    assert_eq!(
        expression
            .columns
            .iter()
            .map(|column| column.column_type)
            .collect::<Vec<_>>(),
        [MYSQL_TYPE_TINY, MYSQL_TYPE_LONGLONG, MYSQL_TYPE_NULL].to_vec()
    );
    let expression_result = prepared_result_set(
        adapter
            .execute_stmt_execute(expression.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(
        expression_result.rows,
        [
            vec![
                BinaryResultValue::Null,
                BinaryResultValue::Integer(1),
                BinaryResultValue::Null,
            ],
            vec![
                BinaryResultValue::Integer(1),
                BinaryResultValue::Integer(1),
                BinaryResultValue::Null,
            ],
        ]
    );
}

#[test]
fn prepared_mediumint_result_preserves_boundaries_nulls_and_empty_metadata() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute("CREATE TABLE prepared_mediumint (value MEDIUMINT)")
        .unwrap();
    let prepared = adapter
        .execute_stmt_prepare("SELECT value FROM prepared_mediumint")
        .unwrap();
    assert_eq!(prepared.columns[0].column_type, MYSQL_TYPE_INT24);
    assert_eq!(prepared.columns[0].column_length, 9);

    let empty = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns[0].column_type, MYSQL_TYPE_INT24);
    assert_eq!(empty.columns[0].column_length, 9);

    adapter
        .connection
        .execute("INSERT INTO prepared_mediumint (value) VALUES (-8388608), (8388607), (NULL)")
        .unwrap();
    let result = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(result.columns[0].column_type, MYSQL_TYPE_INT24);
    assert_eq!(result.columns[0].column_length, 9);
    assert_eq!(
        result.rows,
        [
            vec![BinaryResultValue::Integer(-8_388_608)],
            vec![BinaryResultValue::Integer(8_388_607)],
            vec![BinaryResultValue::Null],
        ]
    );
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordedDatabaseAction {
    Connect(Option<String>),
    Query(String),
    TableSelect { database: String, table: String },
    Create(String),
    Drop(String),
    List,
}

#[cfg(unix)]
#[derive(Default)]
struct RecordingAuthorizer {
    decisions: Mutex<VecDeque<Result<(), AuthorizationError>>>,
    table_decisions: Mutex<VecDeque<Result<(), AuthorizationError>>>,
    actions: Mutex<Vec<RecordedDatabaseAction>>,
    account_ids: Mutex<Vec<AccountId>>,
}

#[cfg(unix)]
impl RecordingAuthorizer {
    fn with_decisions(decisions: impl IntoIterator<Item = Result<(), AuthorizationError>>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            ..Self::default()
        }
    }

    fn with_decisions_and_table_decisions(
        decisions: impl IntoIterator<Item = Result<(), AuthorizationError>>,
        table_decisions: impl IntoIterator<Item = Result<(), AuthorizationError>>,
    ) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            table_decisions: Mutex::new(table_decisions.into_iter().collect()),
            ..Self::default()
        }
    }

    fn actions(&self) -> Vec<RecordedDatabaseAction> {
        self.actions.lock().unwrap().clone()
    }
}

#[cfg(unix)]
impl DatabaseAuthorizer for RecordingAuthorizer {
    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        self.account_ids
            .lock()
            .unwrap()
            .push(principal.account_id().clone());
        let action = match action {
            DatabaseAction::Connect { database } => {
                RecordedDatabaseAction::Connect(database.map(str::to_owned))
            }
            DatabaseAction::Query { database } => {
                RecordedDatabaseAction::Query(database.to_owned())
            }
            DatabaseAction::Create { database } => {
                RecordedDatabaseAction::Create(database.to_owned())
            }
            DatabaseAction::Drop { database } => RecordedDatabaseAction::Drop(database.to_owned()),
            DatabaseAction::List => RecordedDatabaseAction::List,
        };
        self.actions.lock().unwrap().push(action);
        self.decisions.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }

    fn authorize_table(
        &self,
        principal: &AuthenticatedPrincipal,
        action: TableAction<'_>,
    ) -> Result<(), AuthorizationError> {
        self.account_ids
            .lock()
            .unwrap()
            .push(principal.account_id().clone());
        let TableAction::Select { database, table } = action;
        self.actions
            .lock()
            .unwrap()
            .push(RecordedDatabaseAction::TableSelect {
                database: database.to_owned(),
                table: table.to_owned(),
            });
        self.table_decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(AuthorizationError::Denied))
    }
}

#[cfg(unix)]
fn catalog_factory(
    authorizer: Arc<RecordingAuthorizer>,
) -> (
    tempfile::TempDir,
    Arc<MySqlDatabaseCatalog>,
    AuthorizedDatabaseAdapterFactory<RecordingAuthorizer>,
) {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    catalog.create("reports").unwrap();
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("reports").unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE TABLE records (id INT, label TEXT)")
        .unwrap();
    seed.connection()
        .unwrap()
        .execute("INSERT INTO records (id, label) VALUES (7, 'kept')")
        .unwrap();
    drop(seed);
    let factory =
        AuthorizedDatabaseAdapterFactory::new(catalog.clone(), binary_context(), authorizer);
    (directory, catalog, factory)
}

#[cfg(unix)]
#[test]
fn authorized_text_select_uses_durable_table_metadata_for_alias_and_star() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("reports").unwrap();
    seed.connection()
        .unwrap()
        .execute(
            "CREATE TABLE metadata (id INTEGER NOT NULL PRIMARY KEY, label TEXT DEFAULT 'x' UNIQUE, payload BLOB)",
        )
        .unwrap();

    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([41; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT id AS alias, label FROM metadata AS source")
        .unwrap()
    else {
        panic!("table SELECT must return a result set");
    };
    assert_eq!(result.columns[0].name, "alias");
    assert_eq!(result.columns[0].original_name, "id");
    assert_eq!(result.columns[0].schema, "reports");
    assert_eq!(result.columns[0].table, "source");
    assert_eq!(result.columns[0].original_table, "metadata");
    assert_eq!(
        result.columns[0].flags,
        MYSQL_NOT_NULL_FLAG
            | MYSQL_PRI_KEY_FLAG
            | MYSQL_PART_KEY_FLAG
            | MYSQL_NO_DEFAULT_VALUE_FLAG
            | MYSQL_NUM_FLAG
    );
    assert_eq!(result.columns[1].name, "label");
    assert_eq!(result.columns[1].original_name, "label");
    assert_eq!(result.columns[1].table, "source");
    assert_eq!(result.columns[1].original_table, "metadata");
    // Measured: a TEXT column carries the blob flag, and reports BLOB
    // rather than VAR_STRING.
    assert_eq!(result.columns[1].column_type, MYSQL_TYPE_BLOB);
    assert_eq!(
        result.columns[1].flags,
        MYSQL_UNIQUE_KEY_FLAG | MYSQL_PART_KEY_FLAG | MYSQL_BLOB_FLAG
    );
    let codec = PacketCodec::new(4096).unwrap();
    let frame = result.columns[0].encode(codec, 1).unwrap();
    let decoded = crate::ColumnDefinitionPacket::decode(codec, &frame).unwrap();
    let expected_flags = mysql_common::constants::ColumnFlags::NOT_NULL_FLAG.bits()
        | mysql_common::constants::ColumnFlags::PRI_KEY_FLAG.bits()
        | mysql_common::constants::ColumnFlags::PART_KEY_FLAG.bits()
        | mysql_common::constants::ColumnFlags::NO_DEFAULT_VALUE_FLAG.bits()
        | mysql_common::constants::ColumnFlags::NUM_FLAG.bits();
    assert_eq!(decoded.flags, result.columns[0].flags);
    assert_eq!(decoded.flags, expected_flags);

    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SELECT * FROM metadata").unwrap()
    else {
        panic!("star SELECT must return a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.original_name.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.schema.as_str(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("id", "id", "metadata", "metadata", "reports"),
            ("label", "label", "metadata", "metadata", "reports"),
            ("payload", "payload", "metadata", "metadata", "reports"),
        ]
    );
    // Measured: a BLOB column carries the blob flag and the binary one,
    // where a TEXT column carries only the blob flag.
    assert_eq!(result.columns[2].flags, MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG);

    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SELECT 1 AS literal").unwrap()
    else {
        panic!("literal SELECT must return a result set");
    };
    assert!(result.columns[0].schema.is_empty());
    assert!(result.columns[0].table.is_empty());
    assert!(result.columns[0].original_table.is_empty());
    assert!(result.columns[0].original_name.is_empty());

    let prepared = adapter
        .execute_stmt_prepare("SELECT id AS alias, label FROM metadata AS source")
        .unwrap();
    assert_eq!(prepared.columns[0].name, "alias");
    assert_eq!(prepared.columns[0].original_name, "id");
    assert_eq!(prepared.columns[0].schema, "reports");
    assert_eq!(prepared.columns[0].table, "source");
    assert_eq!(prepared.columns[0].original_table, "metadata");
    assert_eq!(
        prepared.columns[0].flags,
        MYSQL_NOT_NULL_FLAG
            | MYSQL_PRI_KEY_FLAG
            | MYSQL_PART_KEY_FLAG
            | MYSQL_NO_DEFAULT_VALUE_FLAG
            | MYSQL_NUM_FLAG
    );
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(prepared.statement_id, &[])
        .unwrap()
    else {
        panic!("prepared table SELECT must return a result set");
    };
    assert_eq!(result.columns[0], prepared.columns[0]);
    assert_eq!(result.columns[1], prepared.columns[1]);
}

#[cfg(unix)]
#[test]
fn authorized_factory_forwards_optional_query_timeout() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut default_adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([23; 32]),
        ))
        .unwrap();
    assert_eq!(default_adapter.query_timeout, None);
    default_adapter.authorize_connection().unwrap();
    default_adapter.execute_init_db("reports").unwrap();
    assert!(matches!(
        default_adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));

    let timeout = Duration::from_secs(2);
    let configured_adapter = AuthorizedDatabaseAdapterFactory::new(
        catalog.clone(),
        binary_context(),
        authorizer.clone(),
    )
    .with_query_timeout(timeout)
    .build(AuthenticatedPrincipal::from_account_id_for_testing(
        AccountId::from_bytes([24; 32]),
    ))
    .unwrap();
    assert_eq!(configured_adapter.query_timeout, Some(timeout));

    let bootstrap_timeout = Duration::from_millis(1500);
    let bootstrap_adapter = AuthorizedDatabaseAdapterFactory::new(
        catalog.clone(),
        binary_context(),
        authorizer.clone(),
    )
    .with_bootstrap_settings(8192, bootstrap_timeout)
    .build(AuthenticatedPrincipal::from_account_id_for_testing(
        AccountId::from_bytes([27; 32]),
    ))
    .unwrap();
    assert_eq!(
        bootstrap_adapter.bootstrap_settings,
        MySqlBootstrapSettings::new(8192, bootstrap_timeout)
    );

    let options = CommandExecutionOptions::from_capability_flags(CLIENT_FOUND_ROWS);
    let option_adapter =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
            .build_with_options(
                AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes(
                    [25; 32],
                )),
                options,
            )
            .unwrap();
    assert_eq!(option_adapter.command_options(), options);
    assert!(option_adapter.command_options().client_found_rows());
}

#[cfg(unix)]
#[test]
fn authorized_factories_share_the_injected_prepared_statement_authority() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, _factory) = catalog_factory(authorizer.clone());
    let authority = MySqlPreparedStatementAuthority::new(1).unwrap();
    let first_factory = AuthorizedDatabaseAdapterFactory::new(
        catalog.clone(),
        binary_context(),
        authorizer.clone(),
    )
    .with_prepared_statement_authority(authority.clone());
    let second_factory =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
            .with_prepared_statement_authority(authority.clone());
    let principal =
        |id| AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([id; 32]));
    let mut first = first_factory.build(principal(31)).unwrap();
    let mut second = second_factory.build(principal(32)).unwrap();
    first.authorize_connection().unwrap();
    second.authorize_connection().unwrap();
    first.execute_init_db("reports").unwrap();
    second.execute_init_db("reports").unwrap();

    first.execute_stmt_prepare("SELECT 1").unwrap();
    assert_eq!(authority.active_count(), 1);
    assert_eq!(
        second.execute_stmt_prepare("SELECT 2"),
        Err(FrontendErrorKind::PreparedStatementLimitReached)
    );
    first.execute_stmt_close(1);
    assert_eq!(authority.active_count(), 0);
    second.execute_stmt_prepare("SELECT 2").unwrap();
}

#[cfg(unix)]
#[test]
fn authorized_prepare_requires_selection_and_query_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Err(AuthorizationError::Denied),
        Ok(()),
    ]));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([26; 32]),
        ))
        .unwrap();

    assert_eq!(
        adapter.execute_stmt_prepare("SELECT 1"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    adapter.execute_init_db("reports").unwrap();
    assert_eq!(
        adapter.execute_stmt_prepare("SELECT 1"),
        Err(FrontendErrorKind::AccessDenied)
    );
    let prepared = adapter.execute_stmt_prepare("SELECT ? AS value").unwrap();
    assert_eq!(prepared.statement_id, 1);
    assert_eq!(prepared.parameters.len(), 1);
    assert_eq!(prepared.columns[0].name, "value");
    assert_eq!(
        authorizer.actions(),
        [
            RecordedDatabaseAction::Connect(Some("reports".to_string())),
            RecordedDatabaseAction::Query("reports".to_string()),
            RecordedDatabaseAction::Query("reports".to_string()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn denied_database_select_falls_back_to_canonical_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied), Ok(())],
        [Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([32; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();

    assert!(matches!(
        adapter.execute_query("SELECT id FROM `RECORDS`"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert!(matches!(
        adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn unavailable_database_select_does_not_try_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
        [],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([33; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_query("SELECT id FROM records"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn unavailable_database_prepare_does_not_try_table_permission_or_provider() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
        [],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([39; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_stmt_prepare("SELECT id FROM records"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn denied_database_select_checks_table_before_missing_table_lookup() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Err(AuthorizationError::Denied)],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([34; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_query("SELECT id FROM missing_table"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "missing_table".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn denied_database_query_does_not_fallback_for_scalar_dml_or_qualified_select() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
        ],
        [],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([35; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for sql in [
        "SELECT 1",
        "INSERT INTO records (id, label) VALUES (8, 'blocked')",
        "SELECT id FROM main.records",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::AccessDenied),
            "authorization must reject {sql:?} before execution"
        );
    }
    assert_eq!(
        adapter.execute_query("SELECT table_name FROM information_schema.tables"),
        Err(FrontendErrorKind::Syntax)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn denied_prepare_does_not_fallback_for_non_simple_select_or_dml() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
        ],
        [],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([40; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for sql in [
        "SELECT id FROM main.records",
        "INSERT INTO records (id, label) VALUES (8, 'blocked')",
    ] {
        assert_eq!(
            adapter.execute_stmt_prepare(sql),
            Err(FrontendErrorKind::AccessDenied),
            "authorization must reject {sql:?} before preparation"
        );
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn prepared_select_reauthorizes_table_permission_and_keeps_origin_database() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
        ],
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
    ));
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("archive").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([36; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    let prepared = adapter
        .execute_stmt_prepare("SELECT id FROM `RECORDS`")
        .unwrap();
    adapter.execute_init_db("archive").unwrap();

    assert!(matches!(
        adapter.execute_stmt_execute(prepared.statement_id, &[]),
        Ok(PreparedStatementExecutionResult::ResultSet(_))
    ));
    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[]),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
            RecordedDatabaseAction::Connect(Some("archive".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn prepared_execute_preserves_long_data_until_query_authorization_succeeds() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
        [],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([37; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"kept");

    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[]),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        adapter
            .pending_long_data
            .values
            .get(&(prepared.statement_id, 0)),
        Some(&b"kept".to_vec())
    );
    assert_eq!(adapter.pending_long_data.retained_bytes, 4);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );

    let payload = [0, 1, MYSQL_TYPE_VAR_STRING, 0];
    assert!(matches!(
        adapter.execute_stmt_execute(prepared.statement_id, &payload),
        Ok(PreparedStatementExecutionResult::ResultSet(_))
    ));
    assert!(!adapter
        .pending_long_data
        .values
        .contains_key(&(prepared.statement_id, 0)));
    assert_eq!(adapter.pending_long_data.retained_bytes, 0);
}

#[cfg(unix)]
#[test]
fn unknown_prepared_execute_does_not_retain_pending_long_data() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([38; 32]),
        ))
        .unwrap();
    adapter.execute_stmt_send_long_data(u32::MAX, 0, b"unknown");

    assert_eq!(
        adapter.execute_stmt_execute(u32::MAX, &[]),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );
    assert!(adapter.pending_long_data.values.is_empty());
    assert!(adapter.pending_long_data.errors.is_empty());
    assert_eq!(adapter.pending_long_data.retained_bytes, 0);
    assert!(authorizer.actions().is_empty());
}

#[cfg(unix)]
#[test]
fn authorized_adapter_reset_keeps_database_and_clears_connection_state() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([30; 32]),
        ))
        .unwrap();
    adapter.execute_reset_connection().unwrap();
    assert_eq!(adapter.session.selected_database(), None);
    assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
    adapter.execute_init_db("reports").unwrap();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    adapter.execute_stmt_send_long_data(prepared.statement_id, 0, b"discarded");
    adapter
        .execute_query(
            "CREATE TABLE generated_records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
        )
        .unwrap();
    adapter
        .execute_query("INSERT INTO generated_records (label) VALUES ('before_reset')")
        .unwrap();
    adapter.execute_query("BEGIN").unwrap();
    adapter
        .execute_query("INSERT INTO records (id, label) VALUES (8, 'discarded')")
        .unwrap();

    adapter.execute_reset_connection().unwrap();

    assert_eq!(adapter.session.selected_database(), Some("reports"));
    assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
    assert_eq!(adapter.session.connection().unwrap().last_insert_id(), 0);
    assert!(adapter.pending_long_data.values.is_empty());
    assert!(adapter.pending_long_data.errors.is_empty());
    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[0, 0, MYSQL_TYPE_VAR_STRING, 0]),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT id, label FROM records")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert_eq!(
        result.rows,
        [vec![Some(b"7".to_vec()), Some(b"kept".to_vec())]]
    );
}

#[cfg(unix)]
#[test]
fn authorized_adapter_reset_clears_prepared_state_across_database_switches() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("archive").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([31; 32]),
        ))
        .unwrap();
    adapter.execute_init_db("reports").unwrap();
    let reports = adapter
        .execute_stmt_prepare("SELECT ? AS report_value")
        .unwrap();
    adapter.execute_stmt_send_long_data(reports.statement_id, 0, b"reports");

    adapter.execute_init_db("archive").unwrap();
    let archive = adapter
        .execute_stmt_prepare("SELECT ? AS archive_value")
        .unwrap();
    adapter.execute_stmt_send_long_data(archive.statement_id, 0, b"archive");

    adapter.execute_reset_connection().unwrap();

    assert_eq!(adapter.session.selected_database(), Some("archive"));
    assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
    assert!(adapter.pending_long_data.values.is_empty());
    assert!(adapter.pending_long_data.errors.is_empty());
    assert_eq!(
        adapter.execute_stmt_execute(reports.statement_id, &[]),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );
    assert_eq!(
        adapter.execute_stmt_execute(archive.statement_id, &[]),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );
    catalog.drop_database("reports").unwrap();
}

#[cfg(unix)]
#[test]
fn authorized_prepared_statements_keep_origin_connections_across_database_switches() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("archive").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([28; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    let reports = adapter
        .execute_stmt_prepare("SELECT ? AS report_value")
        .unwrap();
    adapter.execute_stmt_send_long_data(reports.statement_id, 0, b"origin");

    adapter.execute_init_db("archive").unwrap();
    let archive = adapter
        .execute_stmt_prepare("SELECT 1 AS archive_value")
        .unwrap();
    assert_eq!((reports.statement_id, archive.statement_id), (1, 2));
    assert!(matches!(
        catalog.drop_database("reports"),
        Err(MySqlDatabaseError::DatabaseBusy(name)) if name == "reports"
    ));

    let first_payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0];
    let first = adapter
        .execute_stmt_execute(reports.statement_id, &first_payload)
        .unwrap();
    let first = prepared_result_set(first);
    assert_eq!(
        first.rows,
        [vec![BinaryResultValue::Text("origin".to_string())]]
    );

    let mut cached_type_payload = vec![0, 0];
    cached_type_payload.extend_from_slice(&[6, b'c', b'a', b'c', b'h', b'e', b'd']);
    let cached_type = adapter
        .execute_stmt_execute(reports.statement_id, &cached_type_payload)
        .unwrap();
    let cached_type = prepared_result_set(cached_type);
    assert_eq!(
        cached_type.rows,
        [vec![BinaryResultValue::Text("cached".to_string())]]
    );

    assert_eq!(adapter.execute_stmt_reset(reports.statement_id), Ok(()));
    adapter.execute_stmt_close(reports.statement_id);
    assert_eq!(
        adapter.execute_stmt_reset(reports.statement_id),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    );
    assert!(matches!(
        adapter.execute_stmt_execute(reports.statement_id, &cached_type_payload),
        Err(FrontendErrorKind::UnknownPreparedStatement)
    ));
    catalog.drop_database("reports").unwrap();

    let next = adapter
        .execute_stmt_prepare("SELECT 2 AS next_value")
        .unwrap();
    assert_eq!(next.statement_id, 3);
    assert_eq!(
        authorizer.actions(),
        [
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_string())),
            RecordedDatabaseAction::Query("reports".to_string()),
            RecordedDatabaseAction::Connect(Some("archive".to_string())),
            RecordedDatabaseAction::Query("archive".to_string()),
            RecordedDatabaseAction::Query("reports".to_string()),
            RecordedDatabaseAction::Query("reports".to_string()),
            RecordedDatabaseAction::Query("archive".to_string()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn authorized_database_switch_rejects_autocommit_disabled_before_a_transaction_starts() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("archive").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([29; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    adapter.execute_query("SET autocommit = 0").unwrap();

    assert_eq!(
        adapter.execute_init_db("archive"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("USE archive"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(adapter.session.selected_database(), Some("reports"));
}

#[test]
fn prepared_select_rejects_rows_beyond_dispatch_limit_during_execution() {
    let mut adapter = adapter();
    let prepared = adapter
        .execute_stmt_prepare("SELECT id FROM many_rows")
        .unwrap();

    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[]),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[test]
fn prepared_execute_keeps_parameter_types_after_execution_error() {
    let mut adapter = adapter();
    let prepared = adapter.execute_stmt_prepare("SELECT ?").unwrap();
    let mut first_payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
    first_payload.extend_from_slice(&1i64.to_le_bytes());

    let result = execute_prepared_statement(
        &adapter.connection,
        &mut adapter.prepared_types,
        prepared.statement_id,
        &first_payload,
        StatementLongData {
            values: Vec::new(),
            error: None,
        },
        Some(Duration::ZERO),
        MySqlAffectedRowsMode::Changed,
    );
    assert_eq!(result, Err(FrontendErrorKind::QueryTimeout));

    let mut retry_payload = vec![0, 0];
    retry_payload.extend_from_slice(&2i64.to_le_bytes());
    let retried = adapter
        .execute_stmt_execute(prepared.statement_id, &retry_payload)
        .unwrap();
    let retried = prepared_result_set(retried);
    assert_eq!(retried.rows, [vec![BinaryResultValue::Integer(2)]]);
}

#[cfg(unix)]
#[test]
#[should_panic(expected = "query timeout must be non-zero")]
fn authorized_factory_rejects_zero_query_timeout() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("archive").unwrap();
    let _ = factory.with_query_timeout(Duration::ZERO);
}

#[test]
fn select_result_preserves_null_and_binary_values() {
    let mut adapter = adapter();
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT id, payload FROM result_values")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };

    assert_eq!(result.columns.len(), 2);
    assert_eq!(
        result.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(vec![0, 0xff])],
            vec![Some(b"2".to_vec()), None]
        ]
    );
    assert_eq!(result.columns[1].column_type, MYSQL_TYPE_BLOB);
    assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONG);
}

#[test]
fn last_insert_id_is_available_through_the_checked_select_path() {
    let mut adapter = adapter();
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT LAST_INSERT_ID() AS generated_id")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };

    assert_eq!(result.rows, vec![vec![Some(b"0".to_vec())]]);
    assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONGLONG);
}

#[test]
fn drop_view_dispatch_preserves_backticks_in_select_and_insert_strings() {
    let mut adapter = adapter();
    let CommandExecutionResult::ResultSet(result) = adapter.execute_query("SELECT '`'").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(result.rows, vec![vec![Some(b"`".to_vec())]]);
    adapter
        .execute_query("CREATE TABLE quoted_values (label TEXT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO quoted_values (label) VALUES ('`')")
        .unwrap();
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT label FROM quoted_values")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(result.rows, vec![vec![Some(b"`".to_vec())]]);
}

#[test]
fn drop_view_commits_before_success_and_object_errors() {
    let mut adapter = adapter();
    adapter
        .execute_query("CREATE TABLE records (id INT)")
        .unwrap();
    adapter
        .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
        .unwrap();
    for (sql, expected) in [
        ("DROP VIEW records_view", None),
        (
            "DROP VIEW missing_view",
            Some(FrontendErrorKind::UnknownView),
        ),
        ("DROP VIEW records", Some(FrontendErrorKind::NotView)),
    ] {
        adapter.execute_query("BEGIN").unwrap();
        adapter
            .execute_query("INSERT INTO records (id) VALUES (7)")
            .unwrap();
        let result = adapter.execute_query(sql);
        if let Some(error) = expected {
            assert_eq!(result, Err(error));
        } else {
            let CommandExecutionResult::Ok(result) = result.unwrap() else {
                panic!("DROP must return OK");
            };
            assert_eq!(result.affected_rows, 0);
            assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
        }
        assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);
        adapter.execute_query("ROLLBACK").unwrap();
    }
    let CommandExecutionResult::ResultSet(rows) =
        adapter.execute_query("SELECT id FROM records").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(
        adapter.execute_query("DROP VIEW records_view"),
        Err(FrontendErrorKind::UnknownView)
    );
    assert!(adapter
        .execute_query("SELECT id FROM records_view")
        .is_err());
}

/// `CREATE TABLE ... AS SELECT` makes a table out of what a `SELECT` answers.
#[cfg(unix)]
#[test]
fn create_table_as_select_copies_the_columns_and_the_rows() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([28; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE src (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, n INT, c INT NOT NULL DEFAULT 7, amount DECIMAL(10,2))",
        )
        .unwrap();
    adapter
        .execute_query("INSERT INTO src (n, amount) VALUES (1, 1.5), (2, 2.5), (3, 3.5)")
        .unwrap();

    let CommandExecutionResult::Ok(copied) = adapter
        .execute_query("CREATE TABLE copy_all AS SELECT * FROM src")
        .unwrap()
    else {
        panic!("CREATE TABLE AS SELECT must return OK");
    };
    // Measured on MySQL 8.4.11: `ROW_COUNT()` is the number of rows copied.
    assert_eq!(copied.affected_rows, 3);

    // Byte for byte what MySQL 8.4.11 prints for the copy: the type, the
    // NOT NULL and the DEFAULT are kept, the keys and the AUTO_INCREMENT are
    // gone, and a zero default takes the AUTO_INCREMENT's place.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE copy_all").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `copy_all` (\n",
            "  `id` int NOT NULL DEFAULT '0',\n",
            "  `n` int DEFAULT NULL,\n",
            "  `c` int NOT NULL DEFAULT '7',\n",
            "  `amount` decimal(10,2) DEFAULT NULL\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id, n FROM copy_all ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(
        rows.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"1".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"2".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"3".to_vec())],
        ]
    );

    // A listed projection copies only what it names, and an alias renames it.
    let CommandExecutionResult::Ok(some) = adapter
        .execute_query("CREATE TABLE copy_some AS SELECT id, n AS count FROM src WHERE n > 1")
        .unwrap()
    else {
        panic!("CREATE TABLE AS SELECT must return OK");
    };
    assert_eq!(some.affected_rows, 2);
    let CommandExecutionResult::ResultSet(created) = adapter
        .execute_query("SHOW CREATE TABLE copy_some")
        .unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `copy_some` (\n",
            "  `id` int NOT NULL DEFAULT '0',\n",
            "  `count` int DEFAULT NULL\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // A failure leaves no table behind, and a name that is not there answers
    // 1146 rather than making an empty one.
    assert_eq!(
        adapter.execute_query("CREATE TABLE from_missing AS SELECT id FROM nosuch"),
        Err(FrontendErrorKind::UnknownTable)
    );
    assert_eq!(
        adapter.execute_query("CREATE TABLE from_missing AS SELECT nosuchcolumn FROM src"),
        Err(FrontendErrorKind::UnknownColumn)
    );
    assert!(adapter
        .execute_query("SELECT id FROM from_missing")
        .is_err());

    // An expression column is a rule of its own, measured but not written.
    assert!(adapter
        .execute_query("CREATE TABLE from_expr AS SELECT id + 1 AS s FROM src")
        .is_err());

    // A string DEFAULT is refused rather than reprinted, because its escaping
    // is not decided here — the same reason SHOW CREATE TABLE refuses one.
    adapter
        .execute_query("CREATE TABLE texty (id INT NOT NULL, label VARCHAR(8) DEFAULT 'x')")
        .unwrap();
    assert_eq!(
        adapter.execute_query("CREATE TABLE from_texty AS SELECT * FROM texty"),
        Err(FrontendErrorKind::Unsupported)
    );

    // Measured on MySQL 8.4.11: a ROLLBACK after one leaves the table there,
    // so the statement commits the way its other DDL does.
    adapter.execute_query("BEGIN").unwrap();
    adapter
        .execute_query("CREATE TABLE copy_txn AS SELECT id FROM src")
        .unwrap();
    adapter.execute_query("ROLLBACK").unwrap();
    let CommandExecutionResult::ResultSet(kept) = adapter
        .execute_query("SELECT id FROM copy_txn ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(kept.rows.len(), 3);
}

/// `ALTER TABLE` adds and drops indexes, which is how a migration writes one.
#[cfg(unix)]
#[test]
fn alter_table_adds_and_drops_indexes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([27; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, c VARCHAR(10), d INT)")
        .unwrap();
    let CommandExecutionResult::Ok(result) = adapter
        .execute_query("ALTER TABLE t ADD INDEX idx_c (c)")
        .unwrap()
    else {
        panic!("ALTER TABLE must return OK");
    };
    assert_eq!(result.affected_rows, 0);
    adapter
        .execute_query("ALTER TABLE t ADD KEY idx_d (d), ADD UNIQUE INDEX uniq_cd (c, d)")
        .unwrap();

    // Byte for byte what MySQL 8.4.11 prints after the same three
    // operations.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE t").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `t` (\n",
            "  `id` int NOT NULL,\n",
            "  `c` varchar(10) DEFAULT NULL,\n",
            "  `d` int DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`),\n",
            "  UNIQUE KEY `uniq_cd` (`c`,`d`),\n",
            "  KEY `idx_c` (`c`),\n",
            "  KEY `idx_d` (`d`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    // The unique key is a real one, not just a line in the printout.
    adapter
        .execute_query("INSERT INTO t (id, c, d) VALUES (1, 'a', 1)")
        .unwrap();
    assert!(adapter
        .execute_query("INSERT INTO t (id, c, d) VALUES (2, 'a', 1)")
        .is_err());

    adapter
        .execute_query("ALTER TABLE t DROP INDEX idx_c, DROP INDEX idx_d")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE t").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `t` (\n",
            "  `id` int NOT NULL,\n",
            "  `c` varchar(10) DEFAULT NULL,\n",
            "  `d` int DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`),\n",
            "  UNIQUE KEY `uniq_cd` (`c`,`d`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // Measured on MySQL 8.4.11: 1061 for a name the table already carries,
    // 1091 for one it does not, and 1146 for a table that is not there.
    assert_eq!(
        adapter.execute_query("ALTER TABLE t ADD INDEX uniq_cd (c)"),
        Err(FrontendErrorKind::DuplicateKeyName)
    );
    assert_eq!(
        adapter.execute_query("ALTER TABLE t DROP INDEX idx_c"),
        Err(FrontendErrorKind::CantDropKey)
    );
    assert_eq!(
        adapter.execute_query("ALTER TABLE missing ADD INDEX idx_c (c)"),
        Err(FrontendErrorKind::UnknownTable)
    );

    // MySQL applies the whole statement or none of it, so a second operation
    // that fails leaves the first one undone.
    assert_eq!(
        adapter.execute_query("ALTER TABLE t ADD INDEX idx_c (c), ADD INDEX uniq_cd (d)"),
        Err(FrontendErrorKind::DuplicateKeyName)
    );
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE t").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert!(!String::from_utf8(created.rows[0][1].clone().unwrap())
        .unwrap()
        .contains("idx_c"));

    // The spellings and shapes this does not take.
    for sql in [
        // MySQL names an unnamed key after its first column and disambiguates
        // with `_2`, which this does not implement.
        "ALTER TABLE t ADD INDEX (c)",
        // `sqlparser` reads only the `DROP INDEX` spelling.
        "ALTER TABLE t DROP KEY uniq_cd",
        "ALTER TABLE t ADD COLUMN e INT, ADD INDEX idx_e (e)",
        "ALTER TABLE t DROP INDEX `PRIMARY`",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `TRUNCATE TABLE` empties a table, and commits the way MySQL's DDL does.
#[test]
fn truncate_table_empties_the_table_and_cannot_be_rolled_back() {
    let mut adapter = adapter();
    adapter
        .execute_query("CREATE TABLE records (id INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO records (id) VALUES (1), (2), (3)")
        .unwrap();
    let CommandExecutionResult::Ok(result) =
        adapter.execute_query("TRUNCATE TABLE records").unwrap()
    else {
        panic!("TRUNCATE TABLE must return OK");
    };
    // Measured on MySQL 8.4.11: `ROW_COUNT()` is 0 however many rows went.
    assert_eq!(result.affected_rows, 0);
    assert_eq!(result.warnings, 0);
    let CommandExecutionResult::ResultSet(rows) =
        adapter.execute_query("SELECT id FROM records").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert!(rows.rows.is_empty());

    // Measured on MySQL 8.4.11: a ROLLBACK after one leaves the table empty,
    // and the write before it is committed rather than undone. The `TABLE`
    // keyword is optional there too.
    adapter
        .execute_query("INSERT INTO records (id) VALUES (4)")
        .unwrap();
    adapter.execute_query("BEGIN").unwrap();
    adapter
        .execute_query("INSERT INTO records (id) VALUES (5)")
        .unwrap();
    adapter.execute_query("TRUNCATE records").unwrap();
    adapter.execute_query("ROLLBACK").unwrap();
    let CommandExecutionResult::ResultSet(rows) =
        adapter.execute_query("SELECT id FROM records").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert!(rows.rows.is_empty());

    // Measured on MySQL 8.4.11: an unknown name and a view both answer 1146.
    assert_eq!(
        adapter.execute_query("TRUNCATE TABLE missing_records"),
        Err(FrontendErrorKind::UnknownTable)
    );
    adapter
        .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
        .unwrap();
    assert_eq!(
        adapter.execute_query("TRUNCATE TABLE records_view"),
        Err(FrontendErrorKind::UnknownTable)
    );
}

/// MySQL restarts an `AUTO_INCREMENT` counter at 1 on `TRUNCATE TABLE`, and
/// the durable allocator here only moves its high water forward.
#[cfg(unix)]
#[test]
fn truncate_table_refuses_an_auto_increment_table() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([26; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE tickets (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO tickets (v) VALUES (1), (2)")
        .unwrap();
    assert_eq!(
        adapter.execute_query("TRUNCATE TABLE tickets"),
        Err(FrontendErrorKind::Unsupported)
    );
    // The refusal leaves the rows alone rather than emptying the table and
    // then failing.
    let CommandExecutionResult::ResultSet(rows) =
        adapter.execute_query("SELECT id FROM tickets").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(rows.rows.len(), 2);

    // A table with no allocator is taken in the same session.
    adapter
        .execute_query("CREATE TABLE plain (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO plain (id) VALUES (1)")
        .unwrap();
    adapter.execute_query("TRUNCATE TABLE plain").unwrap();
    let CommandExecutionResult::ResultSet(rows) =
        adapter.execute_query("SELECT id FROM plain").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert!(rows.rows.is_empty());
}

#[test]
fn drop_table_commits_and_respects_if_exists_warning_notes() {
    let mut adapter = adapter();
    adapter
        .execute_query("CREATE TABLE records (id INT)")
        .unwrap();
    adapter.execute_query("BEGIN").unwrap();
    adapter
        .execute_query("INSERT INTO records (id) VALUES (7)")
        .unwrap();
    assert_eq!(
        adapter.execute_query("DROP TABLE missing_records"),
        Err(FrontendErrorKind::UnknownTable)
    );
    adapter.execute_query("ROLLBACK").unwrap();
    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SELECT id FROM records").unwrap()
    else {
        panic!("the failed DROP TABLE must commit preceding writes");
    };
    assert_eq!(result.rows, vec![vec![Some(b"7".to_vec())]]);
    let CommandExecutionResult::Ok(result) = adapter.execute_query("DROP TABLE records").unwrap()
    else {
        panic!("DROP TABLE must return OK");
    };
    assert_eq!(result.warnings, 0);
    assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
    assert_eq!(
        adapter.execute_query("DROP TABLE records"),
        Err(FrontendErrorKind::UnknownTable)
    );

    adapter.execute_query("SET sql_notes = 0").unwrap();
    let CommandExecutionResult::Ok(result) = adapter
        .execute_query("DROP TABLE IF EXISTS records")
        .unwrap()
    else {
        panic!("DROP TABLE IF EXISTS must return OK");
    };
    assert_eq!(result.warnings, 0);

    adapter.execute_query("SET sql_notes = 1").unwrap();
    let CommandExecutionResult::Ok(result) = adapter
        .execute_query("DROP TABLE IF EXISTS records")
        .unwrap()
    else {
        panic!("DROP TABLE IF EXISTS must return OK");
    };
    assert_eq!(result.warnings, 1);

    adapter
        .execute_query("CREATE TABLE records (id INT)")
        .unwrap();
    adapter
        .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
        .unwrap();
    assert_eq!(
        adapter.execute_query("DROP TABLE records_view"),
        Err(FrontendErrorKind::UnknownTable)
    );
    let CommandExecutionResult::Ok(result) = adapter
        .execute_query("DROP TABLE IF EXISTS records_view")
        .unwrap()
    else {
        panic!("DROP TABLE IF EXISTS must return OK for a view");
    };
    assert_eq!(result.warnings, 1);
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT id FROM records_view")
        .unwrap()
    else {
        panic!("the view must remain after DROP TABLE IF EXISTS");
    };
    assert!(result.rows.is_empty());
}

#[test]
fn sql_notes_is_isolated_and_resets_only_after_success() {
    let mut first = adapter();
    let mut second = adapter();
    first.execute_query("BEGIN").unwrap();
    let CommandExecutionResult::Ok(result) = first.execute_query("SET sql_notes = 0").unwrap()
    else {
        panic!("SET must return OK");
    };
    assert_eq!(
        result.status_flags,
        SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
    );
    for (adapter, expected) in [(&mut first, b"0"), (&mut second, b"1")] {
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SELECT @@sql_notes").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(result.rows, vec![vec![Some(expected.to_vec())]]);
    }
    first.execute_reset_connection().unwrap();
    let CommandExecutionResult::ResultSet(result) =
        first.execute_query("SELECT @@sql_notes").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(result.rows, vec![vec![Some(b"1".to_vec())]]);
    first.execute_query("SET sql_notes = 0").unwrap();
    first.execute_query("BEGIN").unwrap();
    first
        .execute_query("INSERT INTO result_values (id) VALUES (3)")
        .unwrap();
    first.connection.close().unwrap();
    assert!(first.execute_reset_connection().is_err());
    let CommandExecutionResult::ResultSet(result) =
        first.execute_query("SELECT @@sql_notes").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(result.rows, vec![vec![Some(b"0".to_vec())]]);
}

#[test]
fn checked_schema_commands_commit_pending_writes_and_report_idle_status() {
    for ddl in [
        "CREATE INDEX records_id ON records (id)",
        "CREATE VIEW records_view AS SELECT id FROM records",
        "ALTER TABLE records ADD COLUMN label TEXT",
    ] {
        let mut adapter = adapter();
        adapter
            .execute_query("CREATE TABLE records (id INT)")
            .unwrap();
        adapter.execute_query("SET autocommit = 0").unwrap();
        adapter
            .execute_query("INSERT INTO records (id) VALUES (7)")
            .unwrap();
        assert_eq!(adapter.status_flags(), SERVER_STATUS_IN_TRANS);
        let CommandExecutionResult::Ok(result) = adapter.execute_query(ddl).unwrap() else {
            panic!("schema command must return OK: {ddl}");
        };
        assert_eq!(result.status_flags, 0, "{ddl}");
        assert_eq!(result.affected_rows, 0, "{ddl}");
        assert_eq!(result.last_insert_id, 0, "{ddl}");
        adapter.execute_query("ROLLBACK").unwrap();
        let CommandExecutionResult::ResultSet(rows) =
            adapter.execute_query("SELECT id FROM records").unwrap()
        else {
            panic!("SELECT must return rows");
        };
        assert_eq!(rows.rows, vec![vec![Some(b"7".to_vec())]], "{ddl}");
    }
}

#[test]
fn checked_schema_commands_preserve_view_and_altered_column_metadata() {
    let mut adapter = adapter();
    adapter
        .execute_query("CREATE TABLE records (id SMALLINT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO records (id) VALUES (7)")
        .unwrap();
    adapter
        .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
        .unwrap();
    let CommandExecutionResult::ResultSet(view) = adapter
        .execute_query("SELECT id FROM records_view")
        .unwrap()
    else {
        panic!("view SELECT must return rows");
    };
    assert_eq!(view.rows, vec![vec![Some(b"7".to_vec())]]);
    adapter
        .execute_query("ALTER TABLE records ADD COLUMN label TEXT DEFAULT 'new'")
        .unwrap();
    let CommandExecutionResult::ResultSet(altered) =
        adapter.execute_query("SELECT label FROM records").unwrap()
    else {
        panic!("altered column SELECT must return rows");
    };
    assert_eq!(altered.rows, vec![vec![Some(b"new".to_vec())]]);
    assert!(adapter
        .execute_query("ALTER TABLE records RENAME TO renamed_records")
        .is_err());
    let sql = "DROP INDEX records_id ON records";
    assert!(
        adapter.execute_query(sql).is_err(),
        "unsupported DDL accepted: {sql}"
    );
}

#[test]
fn empty_insert_distinguishes_missing_default_from_explicit_null() {
    let mut adapter = adapter();
    adapter
        .execute_query(
            "CREATE TABLE required_values (required INT NOT NULL, optional INT DEFAULT 7)",
        )
        .unwrap();
    for (sql, expected) in [
        (
            "INSERT INTO required_values () VALUES ()",
            FrontendErrorKind::MissingRequiredDefault,
        ),
        (
            "INSERT INTO required_values (required) VALUES (NULL)",
            FrontendErrorKind::NotNullViolation,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(expected));
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        assert_eq!(
            adapter.execute_stmt_execute(prepared.statement_id, &[]),
            Err(expected)
        );
        adapter.execute_stmt_close(prepared.statement_id);
    }
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT required FROM required_values")
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert!(result.rows.is_empty());
    adapter
        .execute_query("CREATE TABLE default_values (value INT DEFAULT 7)")
        .unwrap();
    let prepared = adapter
        .execute_stmt_prepare("INSERT INTO default_values () VALUES ()")
        .unwrap();
    adapter
        .execute_query("ALTER TABLE default_values ADD COLUMN required INT NOT NULL")
        .unwrap();
    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[]),
        Err(FrontendErrorKind::MissingRequiredDefault)
    );
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT value FROM default_values")
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert!(result.rows.is_empty());
}

#[test]
fn checked_insert_and_delete_return_ok_results() {
    let mut adapter = adapter();
    let CommandExecutionResult::Ok(inserted) = adapter
        .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'kept')")
        .unwrap()
    else {
        panic!("INSERT must produce an OK result");
    };
    assert_eq!(inserted.affected_rows, 1);
    assert_eq!(inserted.last_insert_id, 0);

    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query("DELETE FROM result_values WHERE payload IS NULL")
        .unwrap()
    else {
        panic!("DELETE must produce an OK result");
    };
    assert_eq!(deleted.affected_rows, 1);
    assert_eq!(deleted.last_insert_id, 0);

    let CommandExecutionResult::Ok(deleted_again) = adapter
        .execute_query("DELETE FROM result_values WHERE payload IS NULL")
        .unwrap()
    else {
        panic!("DELETE must produce an OK result");
    };
    assert_eq!(deleted_again.affected_rows, 0);
}

#[test]
fn explicit_transactions_report_status_and_rollback_rows() {
    let mut adapter = adapter();
    let CommandExecutionResult::Ok(begin) = adapter.execute_query("BEGIN").unwrap() else {
        panic!("BEGIN must produce an OK result");
    };
    assert_eq!(
        begin.status_flags,
        SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
    );

    let CommandExecutionResult::Ok(inserted) = adapter
        .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'discarded')")
        .unwrap()
    else {
        panic!("INSERT must produce an OK result");
    };
    assert_eq!(inserted.status_flags, begin.status_flags);

    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT id, payload FROM result_values")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert_eq!(selected.status_flags, begin.status_flags);

    let CommandExecutionResult::Ok(rollback) = adapter.execute_query("ROLLBACK").unwrap() else {
        panic!("ROLLBACK must produce an OK result");
    };
    assert_eq!(rollback.status_flags, SERVER_STATUS_AUTOCOMMIT);
    assert_eq!(adapter.status_flags(), SERVER_STATUS_AUTOCOMMIT);

    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT id, payload FROM result_values")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert_eq!(selected.rows.len(), 2);
}

#[test]
fn autocommit_status_tracks_setting_and_lazy_write_transaction() {
    let mut adapter = adapter();
    let CommandExecutionResult::Ok(disabled) =
        adapter.execute_query("SET SESSION autocommit = 0").unwrap()
    else {
        panic!("SET autocommit must produce an OK result");
    };
    assert_eq!(disabled.status_flags, 0);

    let CommandExecutionResult::ResultSet(constant) =
        adapter.execute_query("SELECT 1 AS value").unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert_eq!(constant.status_flags, 0);

    let CommandExecutionResult::Ok(inserted) = adapter
        .execute_query("INSERT INTO result_values (id, payload) VALUES (3, 'pending')")
        .unwrap()
    else {
        panic!("INSERT must produce an OK result");
    };
    assert_eq!(inserted.status_flags, SERVER_STATUS_IN_TRANS);

    let CommandExecutionResult::Ok(committed) =
        adapter.execute_query("SET autocommit = 1").unwrap()
    else {
        panic!("SET autocommit must produce an OK result");
    };
    assert_eq!(committed.status_flags, SERVER_STATUS_AUTOCOMMIT);
}

#[cfg(unix)]
#[test]
fn active_transaction_rejects_database_switch_without_losing_state() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([27; 32]),
        ))
        .unwrap();
    adapter.execute_init_db("reports").unwrap();
    adapter.execute_query("BEGIN").unwrap();

    assert_eq!(
        adapter.execute_init_db("archive"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("USE archive"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.status_flags(),
        SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
    );
    assert_eq!(adapter.session.selected_database(), Some("reports"));
}

#[cfg(unix)]
#[test]
fn authorized_adapter_applies_found_rows_to_update_ok_results() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, _factory) = catalog_factory(authorizer.clone());
    let principal =
        AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([26; 32]));

    let mut changed_rows = AuthorizedDatabaseAdapterFactory::new(
        catalog.clone(),
        binary_context(),
        authorizer.clone(),
    )
    .build(principal)
    .unwrap();
    changed_rows.authorize_connection().unwrap();
    changed_rows.execute_init_db("reports").unwrap();
    let CommandExecutionResult::Ok(result) = changed_rows
        .execute_query("UPDATE records SET label = 'kept' WHERE TRUE")
        .unwrap()
    else {
        panic!("UPDATE must produce an OK result");
    };
    assert_eq!(result.affected_rows, 0);

    let mut matched_rows =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
            .build_with_options(
                AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes(
                    [27; 32],
                )),
                CommandExecutionOptions::from_capability_flags(CLIENT_FOUND_ROWS),
            )
            .unwrap();
    matched_rows.authorize_connection().unwrap();
    matched_rows.execute_init_db("reports").unwrap();

    let CommandExecutionResult::Ok(no_op) = matched_rows
        .execute_query("UPDATE records SET label = 'kept' WHERE TRUE")
        .unwrap()
    else {
        panic!("UPDATE must produce an OK result");
    };
    assert_eq!(no_op.affected_rows, 1);

    let CommandExecutionResult::Ok(actual) = matched_rows
        .execute_query("UPDATE records SET label = 'changed' WHERE TRUE")
        .unwrap()
    else {
        panic!("UPDATE must produce an OK result");
    };
    assert_eq!(actual.affected_rows, 1);
}

#[cfg(unix)]
#[test]
fn authorized_prepared_update_applies_found_rows_option() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, _factory) = catalog_factory(authorizer.clone());

    let mut changed_rows = AuthorizedDatabaseAdapterFactory::new(
        catalog.clone(),
        binary_context(),
        authorizer.clone(),
    )
    .build(AuthenticatedPrincipal::from_account_id_for_testing(
        AccountId::from_bytes([30; 32]),
    ))
    .unwrap();
    changed_rows.authorize_connection().unwrap();
    changed_rows.execute_init_db("reports").unwrap();
    let changed = changed_rows
        .execute_stmt_prepare("UPDATE records SET label = ? WHERE TRUE")
        .unwrap();
    let payload = [0, 1, MYSQL_TYPE_VAR_STRING, 0, 4, b'k', b'e', b'p', b't'];
    assert!(matches!(
        changed_rows
            .execute_stmt_execute(changed.statement_id, &payload)
            .unwrap(),
        PreparedStatementExecutionResult::Ok(CommandOkResult {
            affected_rows: 0,
            ..
        })
    ));

    let mut matched_rows =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
            .build_with_options(
                AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes(
                    [31; 32],
                )),
                CommandExecutionOptions::from_capability_flags(CLIENT_FOUND_ROWS),
            )
            .unwrap();
    matched_rows.authorize_connection().unwrap();
    matched_rows.execute_init_db("reports").unwrap();
    let matched = matched_rows
        .execute_stmt_prepare("UPDATE records SET label = ? WHERE TRUE")
        .unwrap();
    assert!(matches!(
        matched_rows
            .execute_stmt_execute(matched.statement_id, &payload)
            .unwrap(),
        PreparedStatementExecutionResult::Ok(CommandOkResult {
            affected_rows: 1,
            ..
        })
    ));
}

#[test]
fn checked_writes_allow_leading_comments() {
    let mut adapter = adapter();
    let CommandExecutionResult::Ok(inserted) = adapter
        .execute_query(
            "/* leading comment */ INSERT INTO result_values (id, payload) VALUES (3, 'kept')",
        )
        .unwrap()
    else {
        panic!("INSERT must produce an OK result");
    };
    assert_eq!(inserted.affected_rows, 1);

    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query("-- leading comment\nDELETE FROM result_values")
        .unwrap()
    else {
        panic!("DELETE must produce an OK result");
    };
    assert_eq!(deleted.affected_rows, 3);
}

#[test]
fn metadata_type_survives_all_null_result() {
    let mut adapter = adapter();
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT payload FROM result_values WHERE payload IS NULL")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };

    assert_eq!(result.rows, vec![vec![None]]);
    assert_eq!(result.columns[0].column_type, MYSQL_TYPE_BLOB);

    let CommandExecutionResult::ResultSet(empty) = adapter
        .execute_query("SELECT payload FROM result_values WHERE id IS NULL")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns[0].column_type, MYSQL_TYPE_BLOB);
}

#[test]
fn literal_metadata_has_stable_mysql_types_and_collations() {
    let mut adapter = adapter();
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT 1 AS i, 'x' AS t, TRUE AS b, NULL AS n")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.column_type, column.character_set))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_LONGLONG, MYSQL_BINARY_COLLATION),
            (MYSQL_TYPE_VAR_STRING, u16::from(DEFAULT_UTF8MB4_COLLATION)),
            (MYSQL_TYPE_LONGLONG, MYSQL_BINARY_COLLATION),
            (MYSQL_TYPE_NULL, MYSQL_BINARY_COLLATION),
        ]
    );
}

#[test]
fn static_literal_metadata_matches_oracle_for_text_prepare_and_empty_binary() {
    let sql = "SELECT 0 AS zero, -0 AS negative_zero, +0 AS positive_zero, 1 AS one, -1 AS neg_one, 0001 AS leading_zero, -0001 AS negative_leading_zero, +0001 AS positive_leading_zero, 9223372036854775807 AS max_i64, -9223372036854775808 AS min_i64, NULL AS null_value, TRUE AS true_value, FALSE AS false_value, +1 AS positive_sign LIMIT 0";
    let integer_metadata = |column_length| {
        (
            MYSQL_TYPE_LONGLONG,
            MYSQL_BINARY_COLLATION,
            column_length,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            0,
        )
    };
    let expected = [
        integer_metadata(2),
        integer_metadata(2),
        integer_metadata(2),
        integer_metadata(2),
        integer_metadata(2),
        integer_metadata(5),
        integer_metadata(5),
        integer_metadata(5),
        integer_metadata(20),
        integer_metadata(20),
        (
            MYSQL_TYPE_NULL,
            MYSQL_BINARY_COLLATION,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            0,
        ),
        integer_metadata(1),
        integer_metadata(1),
        integer_metadata(2),
    ];
    let metadata = |columns: &[ColumnDefinitionConfig]| {
        columns
            .iter()
            .map(|column| {
                (
                    column.column_type,
                    column.character_set,
                    column.column_length,
                    column.flags,
                    column.decimals,
                )
            })
            .collect::<Vec<_>>()
    };

    let mut adapter = adapter();
    let CommandExecutionResult::ResultSet(text) = adapter.execute_query(sql).unwrap() else {
        panic!("static literal query must produce a result set");
    };
    assert!(text.rows.is_empty());
    assert_eq!(metadata(&text.columns), expected);

    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(metadata(&prepared.columns), expected);
    let binary = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert!(binary.rows.is_empty());
    assert_eq!(metadata(&binary.columns), expected);
}

#[test]
fn static_literal_metadata_survives_wildcard_expansion() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute("CREATE TABLE wildcard_metadata (id INT, label TEXT)")
        .unwrap();
    let sql = "SELECT *, 0001 AS literal_value FROM wildcard_metadata LIMIT 0";
    let expected = (
        MYSQL_TYPE_LONGLONG,
        MYSQL_BINARY_COLLATION,
        5,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        0,
    );

    let CommandExecutionResult::ResultSet(text) = adapter.execute_query(sql).unwrap() else {
        panic!("wildcard SELECT must produce a result set");
    };
    assert!(text.rows.is_empty());
    assert_eq!(
        (
            text.columns[2].column_type,
            text.columns[2].character_set,
            text.columns[2].column_length,
            text.columns[2].flags,
            text.columns[2].decimals,
        ),
        expected
    );

    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(
        (
            prepared.columns[2].column_type,
            prepared.columns[2].character_set,
            prepared.columns[2].column_length,
            prepared.columns[2].flags,
            prepared.columns[2].decimals,
        ),
        expected
    );
    let binary = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert!(binary.rows.is_empty());
    assert_eq!(
        (
            binary.columns[2].column_type,
            binary.columns[2].character_set,
            binary.columns[2].column_length,
            binary.columns[2].flags,
            binary.columns[2].decimals,
        ),
        expected
    );
}

#[test]
fn multiple_wildcards_fall_back_without_metadata_index_panic() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute("CREATE TABLE multiple_wildcards (id INT, label TEXT)")
        .unwrap();
    let sql = "SELECT *, * FROM multiple_wildcards LIMIT 0";

    let CommandExecutionResult::ResultSet(text) = adapter.execute_query(sql).unwrap() else {
        panic!("multiple-wildcard SELECT must produce a result set");
    };
    assert!(text.rows.is_empty());
    assert_eq!(text.columns.len(), 4);

    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns.len(), 4);
    let binary = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert!(binary.rows.is_empty());
    assert_eq!(binary.columns.len(), 4);
}

#[test]
fn static_literal_metadata_survives_prepared_reprepare() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute("CREATE TABLE reprepare_metadata (id INT)")
        .unwrap();
    let sql = "SELECT *, 0001 AS literal_value FROM reprepare_metadata LIMIT 0";
    let expected = (
        MYSQL_TYPE_LONGLONG,
        MYSQL_BINARY_COLLATION,
        5,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        0,
    );
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    adapter
        .connection
        .execute("ALTER TABLE reprepare_metadata ADD COLUMN ignored TEXT")
        .unwrap();
    let result = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert!(result.rows.is_empty());
    assert_eq!(result.columns.len(), 3);
    assert_eq!(
        (
            result.columns[2].column_type,
            result.columns[2].character_set,
            result.columns[2].column_length,
            result.columns[2].flags,
            result.columns[2].decimals,
        ),
        expected
    );
}

#[test]
fn static_literal_metadata_survives_prepared_reprepare_with_rows() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute("CREATE TABLE reprepare_rows (id INT)")
        .unwrap();
    adapter
        .connection
        .execute("INSERT INTO reprepare_rows (id) VALUES (7)")
        .unwrap();
    let sql = "SELECT *, 0001 AS literal_value FROM reprepare_rows";
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    adapter
        .connection
        .execute("ALTER TABLE reprepare_rows ADD COLUMN ignored TEXT")
        .unwrap();
    let result = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(result.columns.len(), 3);
    assert_eq!(
        result.rows,
        vec![vec![
            BinaryResultValue::Integer(7),
            BinaryResultValue::Null,
            BinaryResultValue::Integer(1),
        ]]
    );
    assert_eq!(result.columns[2].column_length, 5);
    assert_eq!(
        result.columns[2].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
}

#[test]
fn declared_integer_text_metadata_preserves_mysql_wire_widths() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute(
            "CREATE TABLE text_integer_widths (tiny TINYINT, small SMALLINT, int_value INT, integer_value INTEGER, big BIGINT)",
        )
        .unwrap();
    adapter
        .connection
        .execute(
            "INSERT INTO text_integer_widths (tiny, small, int_value, integer_value, big) VALUES (-128, -32768, -2147483648, -2147483648, -9223372036854775808), (127, 32767, 2147483647, 2147483647, 9223372036854775807), (NULL, NULL, NULL, NULL, NULL)",
        )
        .unwrap();

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT tiny, small, int_value, integer_value, big FROM text_integer_widths")
        .unwrap()
    else {
        panic!("declared integer query must produce a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length))
            .collect::<Vec<_>>(),
        [
            (MYSQL_TYPE_TINY, 4),
            (MYSQL_TYPE_SHORT, 6),
            (MYSQL_TYPE_LONG, 11),
            (MYSQL_TYPE_LONG, 11),
            (MYSQL_TYPE_LONGLONG, 20),
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Some(b"-128".to_vec()),
                Some(b"-32768".to_vec()),
                Some(b"-2147483648".to_vec()),
                Some(b"-2147483648".to_vec()),
                Some(b"-9223372036854775808".to_vec()),
            ],
            vec![
                Some(b"127".to_vec()),
                Some(b"32767".to_vec()),
                Some(b"2147483647".to_vec()),
                Some(b"2147483647".to_vec()),
                Some(b"9223372036854775807".to_vec()),
            ],
            vec![None, None, None, None, None],
        ]
    );
}

#[test]
fn mediumint_text_metadata_preserves_boundaries_and_nulls() {
    let mut adapter = adapter();
    adapter
        .connection
        .execute("CREATE TABLE text_mediumint (value MEDIUMINT)")
        .unwrap();
    adapter
        .connection
        .execute("INSERT INTO text_mediumint (value) VALUES (-8388608), (8388607), (NULL)")
        .unwrap();

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT value FROM text_mediumint")
        .unwrap()
    else {
        panic!("MEDIUMINT query must produce a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length))
            .collect::<Vec<_>>(),
        [(MYSQL_TYPE_INT24, 9)]
    );
    assert_eq!(
        result.rows,
        [
            vec![Some(b"-8388608".to_vec())],
            vec![Some(b"8388607".to_vec())],
            vec![None],
        ]
    );
}

#[test]
fn declared_type_metadata_normalizes_case_and_falls_back_for_unknown_types() {
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("tInYiNt"), Some("INTEGER")),
        Some(MYSQL_TYPE_TINY)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("sMaLlInT"), Some("INTEGER")),
        Some(MYSQL_TYPE_SHORT)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("mEdIuMiNt"), Some("INTEGER")),
        Some(MYSQL_TYPE_INT24)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("InTeGeR"), Some("INTEGER")),
        Some(MYSQL_TYPE_LONG)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("iNt"), Some("INTEGER")),
        Some(MYSQL_TYPE_LONG)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("bIgInT"), Some("INTEGER")),
        Some(MYSQL_TYPE_LONGLONG)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("CUSTOM_INTEGER"), Some("INTEGER")),
        Some(MYSQL_TYPE_LONGLONG)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("VARCHAR(32)"), Some("TEXT")),
        Some(MYSQL_TYPE_VAR_STRING)
    );
    assert_eq!(
        mysql_type_for_declared_or_inferred(Some("CUSTOM_INTEGER"), None),
        None
    );
}

#[test]
fn smallint_metadata_uses_mysql_short_type() {
    assert_eq!(mysql_type_for_name("SMALLINT"), Some(MYSQL_TYPE_SHORT));
}

#[test]
fn mediumint_metadata_uses_mysql_int24_type_and_length() {
    assert_eq!(mysql_type_for_name("MEDIUMINT"), Some(MYSQL_TYPE_INT24));
    assert_eq!(
        column_definition("value".to_owned(), MYSQL_TYPE_INT24).column_length,
        9
    );
}

#[test]
fn prepared_integer_name_mapping_distinguishes_declared_and_inferred_integer() {
    assert_eq!(mysql_type_for_name("TINYINT"), Some(MYSQL_TYPE_TINY));
    assert_eq!(mysql_type_for_name("INT"), Some(MYSQL_TYPE_LONG));
    assert_eq!(mysql_type_for_name("INTEGER"), Some(MYSQL_TYPE_LONGLONG));
    assert_eq!(mysql_type_for_name("BIGINT"), Some(MYSQL_TYPE_LONGLONG));
    let adapter = adapter();
    adapter
        .connection
        .execute("CREATE TABLE integer_sources (integer_value INTEGER)")
        .unwrap();
    let metadata = adapter
        .connection
        .prepare_checked_statement("SELECT integer_value, 1 AS literal_value FROM integer_sources")
        .unwrap();
    let type_metadata = adapter
        .connection
        .prepared_statement_result_column_type_metadata(metadata.statement_id)
        .unwrap();
    assert_eq!(
        mysql_type_for_prepared_column(&metadata.result_columns[0], &type_metadata[0]),
        Some(MYSQL_TYPE_LONG)
    );
    assert_eq!(
        mysql_type_for_prepared_column(&metadata.result_columns[1], &type_metadata[1]),
        Some(MYSQL_TYPE_LONGLONG)
    );
}

#[test]
fn bigint_metadata_uses_mysql_integer_type() {
    assert_eq!(mysql_type_for_name("BIGINT"), Some(MYSQL_TYPE_LONGLONG));
}

#[test]
fn unsupported_query_and_init_db_are_typed_denials() {
    let mut adapter = adapter();
    assert_eq!(
        adapter.execute_query("INSERT INTO users VALUES (1)"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_init_db("users"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("SELECT ?"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[cfg(unix)]
#[test]
fn authorized_adapter_selects_with_init_db_and_requires_a_selection_for_query() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([7; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    assert_eq!(
        adapter.execute_query("SELECT id FROM records"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    assert_eq!(
        adapter.execute_init_db("REPORTS"),
        Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
    );

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT id, label FROM records")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert_eq!(
        result.rows,
        vec![vec![Some(b"7".to_vec()), Some(b"kept".to_vec())]]
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn authorized_adapter_serves_bootstrap_without_database_or_query_authorization() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .with_bootstrap_settings(8192, Duration::from_millis(500))
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([30; 32]),
        ))
        .unwrap();

    adapter.authorize_connection().unwrap();
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT @@max_allowed_packet,@@wait_timeout")
        .unwrap()
    else {
        panic!("driver bootstrap query must produce a result set");
    };
    assert_eq!(
        result.rows,
        vec![vec![Some(b"8192".to_vec()), Some(b"1".to_vec())]]
    );
    assert_eq!(result.columns.len(), 2);
    assert!(result
        .columns
        .iter()
        .all(|column| column.column_type == MYSQL_TYPE_LONGLONG));
    assert_eq!(
        authorizer.actions(),
        vec![RecordedDatabaseAction::Connect(None)]
    );

    assert_eq!(
        adapter.execute_query("SELECT 1"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    assert_eq!(
        authorizer.actions(),
        vec![RecordedDatabaseAction::Connect(None)]
    );
}

#[cfg(unix)]
#[test]
fn authorized_unknown_system_variables_remain_unsupported_after_selection() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([31; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_query("SELECT @@socket,@@wait_timeout"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[cfg(unix)]
#[test]
fn authorization_hides_existing_and_missing_databases_before_catalog_lookup() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Err(AuthorizationError::Denied),
        Err(AuthorizationError::Unavailable),
    ]));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([8; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_init_db("reports"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        adapter.execute_init_db("missing"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Connect(Some("missing".to_owned())),
        ]
    );
}

#[cfg(unix)]
#[test]
fn failed_init_db_keeps_the_previous_database_selected() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([9; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_init_db("missing"),
        Err(FrontendErrorKind::UnknownDatabase)
    );
    assert!(matches!(
        adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
}

#[cfg(unix)]
#[test]
fn denied_database_switch_keeps_the_previous_database_selected() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Ok(()),
        Err(AuthorizationError::Denied),
        Ok(()),
    ]));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([13; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_init_db("archive"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert!(matches!(
        adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Connect(Some("archive".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn every_query_is_reauthorized_after_database_selection() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Ok(()),
        Ok(()),
        Err(AuthorizationError::Denied),
    ]));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([10; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    assert!(matches!(
        adapter.execute_query("SELECT 1"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert_eq!(
        adapter.execute_query("SELECT 1"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn admin_queries_authorize_canonical_names_before_typed_execution() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([14; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("CREATE DATABASE Archive;"),
        Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
    );
    assert_eq!(
        adapter.execute_query("USE ARCHIVE"),
        Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
    );
    assert_eq!(
        adapter.execute_query("SHOW DATABASES"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: vec![database_list_column()],
            rows: vec![
                vec![Some(b"archive".to_vec())],
                vec![Some(b"reports".to_vec())],
            ],
            warnings: 0,
            status_flags: 0x0002,
        }))
    );
    assert_eq!(
        adapter.execute_query("DROP DATABASE ARCHIVE"),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    assert_eq!(
        adapter.execute_query("DROP DATABASE REPORTS"),
        Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
    );
    assert_eq!(catalog.list().unwrap(), vec![String::from("archive")]);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Create(String::from("archive")),
            RecordedDatabaseAction::Connect(Some(String::from("archive"))),
            RecordedDatabaseAction::List,
            RecordedDatabaseAction::Drop(String::from("archive")),
            RecordedDatabaseAction::Drop(String::from("reports")),
        ]
    );
}

#[cfg(unix)]
#[test]
fn admin_authorization_hides_existence_and_preserves_catalog_state() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Err(AuthorizationError::Denied),
        Err(AuthorizationError::Unavailable),
        Err(AuthorizationError::Denied),
    ]));
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([15; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("CREATE DATABASE REPORTS"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        adapter.execute_query("DROP DATABASE MISSING"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        adapter.execute_query("SHOW DATABASES"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(catalog.list().unwrap(), vec![String::from("reports")]);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Create(String::from("reports")),
            RecordedDatabaseAction::Drop(String::from("missing")),
            RecordedDatabaseAction::List,
        ]
    );
}

#[cfg(unix)]
#[test]
fn authorized_admin_catalog_errors_keep_their_typed_categories() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([21; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    adapter.execute_query("CREATE DATABASE Archive").unwrap();
    assert_eq!(
        adapter.execute_query("CREATE DATABASE ARCHIVE"),
        Err(FrontendErrorKind::DuplicateDatabase)
    );
    assert_eq!(
        adapter.execute_query("DROP DATABASE MISSING"),
        Err(FrontendErrorKind::UnknownDatabase)
    );
    assert_eq!(
        adapter.execute_query("USE MISSING"),
        Err(FrontendErrorKind::UnknownDatabase)
    );
    assert_eq!(
        catalog.list().unwrap(),
        vec![String::from("archive"), String::from("reports")]
    );
}

#[cfg(unix)]
#[test]
fn sql_use_denial_keeps_the_previous_database_selected() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Ok(()),
        Err(AuthorizationError::Denied),
        Ok(()),
    ]));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([16; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    adapter.execute_query("USE REPORTS").unwrap();
    assert_eq!(
        adapter.execute_query("USE MISSING"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert!(matches!(
        adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some(String::from("reports"))),
            RecordedDatabaseAction::Connect(Some(String::from("missing"))),
            RecordedDatabaseAction::Query(String::from("reports")),
        ]
    );
}

#[cfg(unix)]
#[test]
fn denied_drop_does_not_reveal_that_the_selected_database_is_busy() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Ok(()),
        Err(AuthorizationError::Denied),
        Ok(()),
    ]));
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([22; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE REPORTS").unwrap();

    assert_eq!(
        adapter.execute_query("DROP DATABASE REPORTS"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert!(matches!(
        adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert_eq!(catalog.list().unwrap(), vec![String::from("reports")]);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some(String::from("reports"))),
            RecordedDatabaseAction::Drop(String::from("reports")),
            RecordedDatabaseAction::Query(String::from("reports")),
        ]
    );
}

#[cfg(unix)]
#[test]
fn malformed_admin_is_syntax_but_other_admin_sql_is_unsupported() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([17; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("CREATE DATABASE"),
        Err(FrontendErrorKind::Syntax)
    );
    assert_eq!(
        adapter.execute_query("SHOW DATABASES trailing"),
        Err(FrontendErrorKind::Syntax)
    );
    assert_eq!(
        adapter.execute_query("CREATE TABLE records (id INT)"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    adapter.execute_query("USE REPORTS").unwrap();
    assert_eq!(
        adapter.execute_query("SHOW COLUMNS"),
        Err(FrontendErrorKind::Syntax)
    );
}

#[cfg(unix)]
#[test]
fn show_columns_requires_selection_and_reauthorizes_the_selected_database() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([35; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("SHOW COLUMNS FROM records"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    assert_eq!(
        authorizer.actions(),
        vec![RecordedDatabaseAction::Connect(None)]
    );

    adapter.execute_query("USE REPORTS").unwrap();
    assert_eq!(
        adapter.execute_query("SHOW COLUMNS FROM RECORDS;"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_columns_columns(),
            rows: vec![
                vec![
                    Some(b"id".to_vec()),
                    Some(b"int".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"label".to_vec()),
                    Some(b"text".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
            ],
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );

    assert_eq!(
        adapter.execute_query("DESCRIBE records"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_columns_columns(),
            rows: vec![
                vec![
                    Some(b"id".to_vec()),
                    Some(b"int".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"label".to_vec()),
                    Some(b"text".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
            ],
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn show_columns_requires_query_or_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Err(AuthorizationError::Denied)],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([36; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();

    assert_eq!(
        adapter.execute_query("SHOW COLUMNS FROM records"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn show_columns_and_describe_fall_back_to_granted_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
        ],
        [Ok(()), Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([46; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for sql in ["SHOW COLUMNS FROM RECORDS", "DESCRIBE records"] {
        assert!(matches!(
            adapter.execute_query(sql),
            Ok(CommandExecutionResult::ResultSet(_))
        ));
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn show_columns_and_describe_direct_view_preserve_source_nullability() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
        ],
        [Ok(()), Ok(())],
    ));
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("reports").unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE TABLE strict_records (id INT NOT NULL, label TEXT)")
        .unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE VIEW strict_records_view AS SELECT id, label FROM strict_records")
        .unwrap();
    drop(seed);

    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([48; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    let expected = |rows| {
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_columns_columns(),
            rows,
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    };
    for sql in [
        "SHOW COLUMNS FROM strict_records_view",
        "DESCRIBE strict_records_view",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            expected(vec![
                vec![
                    Some(b"id".to_vec()),
                    Some(b"int".to_vec()),
                    Some(b"NO".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
                vec![
                    Some(b"label".to_vec()),
                    Some(b"text".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
            ])
        );
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "strict_records_view".to_owned(),
            },
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "strict_records_view".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn unavailable_show_columns_authorization_does_not_try_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
        [Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([47; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_query("SHOW COLUMNS FROM records"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn show_columns_encodes_typed_default_values() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([37; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE metadata (id INT NOT NULL UNIQUE DEFAULT 1, name TEXT DEFAULT 'guest', payload BLOB, tiny TINYINT, small SMALLINT, maybe INT DEFAULT NULL)",
        )
        .unwrap();
    let columns = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("metadata").unwrap())
        .unwrap();
    let result =
        show_columns_result_to_execution_result(columns, SERVER_STATUS_AUTOCOMMIT).unwrap();

    let CommandExecutionResult::ResultSet(result) = result else {
        panic!("SHOW COLUMNS must produce a result set");
    };
    assert_eq!(result.columns, show_columns_columns());
    assert_eq!(
        result.rows,
        vec![
            vec![
                Some(b"id".to_vec()),
                Some(b"int".to_vec()),
                Some(b"NO".to_vec()),
                Some(b"UNI".to_vec()),
                Some(b"1".to_vec()),
                Some(Vec::new()),
            ],
            vec![
                Some(b"name".to_vec()),
                Some(b"text".to_vec()),
                Some(b"YES".to_vec()),
                Some(Vec::new()),
                Some(b"guest".to_vec()),
                Some(Vec::new()),
            ],
            vec![
                Some(b"payload".to_vec()),
                Some(b"blob".to_vec()),
                Some(b"YES".to_vec()),
                Some(Vec::new()),
                None,
                Some(Vec::new()),
            ],
            vec![
                Some(b"tiny".to_vec()),
                Some(b"tinyint".to_vec()),
                Some(b"YES".to_vec()),
                Some(Vec::new()),
                None,
                Some(Vec::new()),
            ],
            vec![
                Some(b"small".to_vec()),
                Some(b"smallint".to_vec()),
                Some(b"YES".to_vec()),
                Some(Vec::new()),
                None,
                Some(Vec::new()),
            ],
            vec![
                Some(b"maybe".to_vec()),
                Some(b"int".to_vec()),
                Some(b"YES".to_vec()),
                Some(Vec::new()),
                None,
                Some(Vec::new()),
            ],
        ]
    );
    assert_eq!(
        show_column_default_value(Some(&MySqlColumnDefault::Boolean(true))),
        Ok(Some(b"1".to_vec()))
    );
    assert_eq!(
        show_column_default_value(Some(&MySqlColumnDefault::Boolean(false))),
        Ok(Some(b"0".to_vec()))
    );
    assert_eq!(
        show_column_default_value(Some(&MySqlColumnDefault::Integer {
            text: "+42".to_owned(),
            value: 42,
        })),
        Ok(Some(b"42".to_vec()))
    );
    assert_eq!(
        show_column_default_value(Some(&MySqlColumnDefault::Text("it's".to_owned()))),
        Ok(Some(b"it's".to_vec()))
    );
    assert_eq!(
        show_column_default_value(Some(&MySqlColumnDefault::Null)),
        Ok(None)
    );
}

#[cfg(unix)]
#[test]
fn show_columns_reports_mediumint_as_lowercase_type_name() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([41; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();
    adapter
        .execute_query("CREATE TABLE medium_columns (value MEDIUMINT NULL)")
        .unwrap();

    assert_eq!(
        adapter.execute_query("SHOW COLUMNS FROM medium_columns"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_columns_columns(),
            rows: vec![vec![
                Some(b"value".to_vec()),
                Some(b"mediumint".to_vec()),
                Some(b"YES".to_vec()),
                Some(Vec::new()),
                None,
                Some(Vec::new()),
            ]],
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
}

#[cfg(unix)]
#[test]
fn show_columns_encodes_primary_and_auto_increment_metadata() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([40; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE key_metadata (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
        )
        .unwrap();

    assert_eq!(
        adapter.execute_query("SHOW COLUMNS FROM key_metadata"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_columns_columns(),
            rows: vec![
                vec![
                    Some(b"id".to_vec()),
                    Some(b"int".to_vec()),
                    Some(b"NO".to_vec()),
                    Some(b"PRI".to_vec()),
                    None,
                    Some(b"auto_increment".to_vec()),
                ],
                vec![
                    Some(b"label".to_vec()),
                    Some(b"text".to_vec()),
                    Some(b"YES".to_vec()),
                    Some(Vec::new()),
                    None,
                    Some(Vec::new()),
                ],
            ],
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
    assert_eq!(show_column_extra(""), Ok(b"".as_slice()));
    assert_eq!(
        show_column_extra("AUTO_INCREMENT"),
        Ok(b"auto_increment".as_slice())
    );
    assert_eq!(
        show_column_extra("unexpected"),
        Err(FrontendErrorKind::Internal)
    );
}

#[cfg(unix)]
#[test]
fn show_columns_maps_metadata_failures_to_safe_frontend_categories() {
    assert_eq!(
        column_metadata_error_kind(MySqlColumnMetadataError::TableNotFound),
        FrontendErrorKind::MissingObject
    );
    assert_eq!(
        column_metadata_error_kind(MySqlColumnMetadataError::UnsupportedDefinition),
        FrontendErrorKind::Unsupported
    );
    assert_eq!(
        column_metadata_error_kind(MySqlColumnMetadataError::CorruptDefinition),
        FrontendErrorKind::Internal
    );
    assert_eq!(
        column_metadata_error_kind(MySqlColumnMetadataError::Engine(LimboError::TooBig)),
        FrontendErrorKind::Internal
    );
}

#[cfg(unix)]
#[test]
fn show_columns_has_bounded_protocol_result() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([38; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();
    let codec = PacketCodec::new(4096).unwrap();
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(b"SHOW COLUMNS FROM records");
    let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
    let mut connection = ready_connection();

    let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
    assert_eq!(
        frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
    );
    assert_eq!(
        crate::ColumnCountPacket::decode(codec, &frames[0])
            .unwrap()
            .column_count,
        6
    );
    let definitions = (1..=6)
        .map(|index| crate::ColumnDefinitionPacket::decode(codec, &frames[index]).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        definitions
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["Field", "Type", "Null", "Key", "Default", "Extra"]
    );
    assert!(definitions.iter().all(|column| {
        column.column_type == MYSQL_TYPE_VAR_STRING
            && column.character_set == u16::from(DEFAULT_UTF8MB4_COLLATION)
    }));

    for index in [7, 10] {
        assert!(matches!(
            crate::ResultTerminatorPacket::decode(
                codec,
                &frames[index],
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            )
            .unwrap(),
            crate::ResultTerminatorPacket::Eof(_)
        ));
    }
    let first_row = crate::TextRowPacket::decode(codec, &frames[8], 6).unwrap();
    assert_eq!(first_row.values[0], TextRowValue::Bytes(b"id"));
    assert_eq!(first_row.values[1], TextRowValue::Bytes(b"int"));
    assert_eq!(first_row.values[2], TextRowValue::Bytes(b"YES"));
    assert_eq!(first_row.values[3], TextRowValue::Bytes(b""));
    assert_eq!(first_row.values[4], TextRowValue::Null);
    assert_eq!(first_row.values[5], TextRowValue::Bytes(b""));
    let second_row = crate::TextRowPacket::decode(codec, &frames[9], 6).unwrap();
    assert_eq!(second_row.values[0], TextRowValue::Bytes(b"label"));
    assert_eq!(second_row.values[1], TextRowValue::Bytes(b"text"));
    assert_eq!(second_row.values[2], TextRowValue::Bytes(b"YES"));
    assert_eq!(second_row.values[3], TextRowValue::Bytes(b""));
    assert_eq!(second_row.values[4], TextRowValue::Null);
    assert_eq!(second_row.values[5], TextRowValue::Bytes(b""));
}

#[cfg(unix)]
#[test]
fn show_columns_rejects_unencodable_results_before_dispatch() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([39; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();

    adapter
        .execute_query("CREATE TABLE bounded (value TEXT)")
        .unwrap();
    let bounded = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("bounded").unwrap())
        .unwrap();
    assert_eq!(
        show_columns_result_to_execution_result(
            vec![bounded[0].clone(); MAX_DISPATCH_RESULT_ROWS + 1],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );

    let oversized_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH);
    adapter
        .execute_query(&format!(
            "CREATE TABLE oversized_default (value TEXT DEFAULT '{oversized_default}')"
        ))
        .unwrap();
    let oversized_default = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("oversized_default").unwrap())
        .unwrap();
    assert_eq!(
        show_columns_result_to_execution_result(oversized_default, SERVER_STATUS_AUTOCOMMIT,),
        Err(FrontendErrorKind::Internal)
    );

    let packet_bound_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH - 19);
    adapter
        .execute_query(&format!(
            "CREATE TABLE packet_bound (value TEXT DEFAULT '{packet_bound_default}')"
        ))
        .unwrap();
    let packet_bound = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("packet_bound").unwrap())
        .unwrap();
    assert_eq!(
        show_columns_result_to_execution_result(packet_bound, SERVER_STATUS_AUTOCOMMIT),
        Err(FrontendErrorKind::Internal)
    );

    let long_name = "x".repeat(2_000);
    adapter
        .execute_query(&format!("CREATE TABLE retained (`{long_name}` TEXT)"))
        .unwrap();
    let retained = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("retained").unwrap())
        .unwrap();
    assert_eq!(
        show_columns_result_to_execution_result(
            vec![retained[0].clone(); MAX_DISPATCH_RESULT_ROWS],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
}

#[cfg(unix)]
#[test]
fn show_full_tables_filters_grants_and_drop_view_requires_query_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [
            Ok(()),
            Ok(()),
            Err(AuthorizationError::Denied),
            Err(AuthorizationError::Denied),
        ],
        [Err(AuthorizationError::Denied), Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([82; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    adapter
        .session
        .connection()
        .unwrap()
        .execute("CREATE VIEW alpha AS SELECT id FROM records")
        .unwrap();
    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SHOW FULL TABLES").unwrap()
    else {
        panic!("SHOW must return rows");
    };
    assert_eq!(
        result.rows,
        vec![vec![
            Some(b"records".to_vec()),
            Some(b"BASE TABLE".to_vec())
        ]]
    );
    assert_eq!(
        adapter.execute_query("DROP VIEW alpha"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        adapter
            .session
            .connection()
            .unwrap()
            .list_tables()
            .unwrap()
            .len(),
        2
    );
}

#[cfg(unix)]
#[test]
fn drop_table_requires_query_permission_without_table_select_fallback() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([83; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_query("DROP TABLE records"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
    assert_eq!(
        adapter
            .session
            .connection()
            .unwrap()
            .list_tables()
            .unwrap()
            .len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn show_full_tables_has_typed_bounded_metadata_and_requires_selection() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([81; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    assert_eq!(
        adapter.execute_query("SHOW FULL TABLES"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    adapter.execute_query("SET sql_notes = 0").unwrap();
    adapter.execute_query("USE reports").unwrap();
    let CommandExecutionResult::ResultSet(notes) =
        adapter.execute_query("SELECT @@sql_notes").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(notes.rows, vec![vec![Some(b"0".to_vec())]]);
    adapter
        .execute_query("CREATE VIEW records_view AS SELECT id FROM records")
        .unwrap();
    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SHOW FULL TABLES").unwrap()
    else {
        panic!("SHOW must return rows");
    };
    assert_eq!(
        result.rows,
        vec![
            vec![Some(b"records".to_vec()), Some(b"BASE TABLE".to_vec())],
            vec![Some(b"records_view".to_vec()), Some(b"VIEW".to_vec())]
        ]
    );
    assert_eq!(result.columns[0].name, "Tables_in_reports");
    assert_eq!(result.columns[1].name, "Table_type");
    assert_eq!(result.columns[1].column_type, MYSQL_TYPE_STRING);
    assert_eq!(result.columns[1].column_length, 44);
    assert_eq!(
        result.columns[1].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG
    );
    assert_eq!(result.columns[1].catalog, "def");
    assert_eq!(result.columns[1].table, "TABLES");
    assert_eq!(result.columns[1].original_table, "tables");
    let tables = adapter.session.connection().unwrap().list_tables().unwrap();
    assert_eq!(
        show_full_tables_result_to_execution_result(
            "reports",
            vec![tables[0].clone(); MAX_DISPATCH_RESULT_ROWS + 1],
            SERVER_STATUS_AUTOCOMMIT
        ),
        Err(FrontendErrorKind::Internal)
    );
    adapter.execute_reset_connection().unwrap();
    let CommandExecutionResult::ResultSet(notes) =
        adapter.execute_query("SELECT @@sql_notes").unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(notes.rows, vec![vec![Some(b"1".to_vec())]]);
}

#[cfg(unix)]
#[test]
fn show_tables_requires_a_selection_and_reauthorizes_the_selected_database() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([32; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("SHOW TABLES"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    assert_eq!(
        authorizer.actions(),
        vec![RecordedDatabaseAction::Connect(None)]
    );

    adapter.execute_query("USE REPORTS").unwrap();
    assert_eq!(
        adapter.execute_query("SHOW TABLES;"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: vec![show_tables_column("reports")],
            rows: vec![vec![Some(b"records".to_vec())]],
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_tables_requires_selection_and_returns_sorted_user_objects() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([41; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    let query = "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME";
    assert_eq!(
        adapter.execute_query(query),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    adapter.execute_init_db("REPORTS").unwrap();
    let connection = adapter.session.connection().unwrap();
    connection.execute("CREATE TABLE zeta (id INT)").unwrap();
    connection
        .execute("CREATE VIEW alpha AS SELECT id FROM records")
        .unwrap();

    let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap() else {
        panic!("information_schema.TABLES must return a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.original_name.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.schema.as_str(),
                column.catalog.as_str(),
                column.column_type,
                column.character_set,
                column.column_length,
                column.flags,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "TABLE_SCHEMA",
                "TABLE_SCHEMA",
                "TABLES",
                "schemata",
                "information_schema",
                "def",
                MYSQL_TYPE_VAR_STRING,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                256,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "TABLE_NAME",
                "TABLE_NAME",
                "TABLES",
                "tables",
                "information_schema",
                "def",
                MYSQL_TYPE_VAR_STRING,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                256,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "TABLE_TYPE",
                "TABLE_TYPE",
                "TABLES",
                "tables",
                "information_schema",
                "def",
                MYSQL_TYPE_STRING,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                44,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_BINARY_FLAG
                    | MYSQL_ENUM_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Some(b"reports".to_vec()),
                Some(b"alpha".to_vec()),
                Some(b"VIEW".to_vec()),
            ],
            vec![
                Some(b"reports".to_vec()),
                Some(b"records".to_vec()),
                Some(b"BASE TABLE".to_vec()),
            ],
            vec![
                Some(b"reports".to_vec()),
                Some(b"zeta".to_vec()),
                Some(b"BASE TABLE".to_vec()),
            ],
        ]
    );
    assert_eq!(result.warnings, 0);
    assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn show_index_returns_the_fifteen_columns_mysql_returns() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([46; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    assert_eq!(
        adapter.execute_query("SHOW INDEX FROM records"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    adapter.execute_init_db("reports").unwrap();

    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SHOW INDEX FROM records").unwrap()
    else {
        panic!("SHOW INDEX must return a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Table",
            "Non_unique",
            "Key_name",
            "Seq_in_index",
            "Column_name",
            "Collation",
            "Cardinality",
            "Sub_part",
            "Packed",
            "Null",
            "Index_type",
            "Comment",
            "Index_comment",
            "Visible",
            "Expression",
        ]
    );
    for row in &result.rows {
        assert_eq!(row.len(), 15);
        assert_eq!(row[0], Some(b"records".to_vec()));
        assert_eq!(row[5], Some(b"A".to_vec()));
        // Cardinality is a statistic Turso does not gather, and MySQL sends
        // NULL when it has none either.
        assert_eq!(row[6], None);
        assert_eq!(row[10], Some(b"BTREE".to_vec()));
        assert_eq!(row[13], Some(b"YES".to_vec()));
        assert_eq!(row[14], None);
    }

    // Every spelling reaches the same place, and the other catalog
    // commands still answer for themselves.
    for sql in ["SHOW KEYS FROM records", "SHOW INDEXES IN records"] {
        assert_eq!(
            adapter.execute_query(sql).unwrap(),
            adapter.execute_query("SHOW INDEX FROM records").unwrap(),
            "{sql}"
        );
    }
    assert_eq!(
        adapter.execute_query("SHOW INDEX FROM missing"),
        Err(FrontendErrorKind::MissingObject)
    );
    assert_eq!(
        adapter.execute_query("SHOW INDEX FROM archive.records"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[cfg(unix)]
#[test]
fn show_create_table_needs_a_selection_and_returns_the_mysql_ddl() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([43; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("SHOW CREATE TABLE records"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    adapter.execute_init_db("reports").unwrap();

    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query("SHOW CREATE TABLE records").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.character_set,
                column.column_length,
                column.decimals,
                column.flags,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "Table",
                MYSQL_TYPE_VAR_STRING,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                256,
                31,
                MYSQL_NOT_NULL_FLAG,
            ),
            (
                "Create Table",
                MYSQL_TYPE_VAR_STRING,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                4096,
                31,
                MYSQL_NOT_NULL_FLAG,
            ),
        ]
    );
    let [row] = result.rows.as_slice() else {
        panic!("SHOW CREATE TABLE must return exactly one row");
    };
    assert_eq!(row[0], Some(b"records".to_vec()));
    assert_eq!(
        String::from_utf8(row[1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `records` (\n",
            "  `id` int DEFAULT NULL,\n",
            "  `label` text\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    assert_eq!(
        adapter.execute_query("SHOW CREATE TABLE missing"),
        Err(FrontendErrorKind::MissingObject)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_qualifier_naming_the_selected_database_is_taken_and_any_other_is_refused() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([45; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    // The qualifier clients write right after USE is redundant, and MySQL
    // answers it exactly as it answers the unqualified form.
    let qualified = adapter
        .execute_query("SHOW CREATE TABLE reports.records")
        .unwrap();
    let plain = adapter.execute_query("SHOW CREATE TABLE records").unwrap();
    assert_eq!(qualified, plain);
    assert_eq!(
        adapter.execute_query("SHOW CREATE TABLE REPORTS.records"),
        Ok(plain)
    );

    for sql in [
        "SHOW CREATE TABLE archive.records",
        "SHOW COLUMNS FROM archive.records",
        "DESCRIBE archive.records",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
}

#[cfg(unix)]
#[test]
fn show_create_table_authorizes_before_catalog_lookup() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
        [Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([44; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_query("SHOW CREATE TABLE records"),
        Err(FrontendErrorKind::AccessDenied)
    );
    // The catalog was never read: the run stops at the denied Query.
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_tables_authorizes_before_catalog_lookup() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
        [Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([42; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    assert_eq!(
        adapter.execute_query(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME"
        ),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_tables_filters_rows_by_granted_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Err(AuthorizationError::Denied), Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([48; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    adapter
        .session
        .connection()
        .unwrap()
        .execute("CREATE TABLE alpha (id INT)")
        .unwrap();

    let query = "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME";
    let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap() else {
        panic!("information_schema.TABLES must return a result set");
    };
    assert_eq!(
        result.rows,
        vec![vec![
            Some(b"reports".to_vec()),
            Some(b"records".to_vec()),
            Some(b"BASE TABLE".to_vec()),
        ]]
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "alpha".to_owned(),
            },
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_tables_rejects_malformed_queries_without_falling_through() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([43; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for query in [
        "SELECT * FROM information_schema.TABLES",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_SCHEMA",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME DESC",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME; SELECT 1",
    ] {
        assert_eq!(
            adapter.execute_query(query),
            Err(FrontendErrorKind::Syntax),
            "malformed information_schema.TABLES query must not execute as a normal SELECT: {query}"
        );
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_returns_exact_metadata_and_rows() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("metadata").unwrap();
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("metadata").unwrap();
    seed.connection()
        .unwrap()
        .execute_schema_ddl(
            "CREATE TABLE records (id INT NOT NULL, label TEXT, value MEDIUMINT NULL)",
        )
        .unwrap();
    drop(seed);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([50; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("metadata").unwrap();

    let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
    let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap() else {
        panic!("information_schema.COLUMNS must return a result set");
    };
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.catalog.as_str(),
                column.schema.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.name.as_str(),
                column.original_name.as_str(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "def",
                "information_schema",
                "COLUMNS",
                "",
                "COLUMN_NAME",
                "COLUMN_NAME",
            ),
            (
                "def",
                "information_schema",
                "COLUMNS",
                "columns",
                "ORDINAL_POSITION",
                "ORDINAL_POSITION",
            ),
            (
                "def",
                "information_schema",
                "COLUMNS",
                "columns",
                "COLUMN_DEFAULT",
                "COLUMN_DEFAULT",
            ),
            (
                "def",
                "information_schema",
                "COLUMNS",
                "",
                "IS_NULLABLE",
                "IS_NULLABLE",
            ),
            (
                "def",
                "information_schema",
                "COLUMNS",
                "columns",
                "COLUMN_TYPE",
                "COLUMN_TYPE",
            ),
            (
                "def",
                "information_schema",
                "COLUMNS",
                "columns",
                "COLUMN_KEY",
                "COLUMN_KEY",
            ),
            ("def", "information_schema", "COLUMNS", "", "EXTRA", "EXTRA",),
        ]
    );
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.character_set,
                column.column_length,
                column.column_type,
                column.flags,
                column.decimals,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "COLUMN_NAME",
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                256,
                MYSQL_TYPE_VAR_STRING,
                0,
                0,
            ),
            (
                "ORDINAL_POSITION",
                MYSQL_BINARY_COLLATION,
                10,
                MYSQL_TYPE_LONG,
                MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                0,
            ),
            (
                "COLUMN_DEFAULT",
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                262_140,
                MYSQL_TYPE_BLOB,
                MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
                0,
            ),
            (
                "IS_NULLABLE",
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                12,
                MYSQL_TYPE_VAR_STRING,
                MYSQL_NOT_NULL_FLAG,
                0,
            ),
            (
                "COLUMN_TYPE",
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                67_108_860,
                MYSQL_TYPE_BLOB,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_BLOB_FLAG
                    | MYSQL_BINARY_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG,
                0,
            ),
            (
                "COLUMN_KEY",
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                12,
                MYSQL_TYPE_STRING,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_BINARY_FLAG
                    | MYSQL_ENUM_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG,
                0,
            ),
            (
                "EXTRA",
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                1024,
                MYSQL_TYPE_VAR_STRING,
                0,
                0,
            ),
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Some(b"id".to_vec()),
                Some(b"1".to_vec()),
                None,
                Some(b"NO".to_vec()),
                Some(b"int".to_vec()),
                Some(Vec::new()),
                Some(Vec::new()),
            ],
            vec![
                Some(b"label".to_vec()),
                Some(b"2".to_vec()),
                None,
                Some(b"YES".to_vec()),
                Some(b"text".to_vec()),
                Some(Vec::new()),
                Some(Vec::new()),
            ],
            vec![
                Some(b"value".to_vec()),
                Some(b"3".to_vec()),
                None,
                Some(b"YES".to_vec()),
                Some(b"mediumint".to_vec()),
                Some(Vec::new()),
                Some(Vec::new()),
            ],
        ]
    );
    let codec = PacketCodec::new(4096).unwrap();
    for (index, column) in result.columns.iter().enumerate() {
        let frame = column.encode(codec, (index + 1) as u8).unwrap();
        let decoded = crate::ColumnDefinitionPacket::decode(codec, &frame).unwrap();
        assert_eq!(
            (
                decoded.sequence_id,
                decoded.catalog.as_str(),
                decoded.schema.as_str(),
                decoded.table.as_str(),
                decoded.original_table.as_str(),
                decoded.name.as_str(),
                decoded.original_name.as_str(),
                decoded.character_set,
                decoded.column_length,
                decoded.column_type,
                decoded.flags,
                decoded.decimals,
            ),
            (
                (index + 1) as u8,
                column.catalog.as_str(),
                column.schema.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.name.as_str(),
                column.original_name.as_str(),
                column.character_set,
                column.column_length,
                column.column_type,
                column.flags,
                column.decimals,
            )
        );
    }
    assert_eq!(result.warnings, 0);
    assert_eq!(result.status_flags, SERVER_STATUS_AUTOCOMMIT);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("metadata".to_owned())),
            RecordedDatabaseAction::Query("metadata".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_returns_the_requested_table_or_view() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("reports").unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE TABLE other (id BIGINT NOT NULL, note TEXT)")
        .unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE VIEW records_view AS SELECT id FROM records")
        .unwrap();
    drop(seed);

    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([58; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for (table, columns) in [
        ("other", ["id", "note"].as_slice()),
        ("records_view", ["id"].as_slice()),
        ("missing", &[] as &[&str]),
    ] {
        let query = format!(
            "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
        );
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(&query).unwrap()
        else {
            panic!("information_schema.COLUMNS must return a result set");
        };
        assert_eq!(result.rows.len(), columns.len());
        for (row, column) in result.rows.iter().zip(columns) {
            assert_eq!(row[0], Some(column.as_bytes().to_vec()));
        }
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_binds_lookup_to_the_selected_database() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("archive").unwrap();
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("archive").unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE TABLE records (archived_id INT NOT NULL)")
        .unwrap();
    drop(seed);

    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([60; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    for (database, columns) in [
        ("reports", ["id", "label"].as_slice()),
        ("archive", ["archived_id"].as_slice()),
    ] {
        adapter.execute_init_db(database).unwrap();
        let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap()
        else {
            panic!("information_schema.COLUMNS must return a result set");
        };
        assert_eq!(result.rows.len(), columns.len());
        for (row, column) in result.rows.iter().zip(columns) {
            assert_eq!(row[0], Some(column.as_bytes().to_vec()));
        }
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Connect(Some("archive".to_owned())),
            RecordedDatabaseAction::Query("archive".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_uses_granted_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Ok(())],
    ));
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("reports").unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE TABLE other (id BIGINT NOT NULL, note TEXT)")
        .unwrap();
    drop(seed);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([51; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'other' ORDER BY ORDINAL_POSITION";
    let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap() else {
        panic!("information_schema.COLUMNS must return a result set");
    };
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Some(b"id".to_vec()));
    assert_eq!(result.rows[1][0], Some(b"note".to_vec()));
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "other".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_denied_table_returns_empty_result() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Err(AuthorizationError::Denied)],
    ));
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("reports").unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE TABLE other (id BIGINT NOT NULL, note TEXT)")
        .unwrap();
    drop(seed);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([52; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'other' ORDER BY ORDINAL_POSITION";
    assert_eq!(
        adapter.execute_query(query),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: information_schema_columns_columns(),
            rows: Vec::new(),
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "other".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_unavailable_authorization_precedes_lookup() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Unavailable)],
        [Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([53; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
    assert_eq!(
        adapter.execute_query(query),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_missing_records_returns_empty_rows() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("archive").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([54; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("archive").unwrap();

    let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
    assert_eq!(
        adapter.execute_query(query),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: information_schema_columns_columns(),
            rows: Vec::new(),
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_rejects_unencodable_results_before_dispatch() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([57; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    adapter
        .execute_query("CREATE TABLE bounded (value TEXT)")
        .unwrap();
    let bounded = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("bounded").unwrap())
        .unwrap();
    assert_eq!(
        information_schema_columns_result_to_execution_result(
            vec![bounded[0].clone(); MAX_DISPATCH_RESULT_ROWS + 1],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );

    let oversized_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH + 1);
    adapter
        .execute_query(&format!(
            "CREATE TABLE oversized_default (value TEXT DEFAULT '{oversized_default}')"
        ))
        .unwrap();
    let oversized_default = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("oversized_default").unwrap())
        .unwrap();
    assert_eq!(
        information_schema_columns_result_to_execution_result(
            oversized_default,
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );

    let packet_bound_default = "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH - 19);
    adapter
        .execute_query(&format!(
            "CREATE TABLE packet_bound (value TEXT DEFAULT '{packet_bound_default}')"
        ))
        .unwrap();
    let packet_bound = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("packet_bound").unwrap())
        .unwrap();
    assert_eq!(
        information_schema_columns_result_to_execution_result(
            packet_bound,
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );

    let long_name = "x".repeat(2_000);
    adapter
        .execute_query(&format!("CREATE TABLE retained (`{long_name}` TEXT)"))
        .unwrap();
    let retained = adapter
        .session
        .connection()
        .unwrap()
        .list_columns(&MySqlTableName::parse("retained").unwrap())
        .unwrap();
    assert_eq!(
        information_schema_columns_result_to_execution_result(
            vec![retained[0].clone(); MAX_DISPATCH_RESULT_ROWS],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_rejects_malformed_queries_without_fallthrough() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([55; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for query in [
        "SELECT * FROM information_schema.COLUMNS",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records'",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY COLUMN_NAME",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION DESC",
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION; SELECT 1",
    ] {
        assert_eq!(
            adapter.execute_query(query),
            Err(FrontendErrorKind::Syntax),
            "malformed information_schema.COLUMNS query must fail closed: {query}"
        );
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_keeps_prepare_fail_closed() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([56; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    let query = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records' ORDER BY ORDINAL_POSITION";
    assert!(matches!(
        adapter.execute_stmt_prepare(query),
        Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
    ));
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn internal_catalog_selects_fail_closed_without_table_grant_fallback() {
    let mut decisions = vec![Ok(()), Ok(())];
    decisions.extend(std::iter::repeat_with(|| Err(AuthorizationError::Denied)).take(6));
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        decisions,
        vec![Ok(()); 6],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([44; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for query in [
        "SELECT name FROM sqlite_schema",
        "SELECT name FROM sqlite_master",
        "SELECT name FROM sqlite_sequence",
        "SELECT name FROM __turso_internal_types",
        "SELECT name FROM `SQLite_Schema`",
        "/* hidden */ SELECT name FROM sqlite_schema",
    ] {
        assert_eq!(
            adapter.execute_query(query),
            Err(FrontendErrorKind::AccessDenied),
            "internal catalog query must be rejected before authorization fallback: {query}"
        );
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_columns_hides_internal_tables() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([59; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    for table in ["sqlite_schema", "sqlite_master", "__turso_internal_types"] {
        let query = format!(
            "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, COLUMN_KEY, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
        );
        let CommandExecutionResult::ResultSet(result) = adapter.execute_query(&query).unwrap()
        else {
            panic!("information_schema.COLUMNS must return a result set");
        };
        assert!(result.rows.is_empty(), "internal table leaked: {table}");
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_tables_rejects_results_over_dispatch_bounds() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([45; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    let tables = adapter.session.connection().unwrap().list_tables().unwrap();

    assert_eq!(
        information_schema_tables_result_to_execution_result(
            &"x".repeat(MAX_TEXT_ROW_VALUE_LENGTH + 1),
            tables.clone(),
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
    assert_eq!(
        information_schema_tables_result_to_execution_result(
            &"x".repeat(MAX_TEXT_ROW_VALUE_LENGTH - 19),
            tables.clone(),
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );

    assert_eq!(
        information_schema_tables_result_to_execution_result(
            "reports",
            tables
                .iter()
                .cloned()
                .cycle()
                .take(MAX_DISPATCH_RESULT_ROWS + 1),
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
}

#[cfg(unix)]
#[test]
fn show_tables_requires_query_or_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Err(AuthorizationError::Denied)],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([34; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();

    assert_eq!(
        adapter.execute_query("SHOW TABLES"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: vec![show_tables_column("reports")],
            rows: Vec::new(),
            warnings: 0,
            status_flags: SERVER_STATUS_AUTOCOMMIT,
        }))
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn show_tables_filters_rows_by_granted_table_permission() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Err(AuthorizationError::Denied), Ok(())],
    ));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([49; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    adapter
        .session
        .connection()
        .unwrap()
        .execute("CREATE TABLE alpha (id INT)")
        .unwrap();

    let CommandExecutionResult::ResultSet(result) = adapter.execute_query("SHOW TABLES").unwrap()
    else {
        panic!("SHOW TABLES must return a result set");
    };
    assert_eq!(result.rows, vec![vec![Some(b"records".to_vec())]]);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "alpha".to_owned(),
            },
            RecordedDatabaseAction::TableSelect {
                database: "reports".to_owned(),
                table: "records".to_owned(),
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn show_databases_has_bounded_protocol_result() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([18; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("CREATE DATABASE Archive").unwrap();
    let codec = PacketCodec::new(4096).unwrap();
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(b"SHOW DATABASES");
    let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
    let mut connection = ready_connection();

    let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
    assert_eq!(
        frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        crate::ColumnCountPacket::decode(codec, &frames[0])
            .unwrap()
            .column_count,
        1
    );
    let column = crate::ColumnDefinitionPacket::decode(codec, &frames[1]).unwrap();
    assert_eq!(column.name, "Database");
    assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
    assert_eq!(column.column_length, 64);
    let first_row = crate::TextRowPacket::decode(codec, &frames[3], 1).unwrap();
    assert!(matches!(first_row.values[0], TextRowValue::Bytes(value) if value == b"archive"));
    let second_row = crate::TextRowPacket::decode(codec, &frames[4], 1).unwrap();
    assert!(matches!(second_row.values[0], TextRowValue::Bytes(value) if value == b"reports"));
    assert!(matches!(
        crate::ResultTerminatorPacket::decode(
            codec,
            &frames[2],
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap(),
        crate::ResultTerminatorPacket::Eof(_)
    ));
    assert!(matches!(
        crate::ResultTerminatorPacket::decode(
            codec,
            &frames[5],
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap(),
        crate::ResultTerminatorPacket::Eof(_)
    ));
}

#[cfg(unix)]
#[test]
fn show_tables_has_bounded_protocol_result() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([33; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();
    let codec = PacketCodec::new(4096).unwrap();
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(b"SHOW TABLES");
    let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
    let mut connection = ready_connection();

    let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
    assert_eq!(
        frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    assert_eq!(
        crate::ColumnCountPacket::decode(codec, &frames[0])
            .unwrap()
            .column_count,
        1
    );
    let column = crate::ColumnDefinitionPacket::decode(codec, &frames[1]).unwrap();
    assert_eq!(column.name, "Tables_in_reports");
    assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
    assert_eq!(column.column_length, 256);
    let row = crate::TextRowPacket::decode(codec, &frames[3], 1).unwrap();
    assert!(matches!(row.values[0], TextRowValue::Bytes(value) if value == b"records"));
    assert!(matches!(
        crate::ResultTerminatorPacket::decode(
            codec,
            &frames[2],
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap(),
        crate::ResultTerminatorPacket::Eof(_)
    ));
    assert!(matches!(
        crate::ResultTerminatorPacket::decode(
            codec,
            &frames[4],
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap(),
        crate::ResultTerminatorPacket::Eof(_)
    ));
}

#[cfg(unix)]
#[test]
fn show_databases_returns_an_empty_result_without_a_selection() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.drop_database("reports").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([19; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("SHOW DATABASES"),
        Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: vec![database_list_column()],
            rows: Vec::new(),
            warnings: 0,
            status_flags: 0x0002,
        }))
    );
}

#[cfg(unix)]
#[test]
fn information_schema_schemata_lists_databases_without_a_selection() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("Archive").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([60; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA")
        .unwrap()
    else {
        panic!("information_schema.SCHEMATA must return a result set");
    };
    assert_eq!(result.columns, vec![information_schema_schemata_column()]);
    assert_eq!(
        result.rows,
        vec![
            vec![Some(b"archive".to_vec())],
            vec![Some(b"reports".to_vec())],
        ]
    );
    assert_eq!(result.warnings, 0);
    assert_eq!(result.status_flags, 0x0002);
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::List
        ]
    );
}

#[cfg(unix)]
#[test]
fn information_schema_schemata_reuses_list_authorization_and_bounds() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Err(AuthorizationError::Denied),
    ]));
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([61; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    assert_eq!(
        adapter.execute_query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA"),
        Err(FrontendErrorKind::AccessDenied)
    );
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::List
        ]
    );
    assert_eq!(
        information_schema_schemata_result_to_execution_result(vec![
            String::new();
            MAX_DISPATCH_RESULT_ROWS + 1
        ]),
        Err(FrontendErrorKind::Internal)
    );
}

#[cfg(unix)]
#[test]
fn information_schema_schemata_rejects_malformed_queries_without_fallthrough() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([62; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    for query in [
        "SELECT * FROM information_schema.SCHEMATA",
        "SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME FROM information_schema.SCHEMATA",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'reports'",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA LIMIT 1",
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA; SELECT 1",
    ] {
        assert_eq!(
            adapter.execute_query(query),
            Err(FrontendErrorKind::Syntax),
            "malformed information_schema.SCHEMATA query must fail closed: {query}"
        );
    }
    assert_eq!(
        authorizer.actions(),
        vec![RecordedDatabaseAction::Connect(None)]
    );
}

#[cfg(unix)]
#[test]
fn show_databases_rejects_more_rows_than_the_dispatcher_can_encode() {
    assert_eq!(
        admin_result_to_execution_result(MySqlAdminCommandResult::Listed {
            databases: vec![String::new(); MAX_DISPATCH_RESULT_ROWS + 1],
        }),
        Err(FrontendErrorKind::Internal)
    );
}

#[cfg(unix)]
#[test]
fn show_tables_rejects_unencodable_results_before_dispatch() {
    assert_eq!(
        show_tables_result_to_execution_result(
            "reports",
            vec![String::new(); MAX_DISPATCH_RESULT_ROWS + 1],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
    assert_eq!(
        show_tables_result_to_execution_result(
            "reports",
            vec!["x".repeat(MAX_TEXT_ROW_VALUE_LENGTH + 1)],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
    assert_eq!(
        show_tables_result_to_execution_result(
            "reports",
            vec![
                "x".repeat(MAX_TEXT_ROW_VALUE_LENGTH);
                (MAX_FRONTEND_ADAPTER_RESULT_BYTES / MAX_TEXT_ROW_VALUE_LENGTH) + 1
            ],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
}

#[cfg(unix)]
#[test]
fn denied_and_unavailable_admin_actions_are_fixed_access_denied_packets() {
    let authorizer = Arc::new(RecordingAuthorizer::with_decisions([
        Ok(()),
        Err(AuthorizationError::Denied),
        Err(AuthorizationError::Unavailable),
    ]));
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([20; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    let codec = PacketCodec::new(4096).unwrap();
    let mut connection = ready_connection();

    let mut error_frames = Vec::new();
    for sql in ["CREATE DATABASE REPORTS", "DROP DATABASE MISSING"] {
        let mut payload = vec![COM_QUERY];
        payload.extend_from_slice(sql.as_bytes());
        let command = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
        let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
        assert_eq!(frames.len(), 1);
        let error = crate::ErrPacket::decode(
            codec,
            &frames[0],
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap();
        assert_eq!(error.sequence_id, 1);
        assert_eq!(error.error_code, 1045);
        assert_eq!(error.sql_state, Some(*b"28000"));
        assert_eq!(error.message, b"access denied");
        assert_eq!(connection.state(), ConnectionState::Ready);
        error_frames.push(frames[0].clone());
    }
    assert_eq!(error_frames[0], error_frames[1]);
    assert_eq!(catalog.list().unwrap(), vec![String::from("reports")]);
}

#[cfg(unix)]
#[test]
fn factory_passes_the_authenticated_canonical_account_id_to_the_policy() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let expected = AccountId::from_bytes([0xa5; 32]);
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            expected.clone(),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    assert_eq!(
        authorizer.account_ids.lock().unwrap().as_slice(),
        &[expected]
    );
}

#[test]
fn malformed_select_is_a_syntax_category() {
    let mut adapter = adapter();
    assert_eq!(
        adapter.execute_query("SELECT FROM"),
        Err(FrontendErrorKind::Syntax)
    );
}

#[test]
fn core_prepare_errors_are_not_guessed_to_be_syntax_errors() {
    let mut adapter = adapter();
    assert_eq!(
        adapter.execute_query("SELECT id FROM missing_table"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[test]
fn result_collection_stops_at_the_dispatcher_row_limit() {
    let mut adapter = adapter();
    assert_eq!(
        adapter.execute_query("SELECT id FROM many_rows"),
        Err(FrontendErrorKind::Unsupported)
    );
}

#[test]
fn aggregate_row_payload_is_rejected_before_values_are_copied() {
    let mut adapter = adapter();
    assert_eq!(
        adapter.execute_query("SELECT left_value, right_value FROM wide_values"),
        Err(FrontendErrorKind::Unsupported)
    );
}

fn ready_connection() -> ClassicConnection {
    let codec = PacketCodec::new(4096).unwrap();
    let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
    let mut connection = ClassicConnection::with_test_nonce(
        InitialHandshakeSettings {
            capability_flags: capabilities,
            ..InitialHandshakeSettings::default()
        },
        codec,
        TransportSecurity::Secure,
        [0xa5; crate::AUTH_PLUGIN_DATA_LENGTH],
    )
    .unwrap();
    connection.send_initial_handshake().unwrap();
    let response = ClientHandshakeResponseConfig::new(
        capabilities,
        0,
        DEFAULT_UTF8MB4_COLLATION,
        "root",
        [0; 32],
        None::<String>,
        Some(CACHING_SHA2_PASSWORD_PLUGIN),
        None,
    )
    .encode(codec, 1)
    .unwrap();
    connection
        .receive_client_handshake_frame(&response)
        .unwrap();
    connection
        .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
        .unwrap();
    connection.send_authentication_ok().unwrap();
    assert_eq!(connection.state(), ConnectionState::Ready);
    connection
}

#[test]
fn adapter_runs_through_dispatcher_with_protocol_sequences() {
    let mut connection = ready_connection();
    let mut adapter = adapter();
    let codec = PacketCodec::new(4096).unwrap();
    let mut command_payload = vec![COM_QUERY];
    command_payload.extend_from_slice(b"SELECT id, payload FROM result_values");
    let command = codec.encode(COMMAND_SEQUENCE_ID, &command_payload).unwrap();

    let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
    assert_eq!(
        frames.iter().map(|frame| frame[3]).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 7]
    );
    assert_eq!(
        crate::ColumnCountPacket::decode(codec, &frames[0])
            .unwrap()
            .column_count,
        2
    );
    let id_definition = crate::ColumnDefinitionPacket::decode(codec, &frames[1]).unwrap();
    assert_eq!(id_definition.column_type, MYSQL_TYPE_LONG);
    assert_eq!(id_definition.character_set, MYSQL_BINARY_COLLATION);
    let payload_definition = crate::ColumnDefinitionPacket::decode(codec, &frames[2]).unwrap();
    assert_eq!(payload_definition.column_type, MYSQL_TYPE_BLOB);
    assert_eq!(payload_definition.character_set, MYSQL_BINARY_COLLATION);
    let first_row = crate::TextRowPacket::decode(codec, &frames[4], 2).unwrap();
    assert!(matches!(first_row.values[0], TextRowValue::Bytes(value) if value == b"1"));
    assert!(matches!(first_row.values[1], TextRowValue::Bytes(value) if value == [0, 0xff]));
    let second_row = crate::TextRowPacket::decode(codec, &frames[5], 2).unwrap();
    assert!(matches!(second_row.values[0], TextRowValue::Bytes(value) if value == b"2"));
    assert!(matches!(second_row.values[1], TextRowValue::Null));
}

#[cfg(unix)]
#[test]
fn catalog_adapter_runs_init_db_through_the_dispatcher() {
    let mut connection = ready_connection();
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([11; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    let codec = PacketCodec::new(4096).unwrap();
    let mut command_payload = vec![COM_INIT_DB];
    command_payload.extend_from_slice(b"REPORTS");
    let command = codec.encode(COMMAND_SEQUENCE_ID, &command_payload).unwrap();

    let frames = dispatch_command_frame(&mut connection, &mut adapter, &command).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        crate::OkPacket::decode(codec, &frames[0])
            .unwrap()
            .sequence_id,
        1
    );
    assert!(matches!(
        adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert_eq!(connection.state(), ConnectionState::Ready);
}

#[cfg(unix)]
#[test]
fn catalog_adapter_selects_the_handshake_database_before_authentication_ok() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer.clone());
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([12; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    let codec = PacketCodec::new(4096).unwrap();
    let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_CONNECT_WITH_DB;
    let mut connection = ClassicConnection::with_test_nonce(
        InitialHandshakeSettings {
            capability_flags: capabilities,
            ..InitialHandshakeSettings::default()
        },
        codec,
        TransportSecurity::Secure,
        [0xa5; crate::AUTH_PLUGIN_DATA_LENGTH],
    )
    .unwrap();
    connection.send_initial_handshake().unwrap();
    let response = ClientHandshakeResponseConfig::new(
        capabilities,
        0,
        DEFAULT_UTF8MB4_COLLATION,
        "root",
        [0; 32],
        Some("REPORTS"),
        Some(CACHING_SHA2_PASSWORD_PLUGIN),
        None,
    )
    .encode(codec, 1)
    .unwrap();
    connection
        .receive_client_handshake_frame(&response)
        .unwrap();
    connection
        .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
        .unwrap();

    let AuthenticationResponse::Ok(frame) = connection
        .send_authentication_ok_with_selector(&mut adapter)
        .unwrap()
    else {
        panic!("known initial database must produce authentication OK");
    };
    assert_eq!(
        crate::AuthOkPacket::decode(codec, &frame)
            .unwrap()
            .sequence_id,
        3
    );
    assert!(matches!(
        adapter.execute_query("SELECT id FROM records"),
        Ok(CommandExecutionResult::ResultSet(_))
    ));
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
        ]
    );
    assert_eq!(connection.state(), ConnectionState::Ready);
}
