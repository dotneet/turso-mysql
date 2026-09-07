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
        "INSERT INTO v (id, name, seen) VALUES (12, 't', '2026-09-06 24:00:00')",
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

    // Measured on MySQL 8.4.11: an unnamed inline key is named after its first
    // column, and where that is taken it gains `_2`, `_3` and so on, counting
    // the names the statement wrote before it and the ones it named before it.
    adapter
        .execute_query("CREATE TABLE inline (a INT, b INT, KEY (a), KEY (a), KEY (a, b), KEY (b))")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE inline").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `inline` (\n",
            "  `a` int DEFAULT NULL,\n",
            "  `b` int DEFAULT NULL,\n",
            "  KEY `a` (`a`),\n",
            "  KEY `a_2` (`a`),\n",
            "  KEY `a_3` (`a`,`b`),\n",
            "  KEY `b` (`b`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // Measured: a name the statement wrote is counted too, wherever it stands.
    adapter
        .execute_query("CREATE TABLE mixed (c INT, d INT, KEY c_2 (d), KEY (c), KEY (c))")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE mixed").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `mixed` (\n",
            "  `c` int DEFAULT NULL,\n",
            "  `d` int DEFAULT NULL,\n",
            "  KEY `c_2` (`d`),\n",
            "  KEY `c` (`c`),\n",
            "  KEY `c_3` (`c`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // A divergence recorded in COMPAT.md, and the reason the two tables above
    // were given different column names: an index name is per table in MySQL
    // and database-wide in the engine, so two tables cannot carry an index of
    // the same name here. Measured on MySQL 8.4.11, both of these are taken.
    adapter
        .execute_query("CREATE TABLE one (id INT, KEY (id))")
        .unwrap();
    assert!(adapter
        .execute_query("CREATE TABLE two (id INT, KEY (id))")
        .is_err());
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

    // Refused: DISTINCT on other aggregates has its own meaning, and an expression argument
    // has no type this can work out.
    for sql in [
        "SELECT COUNT(DISTINCT *) FROM c",
        "SELECT SUM(DISTINCT n) FROM c",
        "SELECT SUM(n + 1) FROM c",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

#[test]
fn count_distinct_collates_text_and_skips_nulls_matching_mysql_8_4() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([16; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, team VARCHAR(20), n INT)")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO t (id, team, n) VALUES (1,'a',1),(2,'a',2),(3,'b',2),(4,'B',NULL)",
        )
        .unwrap();

    // Measured on MySQL 8.4.11: COUNT(DISTINCT ...) answers LONGLONG 21 NOT NULL BINARY NUM.
    // Text columns collate case-insensitively ('b' and 'B' match -> count 2),
    // and NULL values in numeric/text columns are not counted (count 2).
    for (sql, name, expected_count) in [
        (
            "SELECT COUNT(DISTINCT team) FROM t",
            "COUNT(DISTINCT team)",
            "2",
        ),
        ("SELECT COUNT(DISTINCT n) FROM t", "COUNT(DISTINCT n)", "2"),
        (
            "SELECT count(distinct team) FROM t",
            "count(distinct team)",
            "2",
        ),
        ("SELECT COUNT(DISTINCT team) AS teams FROM t", "teams", "2"),
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
            expected_count,
            "{sql}"
        );
    }

    // Combined with GROUP BY and HAVING
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query(
            "SELECT team, COUNT(DISTINCT n) FROM t GROUP BY team HAVING COUNT(DISTINCT n) > 1",
        )
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap(),
        "a"
    );
}

#[test]
fn group_concat_answers_blob_length_65536_decimals_31_and_skips_nulls_matching_mysql_8_4() {
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
            "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, team VARCHAR(20), name VARCHAR(30), n INT)",
        )
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO t (id, team, name, n) VALUES (1,'a','x',10),(2,'a','y',20),(3,'b','z',30),(4,'a',NULL,NULL)",
        )
        .unwrap();

    // Measured over a utf8mb4 connection, which is the only one this server
    // serves: GROUP_CONCAT answers MYSQL_TYPE_LONG_BLOB (251), length 65536,
    // decimals 31, and flags 0 (nullable, an empty group answering NULL).
    for (sql, name, expected) in [
        (
            "SELECT GROUP_CONCAT(name) FROM t",
            "GROUP_CONCAT(name)",
            "x,y,z",
        ),
        (
            "SELECT group_concat(name) FROM t",
            "group_concat(name)",
            "x,y,z",
        ),
        (
            "SELECT GROUP_CONCAT(n) FROM t",
            "GROUP_CONCAT(n)",
            "10,20,30",
        ),
        (
            "SELECT GROUP_CONCAT(name) AS names FROM t",
            "names",
            "x,y,z",
        ),
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
            (MYSQL_TYPE_LONG_BLOB, 65536, 0, 31),
            "{sql}"
        );
        assert_eq!(
            String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap(),
            expected,
            "{sql}"
        );
    }

    // Combined with GROUP BY
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT team, GROUP_CONCAT(name) FROM t GROUP BY team ORDER BY team")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        String::from_utf8(result.rows[0][1].clone().unwrap()).unwrap(),
        "x,y"
    );
    assert_eq!(
        String::from_utf8(result.rows[1][1].clone().unwrap()).unwrap(),
        "z"
    );

    // Empty result returns NULL
    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT GROUP_CONCAT(name) FROM t WHERE id = 999")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(result.rows[0][0], None);

    // A separator and a DISTINCT are each taken, and both answer what MySQL
    // 8.4.11 answers over the same rows.
    for (sql, expected) in [
        ("SELECT GROUP_CONCAT(name SEPARATOR '-') FROM t", "x-y-z"),
        ("SELECT GROUP_CONCAT(DISTINCT team) FROM t", "a,b"),
        ("SELECT GROUP_CONCAT(team) FROM t", "a,a,b,a"),
    ] {
        let CommandExecutionResult::ResultSet(joined) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            String::from_utf8(joined.rows[0][0].clone().unwrap()).unwrap(),
            expected,
            "{sql}"
        );
    }

    // Refused: an ORDER BY, which MySQL applies to the parts it joins and
    // the engine has no way to say; DISTINCT beside a separator, the engine
    // taking DISTINCT only over one argument; and several columns.
    for sql in [
        "SELECT GROUP_CONCAT(name ORDER BY name DESC) FROM t",
        "SELECT GROUP_CONCAT(DISTINCT name SEPARATOR '-') FROM t",
        "SELECT GROUP_CONCAT(team, name) FROM t",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

#[test]
fn null_safe_equal_comparison_matches_mysql_8_4() {
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
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT, s VARCHAR(20))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, n, s) VALUES (1, 1, 'a'), (2, NULL, NULL)")
        .unwrap();

    // Standard = with NULL yields 0 rows; <=> with NULL matches NULL row
    let CommandExecutionResult::ResultSet(res_eq_null) = adapter
        .execute_query("SELECT id FROM t WHERE n = NULL")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(res_eq_null.rows.len(), 0);

    let CommandExecutionResult::ResultSet(res_n_null) = adapter
        .execute_query("SELECT id FROM t WHERE n <=> NULL")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(res_n_null.rows, vec![vec![Some(b"2".to_vec())]]);

    let CommandExecutionResult::ResultSet(res_n_1) = adapter
        .execute_query("SELECT id FROM t WHERE n <=> 1")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(res_n_1.rows, vec![vec![Some(b"1".to_vec())]]);

    // Collation test: case-insensitive for text column
    let CommandExecutionResult::ResultSet(res_s_a) = adapter
        .execute_query("SELECT id FROM t WHERE s <=> 'A'")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(res_s_a.rows, vec![vec![Some(b"1".to_vec())]]);

    // NOT (n <=> 1): row 1 (1 <=> 1 is TRUE, NOT is FALSE), row 2 (NULL <=> 1 is FALSE, NOT is TRUE)
    let CommandExecutionResult::ResultSet(res_not) = adapter
        .execute_query("SELECT id FROM t WHERE NOT (n <=> 1)")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(res_not.rows, vec![vec![Some(b"2".to_vec())]]);

    // Prepared statements with parameter
    let prepared_n = adapter
        .execute_stmt_prepare("SELECT id FROM t WHERE n <=> ?")
        .unwrap();

    // Bind 1 to n <=> ? -> matches row 1
    let mut integer = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
    integer.extend_from_slice(&1i64.to_le_bytes());
    let res_prep_1 = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared_n.statement_id, &integer)
            .unwrap(),
    );
    assert_eq!(res_prep_1.rows, [vec![BinaryResultValue::Integer(1)]]);

    // Bind NULL to n <=> ? -> matches row 2
    let res_prep_null = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared_n.statement_id, &[1, 1, MYSQL_TYPE_NULL, 0])
            .unwrap(),
    );
    assert_eq!(res_prep_null.rows, [vec![BinaryResultValue::Integer(2)]]);
}

#[test]
fn qualified_column_comparison_matches_mysql_8_4() {
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
        .execute_query("CREATE TABLE f (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(20))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO f (id, n, name) VALUES (1, 7, 'Alice'), (2, 8, 'Bob')")
        .unwrap();

    // 1. Aliased single table filters rows correctly
    let CommandExecutionResult::ResultSet(aliased) = adapter
        .execute_query("SELECT u.id, u.name FROM f u WHERE u.id = 1")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        aliased.rows,
        vec![vec![Some(b"1".to_vec()), Some(b"Alice".to_vec())]]
    );

    // 2. CTE with WHERE c.id = 1 where CTE reorders projected columns (n, id instead of id, n)
    let CommandExecutionResult::ResultSet(cte) = adapter
        .execute_query("WITH c AS (SELECT n, id FROM f) SELECT c.n, c.id FROM c WHERE c.id = 1")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        cte.rows,
        vec![vec![Some(b"7".to_vec()), Some(b"1".to_vec())]]
    );
    // Result column metadata check
    assert_eq!(cte.columns[0].name, "n");
    assert_eq!(cte.columns[1].name, "id");
    assert_eq!(cte.columns[1].column_type, MYSQL_TYPE_LONG);

    // 3. Text column collation with qualified column: case-insensitive match
    let CommandExecutionResult::ResultSet(text_col) = adapter
        .execute_query("SELECT u.id FROM f u WHERE u.name = 'alice'")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(text_col.rows, vec![vec![Some(b"1".to_vec())]]);

    // 4. Type mismatch: integer compared against text column without coercion remains rejected
    assert!(adapter
        .execute_query("SELECT u.id FROM f u WHERE u.name = 1")
        .is_err());

    // 5. Unmatching qualifier is rejected
    assert!(adapter
        .execute_query("SELECT u.id FROM f u WHERE f.id = 1")
        .is_err());
    assert!(adapter
        .execute_query("SELECT id FROM f WHERE other.id = 1")
        .is_err());
}

#[test]
fn dml_in_list_matches_mysql_8_4() {
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
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, name) VALUES (1, 'b'), (2, 'A'), (3, 'c')")
        .unwrap();

    // UPDATE with text IN list: case-insensitive ('a' and 'C' match 'A' and 'c' -> 2 rows affected)
    let CommandExecutionResult::Ok(update_res) = adapter
        .execute_query("UPDATE t SET name = 'z' WHERE name IN ('a', 'C')")
        .unwrap()
    else {
        panic!("must return Ok packet");
    };
    assert_eq!(update_res.affected_rows, 2);

    // Verify rows after UPDATE
    let CommandExecutionResult::ResultSet(rows_after_update) = adapter
        .execute_query("SELECT id, name FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        rows_after_update.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"b".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"z".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"z".to_vec())],
        ]
    );

    // DELETE with NULL in IN list: matches 1 (1 row affected)
    let CommandExecutionResult::Ok(delete_res) = adapter
        .execute_query("DELETE FROM t WHERE id IN (1, NULL)")
        .unwrap()
    else {
        panic!("must return Ok packet");
    };
    assert_eq!(delete_res.affected_rows, 1);

    // DELETE with NOT IN containing NULL: 0 rows affected (three-valued logic)
    let CommandExecutionResult::Ok(delete_not_in) = adapter
        .execute_query("DELETE FROM t WHERE id NOT IN (2, NULL)")
        .unwrap()
    else {
        panic!("must return Ok packet");
    };
    assert_eq!(delete_not_in.affected_rows, 0);

    // Remaining rows: id 2 and 3
    let CommandExecutionResult::ResultSet(remaining) = adapter
        .execute_query("SELECT id FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        remaining.rows,
        vec![vec![Some(b"2".to_vec())], vec![Some(b"3".to_vec())]]
    );
}

#[test]
fn select_order_by_ordinal_over_wildcard_projection_collates_text() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([35; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), n INT)")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO t (id, name, n) VALUES (1, 'b', 10), (2, 'A', 30), (3, 'c', 20)",
        )
        .unwrap();

    // SELECT * FROM t ORDER BY 2: sorts by name case-insensitively ('A', 'b', 'c' -> id 2, 1, 3)
    let CommandExecutionResult::ResultSet(ordered) =
        adapter.execute_query("SELECT * FROM t ORDER BY 2").unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        ordered.rows,
        vec![
            vec![
                Some(b"2".to_vec()),
                Some(b"A".to_vec()),
                Some(b"30".to_vec())
            ],
            vec![
                Some(b"1".to_vec()),
                Some(b"b".to_vec()),
                Some(b"10".to_vec())
            ],
            vec![
                Some(b"3".to_vec()),
                Some(b"c".to_vec()),
                Some(b"20".to_vec())
            ],
        ]
    );

    // DESC ordering: 'c', 'b', 'A' -> id 3, 1, 2
    let CommandExecutionResult::ResultSet(ordered_desc) = adapter
        .execute_query("SELECT * FROM t ORDER BY 2 DESC")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        ordered_desc.rows,
        vec![
            vec![
                Some(b"3".to_vec()),
                Some(b"c".to_vec()),
                Some(b"20".to_vec())
            ],
            vec![
                Some(b"1".to_vec()),
                Some(b"b".to_vec()),
                Some(b"10".to_vec())
            ],
            vec![
                Some(b"2".to_vec()),
                Some(b"A".to_vec()),
                Some(b"30".to_vec())
            ],
        ]
    );

    // Numeric column ordinal ordering: ORDER BY 3 (10, 20, 30 -> id 1, 3, 2)
    let CommandExecutionResult::ResultSet(ordered_n) =
        adapter.execute_query("SELECT * FROM t ORDER BY 3").unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        ordered_n.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"b".to_vec()),
                Some(b"10".to_vec())
            ],
            vec![
                Some(b"3".to_vec()),
                Some(b"c".to_vec()),
                Some(b"20".to_vec())
            ],
            vec![
                Some(b"2".to_vec()),
                Some(b"A".to_vec()),
                Some(b"30".to_vec())
            ],
        ]
    );

    // Multiple ordinals: ORDER BY 1, 2
    let CommandExecutionResult::ResultSet(ordered_multi) = adapter
        .execute_query("SELECT * FROM t ORDER BY 1, 2")
        .unwrap()
    else {
        panic!("must return result set");
    };
    assert_eq!(
        ordered_multi.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"b".to_vec()),
                Some(b"10".to_vec())
            ],
            vec![
                Some(b"2".to_vec()),
                Some(b"A".to_vec()),
                Some(b"30".to_vec())
            ],
            vec![
                Some(b"3".to_vec()),
                Some(b"c".to_vec()),
                Some(b"20".to_vec())
            ],
        ]
    );

    // Ordinal outside projection is refused
    assert!(adapter.execute_query("SELECT * FROM t ORDER BY 4").is_err());
    assert!(adapter.execute_query("SELECT * FROM t ORDER BY 0").is_err());

    // Mixed wildcard and explicit projection is refused
    assert!(adapter
        .execute_query("SELECT t.*, id FROM t ORDER BY 2")
        .is_err());
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

/// A DATE holds the day alone. Measured on MySQL 8.4.11: the column reports
/// type 10 with the width of `YYYY-MM-DD` and the binary flag, `SHOW CREATE
/// TABLE` prints `date`, an impossible day answers 1292, and `CURDATE()` and
/// `CURRENT_DATE` each answer a NOT NULL DATE of the same width.
#[cfg(unix)]
#[test]
fn a_date_column_holds_the_day_and_curdate_answers_one() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([93; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE d (id INT NOT NULL PRIMARY KEY, a DATE, b DATE NOT NULL)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO d (id, a, b) VALUES (1, '2026-09-07', '2026-01-02')")
        .unwrap();

    // The calendar is checked the way a DATETIME's is.
    assert_eq!(
        adapter.execute_query("INSERT INTO d (id, a, b) VALUES (2, '2026-02-30', '2026-01-02')"),
        Err(FrontendErrorKind::IncorrectTemporalValue)
    );
    // A loose spelling is read the way MySQL reads it and stored the way
    // MySQL stores it, so the text read back is MySQL's own.
    for (id, written) in [(3, "2026-9-7"), (4, "20260907"), (5, "2026/9/7")] {
        adapter
            .execute_query(&format!(
                "INSERT INTO d (id, a, b) VALUES ({id}, '{written}', '2026-01-02')"
            ))
            .unwrap_or_else(|_| panic!("{written} must be stored"));
        let CommandExecutionResult::ResultSet(read_back) = adapter
            .execute_query(&format!("SELECT a FROM d WHERE id = {id}"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read_back.rows[0][0].clone().unwrap()).unwrap(),
            "2026-09-07",
            "{written}"
        );
    }

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
            "  `a` date DEFAULT NULL,\n",
            "  `b` date NOT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(selected) =
        adapter.execute_query("SELECT a, b FROM d").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        selected
            .columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.decimals,
                column.character_set,
                column.flags & (MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                MYSQL_TYPE_DATE,
                10,
                0,
                MYSQL_BINARY_COLLATION,
                MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_DATE,
                10,
                0,
                MYSQL_BINARY_COLLATION,
                MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG
            ),
        ]
    );
    assert_eq!(
        String::from_utf8(selected.rows[0][0].clone().unwrap()).unwrap(),
        "2026-09-07"
    );

    // A comparison against the day the column holds finds every row, however
    // each was written: they are all stored as that one day. Measured on
    // MySQL 8.4.11, which answers the same four.
    let CommandExecutionResult::ResultSet(matched) = adapter
        .execute_query("SELECT a FROM d WHERE a = '2026-09-07'")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(matched.rows.len(), 4);
    // A day written any other way is refused: MySQL reads `2026-9-7` as the
    // seventh of September and comparing the stored form against that text
    // would find nothing.
    assert_eq!(
        adapter.execute_query("SELECT a FROM d WHERE a = '2026-9-7'"),
        Err(FrontendErrorKind::Unsupported)
    );

    let CommandExecutionResult::ResultSet(today) = adapter
        .execute_query("SELECT CURDATE(), CURRENT_DATE FROM d")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        today
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
            (
                "CURDATE()",
                MYSQL_TYPE_DATE,
                10,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
            (
                "CURRENT_DATE",
                MYSQL_TYPE_DATE,
                10,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
        ]
    );
    let answered = String::from_utf8(today.rows[0][0].clone().unwrap()).unwrap();
    assert_eq!(answered.len(), 10, "CURDATE answered {answered}");
    assert_eq!(&answered[4..5], "-");
}

/// A TIME holds a span rather than a moment: measured on MySQL 8.4.11 it runs
/// from `-838:59:59` to `838:59:59`, the column reports type 11 with length 10
/// and the binary flag, and `CURTIME()` answers the same type at length 8.
#[cfg(unix)]
#[test]
fn a_time_column_holds_a_span_and_curtime_answers_a_clock_reading() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([94; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE s (id INT NOT NULL PRIMARY KEY, a TIME, b TIME NOT NULL)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO s (id, a, b) VALUES (1, '-838:59:59', '12:34:56')")
        .unwrap();

    for value in ["99:99:99", "838:60:00", "839:00:00", "1 9"] {
        assert_eq!(
            adapter.execute_query(&format!(
                "INSERT INTO s (id, a, b) VALUES (2, '{value}', '00:00:00')"
            )),
            Err(FrontendErrorKind::IncorrectTemporalValue),
            "{value}"
        );
    }

    // A loose spelling is read the way MySQL reads it and stored the way
    // MySQL stores it.
    for (id, written, stored) in [
        (3, "12:34", "12:34:00"),
        (4, "123456", "12:34:56"),
        (5, "5", "00:00:05"),
        (6, "2 1:1:1", "49:01:01"),
    ] {
        adapter
            .execute_query(&format!(
                "INSERT INTO s (id, a, b) VALUES ({id}, '{written}', '00:00:00')"
            ))
            .unwrap_or_else(|_| panic!("{written} must be stored"));
        let CommandExecutionResult::ResultSet(read_back) = adapter
            .execute_query(&format!("SELECT a FROM s WHERE id = {id}"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read_back.rows[0][0].clone().unwrap()).unwrap(),
            stored,
            "{written}"
        );
    }

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE s").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `s` (\n",
            "  `id` int NOT NULL,\n",
            "  `a` time DEFAULT NULL,\n",
            "  `b` time NOT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(selected) =
        adapter.execute_query("SELECT a, b FROM s").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        selected
            .columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.decimals,
                column.character_set,
                column.flags & (MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                MYSQL_TYPE_TIME,
                10,
                0,
                MYSQL_BINARY_COLLATION,
                MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_TIME,
                10,
                0,
                MYSQL_BINARY_COLLATION,
                MYSQL_BINARY_FLAG | MYSQL_NOT_NULL_FLAG
            ),
        ]
    );
    assert_eq!(
        String::from_utf8(selected.rows[0][0].clone().unwrap()).unwrap(),
        "-838:59:59"
    );

    let CommandExecutionResult::ResultSet(now) = adapter
        .execute_query("SELECT CURTIME(), CURRENT_TIME FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        now.columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "CURTIME()",
                MYSQL_TYPE_TIME,
                8,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
            (
                "CURRENT_TIME",
                MYSQL_TYPE_TIME,
                8,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
        ]
    );
    let answered = String::from_utf8(now.rows[0][0].clone().unwrap()).unwrap();
    assert_eq!(answered.len(), 8, "CURTIME answered {answered}");
    assert_eq!(&answered[2..3], ":");
}

/// A YEAR is the odd one among the temporal types: measured on MySQL 8.4.11 it
/// reports the flags of a number rather than of a moment — unsigned, zerofilled
/// and numeric, with no binary flag — at length 4, and it runs from 1901 to
/// 2155.
#[cfg(unix)]
#[test]
fn a_year_column_reports_the_flags_of_a_number() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([95; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE y (id INT NOT NULL PRIMARY KEY, a YEAR)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO y (id, a) VALUES (1, 2026)")
        .unwrap();
    for (id, year) in [(2, 1900), (3, 2156), (4, 100)] {
        assert_eq!(
            adapter.execute_query(&format!("INSERT INTO y (id, a) VALUES ({id}, {year})")),
            Err(FrontendErrorKind::OutOfRange),
            "{year}"
        );
    }
    // The two ends of the range are taken.
    adapter
        .execute_query("INSERT INTO y (id, a) VALUES (5, 1901), (6, 2155)")
        .unwrap();

    // A short year names one in the window MySQL keeps, and the zero is where
    // a year written as text parts from one written as a number: measured,
    // '0' is 2000 and 0 is the zero year.
    for (id, written, stored) in [
        (7, "70", "1970"),
        (8, "69", "2069"),
        (9, "'0'", "2000"),
        (10, "0", "0000"),
        (11, "'70'", "1970"),
    ] {
        adapter
            .execute_query(&format!("INSERT INTO y (id, a) VALUES ({id}, {written})"))
            .unwrap_or_else(|_| panic!("{written} must be stored"));
        let CommandExecutionResult::ResultSet(read_back) = adapter
            .execute_query(&format!("SELECT a FROM y WHERE id = {id}"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read_back.rows[0][0].clone().unwrap()).unwrap(),
            stored,
            "{written}"
        );
    }

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE y").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `y` (\n",
            "  `id` int NOT NULL,\n",
            "  `a` year DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT a FROM y WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let column = &selected.columns[0];
    assert_eq!(column.column_type, MYSQL_TYPE_YEAR);
    assert_eq!(column.column_length, 4);
    assert_eq!(column.decimals, 0);
    assert_eq!(column.character_set, MYSQL_BINARY_COLLATION);
    assert_eq!(
        column.flags,
        MYSQL_UNSIGNED_FLAG | MYSQL_ZEROFILL_FLAG | MYSQL_NUM_FLAG
    );
    assert_eq!(column.flags & MYSQL_BINARY_FLAG, 0);
    assert_eq!(
        String::from_utf8(selected.rows[0][0].clone().unwrap()).unwrap(),
        "2026"
    );
}

/// YEAR, MONTH, DAY and the clock readings each take a part out of a moment,
/// and DATEDIFF counts the days between two. Measured on MySQL 8.4.11:
/// `YEAR(a)` answers a YEAR of length 4 with the unsigned, binary and numeric
/// flags — and no zerofill, which the YEAR column carries — while `MONTH(a)`
/// and `DAY(a)` each answer a LONGLONG of length 3. All three are nullable
/// even over a NOT NULL column.
#[cfg(unix)]
#[test]
fn year_month_and_day_read_a_part_out_of_a_date() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([96; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE p (id INT NOT NULL PRIMARY KEY, a DATE NOT NULL, ",
            "b DATETIME, span TIME, name VARCHAR(4))"
        ))
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO p (id, a, b, span, name) VALUES ",
            "(1, '2026-09-07', '2026-01-02 03:04:05', '01:02:03', 'zz')"
        ))
        .unwrap();

    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query("SELECT YEAR(a), MONTH(a), DAY(b) FROM p")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.rows,
        vec![vec![
            Some(b"2026".to_vec()),
            Some(b"9".to_vec()),
            Some(b"2".to_vec()),
        ]]
    );
    assert_eq!(
        read.columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "YEAR(a)",
                MYSQL_TYPE_YEAR,
                4,
                MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "MONTH(a)",
                MYSQL_TYPE_LONGLONG,
                3,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "DAY(b)",
                MYSQL_TYPE_LONGLONG,
                3,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );

    // The clock readings answer the same shapes over a moment: MINUTE and
    // SECOND a LONGLONG of length 3 as MONTH does, and HOUR one of length 4,
    // its span running past a day. DATEDIFF answers one of length 9.
    let CommandExecutionResult::ResultSet(clock) = adapter
        .execute_query("SELECT HOUR(b), MINUTE(b), SECOND(b), DATEDIFF(b, b) FROM p")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        clock.rows,
        vec![vec![
            Some(b"3".to_vec()),
            Some(b"4".to_vec()),
            Some(b"5".to_vec()),
            Some(b"0".to_vec()),
        ]]
    );
    assert_eq!(
        clock
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_LONGLONG, 4, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONGLONG, 3, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONGLONG, 3, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            (MYSQL_TYPE_LONGLONG, 9, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
        ]
    );

    // Measured on MySQL 8.4.11: shifting a DATE by whole days keeps the day
    // alone, shifting it by a time widens it to a moment, and shifting a
    // DATETIME keeps its time whichever interval it is.
    let CommandExecutionResult::ResultSet(shifted) = adapter
        .execute_query(concat!(
            "SELECT DATE_ADD(a, INTERVAL 1 DAY), DATE_SUB(a, INTERVAL 2 MONTH), ",
            "DATE_ADD(a, INTERVAL 1 HOUR), DATE_ADD(b, INTERVAL 1 DAY), ",
            "DATE_ADD(b, INTERVAL 90 SECOND) FROM p"
        ))
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        shifted.rows,
        vec![vec![
            Some(b"2026-09-08".to_vec()),
            Some(b"2026-07-07".to_vec()),
            Some(b"2026-09-07 01:00:00".to_vec()),
            Some(b"2026-01-03 03:04:05".to_vec()),
            Some(b"2026-01-02 03:05:35".to_vec()),
        ]]
    );
    assert_eq!(
        shifted
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG),
        ]
    );

    // Measured: `YEAR` over a TIME answers the current year, which is a
    // coercion rather than a reading, so the three are held to a date. A TIME
    // is out of the clock readings for a reason of its own: it holds a span
    // running to 838 hours, which the engine has no reader for.
    for sql in [
        "SELECT YEAR(span) FROM p",
        "SELECT MONTH(name) FROM p",
        "SELECT DAY(id) FROM p",
        "SELECT HOUR(span) FROM p",
        "SELECT MINUTE(a) FROM p",
        "SELECT DATEDIFF(a, name) FROM p",
        "SELECT DATE_ADD(span, INTERVAL 1 DAY) FROM p",
        "SELECT DATE_ADD(name, INTERVAL 1 DAY) FROM p",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }

    // A quarter and a week are MySQL units the engine has no modifier for,
    // so neither is answered with a shift of a different size. The reader
    // refuses them before a column is looked at, so these read as syntax.
    for sql in [
        "SELECT DATE_ADD(a, INTERVAL 1 QUARTER) FROM p",
        "SELECT DATE_ADD(a, INTERVAL 1 WEEK) FROM p",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Syntax),
            "{sql}"
        );
    }
}

/// `FLUSH TABLES` closes MySQL's table cache. This server keeps none, so the
/// statement asks for something already true and is answered with an OK.
/// Everything else a `FLUSH` can ask for promises something this server cannot
/// keep, and each is refused.
#[cfg(unix)]
#[test]
fn flush_tables_is_answered_and_every_other_flush_is_refused() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([97; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();

    // It reads the selected database's own grant, as every catalog statement
    // does, so it needs one selected.
    assert_eq!(
        adapter.execute_query("FLUSH TABLES"),
        Err(FrontendErrorKind::NoDatabaseSelected)
    );
    adapter.execute_query("USE reports").unwrap();

    for sql in ["FLUSH TABLES", "flush local tables;"] {
        let CommandExecutionResult::Ok(flushed) = adapter.execute_query(sql).unwrap() else {
            panic!("FLUSH TABLES must produce an OK result");
        };
        assert_eq!(flushed.affected_rows, 0);
        assert_eq!(flushed.status_flags, SERVER_STATUS_AUTOCOMMIT);
    }

    // A read lock held across statements, reloaded grants and rotated logs are
    // each something this server has no way to deliver.
    for sql in [
        "FLUSH TABLES WITH READ LOCK",
        "FLUSH TABLES records",
        "FLUSH PRIVILEGES",
        "FLUSH LOGS",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }

    // The table it names is still there afterwards, the statement having
    // changed nothing.
    let CommandExecutionResult::ResultSet(tables) = adapter.execute_query("SHOW TABLES").unwrap()
    else {
        panic!("SHOW TABLES must return a result set");
    };
    assert_eq!(tables.rows, vec![vec![Some(b"records".to_vec())]]);
}

/// `STDDEV_SAMP` is the one standard deviation both engines compute the same
/// way. Measured on MySQL 8.4.11 over 2, 4, 4, 4, 5, 5, 7, 9: the sample form
/// answers 2.138089935299395 as a DOUBLE of length 23, while `STDDEV` there is
/// the population form and answers 2 — a different number, which is why only
/// the sample spelling is taken.
#[cfg(unix)]
#[test]
fn the_sample_standard_deviation_is_the_one_both_engines_agree_about() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([98; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE n (id INT NOT NULL PRIMARY KEY, v INT NOT NULL)")
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO n (id, v) VALUES ",
            "(1, 2), (2, 4), (3, 4), (4, 4), (5, 5), (6, 5), (7, 7), (8, 9)"
        ))
        .unwrap();

    let CommandExecutionResult::ResultSet(deviation) = adapter
        .execute_query("SELECT STDDEV_SAMP(v) FROM n")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        String::from_utf8(deviation.rows[0][0].clone().unwrap()).unwrap(),
        "2.138089935299395"
    );
    let column = &deviation.columns[0];
    assert_eq!(column.name, "STDDEV_SAMP(v)");
    assert_eq!(column.column_type, MYSQL_TYPE_DOUBLE);
    assert_eq!(column.column_length, 23);
    assert_eq!(column.decimals, NOT_FIXED_DECIMALS);
    assert_eq!(column.flags, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG);

    // Measured: one row has no sample deviation, and both engines answer NULL.
    let CommandExecutionResult::ResultSet(alone) = adapter
        .execute_query("SELECT STDDEV_SAMP(v) FROM n WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(alone.rows, vec![vec![None]]);

    // The population spellings answer a different number, so none is taken.
    for sql in [
        "SELECT STDDEV(v) FROM n",
        "SELECT STD(v) FROM n",
        "SELECT STDDEV_POP(v) FROM n",
        "SELECT VAR_SAMP(v) FROM n",
        "SELECT VARIANCE(v) FROM n",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// MySQL parses an inline `REFERENCES` and ignores it. Measured on 8.4.11: a
/// child row naming a parent that does not exist is stored, and `SHOW CREATE
/// TABLE` prints no constraint at all, whatever `ON DELETE` was written beside
/// it. So this reads the clause and writes nothing, which is the same answer.
#[cfg(unix)]
#[test]
fn an_inline_references_is_read_and_written_nowhere() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([99; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE p (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE c (id INT NOT NULL PRIMARY KEY, ",
            "parent_id INT REFERENCES p(id) ON DELETE CASCADE)"
        ))
        .unwrap();

    // Measured: the orphan is stored, the clause promising nothing.
    adapter
        .execute_query("INSERT INTO c (id, parent_id) VALUES (1, 999)")
        .unwrap();

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE c").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `c` (\n",
            "  `id` int NOT NULL,\n",
            "  `parent_id` int DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // The table-level spelling is a different statement, and it is enforced.
    adapter
        .execute_query(concat!(
            "CREATE TABLE d (id INT NOT NULL PRIMARY KEY, parent_id INT, ",
            "FOREIGN KEY (parent_id) REFERENCES p(id))"
        ))
        .unwrap();
    assert_eq!(
        adapter.execute_query("INSERT INTO d (id, parent_id) VALUES (1, 999)"),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );
}

/// An `ENUM` keeps its members on the one carrier every MySQL type rides here,
/// the engine's declared type name — which the engine takes whole when it is
/// quoted. Measured on MySQL 8.4.11: the column reports the fixed-width string
/// type with the ENUM flag and the width of its longest member, `SHOW CREATE
/// TABLE` prints `enum('small','medium','large')`, and a value that is not a
/// member answers 1265.
#[cfg(unix)]
#[test]
fn an_enum_column_holds_its_members_and_refuses_a_value_outside_them() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([100; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE e (id INT NOT NULL PRIMARY KEY, ",
            "s ENUM('small','medium','large'), t ENUM('a') NOT NULL)"
        ))
        .unwrap();
    adapter
        .execute_query("INSERT INTO e (id, s, t) VALUES (1, 'medium', 'a')")
        .unwrap();

    // Measured: a value outside the members answers 1265, and a member is
    // matched ignoring case and trailing spaces, stored in the spelling the
    // column declares. A value naming no member is read as a position, where
    // the zero is the empty error member MySQL keeps in front of them.
    for value in ["huge", " small", "small\t", "4", "-1"] {
        assert_eq!(
            adapter.execute_query(&format!(
                "INSERT INTO e (id, s, t) VALUES (2, '{value}', 'a')"
            )),
            Err(FrontendErrorKind::NotAMember),
            "{value}"
        );
    }
    for (id, written, stored) in [
        (3, "MEDIUM", "medium"),
        (4, "small  ", "small"),
        (5, "2", "medium"),
        (6, "0", ""),
    ] {
        adapter
            .execute_query(&format!(
                "INSERT INTO e (id, s, t) VALUES ({id}, '{written}', 'a')"
            ))
            .unwrap_or_else(|_| panic!("{written} must be stored"));
        let CommandExecutionResult::ResultSet(read_back) = adapter
            .execute_query(&format!("SELECT s FROM e WHERE id = {id}"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read_back.rows[0][0].clone().unwrap()).unwrap(),
            stored,
            "{written}"
        );
    }

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE e").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `e` (\n",
            "  `id` int NOT NULL,\n",
            "  `s` enum('small','medium','large') DEFAULT NULL,\n",
            "  `t` enum('a') NOT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM e").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        columns
            .rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        vec!["int", "enum('small','medium','large')", "enum('a')"]
    );

    // Measured: the fixed-width string type, the ENUM flag, and the width of
    // the longest member counting the four bytes utf8mb4 reserves.
    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT s, t FROM e WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        selected
            .columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.character_set,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                MYSQL_TYPE_STRING,
                24,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                MYSQL_ENUM_FLAG
            ),
            (
                MYSQL_TYPE_STRING,
                4,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                MYSQL_ENUM_FLAG | MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG
            ),
        ]
    );
    assert_eq!(
        String::from_utf8(selected.rows[0][0].clone().unwrap()).unwrap(),
        "medium"
    );
}

/// Measured on MySQL 8.4.11: an `ENUM` orders by the position its members were
/// declared in rather than by their text, so `small, medium, large` come back
/// in that order and not alphabetically. A NULL sorts in front of everything
/// and the empty error member in front of the declared members.
#[cfg(unix)]
#[test]
fn ordering_by_an_enum_follows_the_declared_positions() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([105; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE shirts (id INT NOT NULL PRIMARY KEY, ",
            "size ENUM('small','medium','large'))"
        ))
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO shirts (id, size) VALUES ",
            "(1, 'large'), (2, 'small'), (3, 'medium'), (4, NULL), (5, '0')"
        ))
        .unwrap();

    let ascending = [None, Some(""), Some("small"), Some("medium"), Some("large")];
    let descending = [Some("large"), Some("medium"), Some("small"), Some(""), None];
    for (sql, expected) in [
        ("SELECT size FROM shirts ORDER BY size", ascending),
        ("SELECT size FROM shirts ORDER BY 1", ascending),
        ("SELECT size FROM shirts ORDER BY size DESC", descending),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter.execute_query(sql).unwrap() else {
            panic!("SELECT must return a result set");
        };
        let read_back = read
            .rows
            .iter()
            .map(|row| {
                row[0]
                    .clone()
                    .map(|value| String::from_utf8(value).unwrap())
            })
            .collect::<Vec<_>>();
        let expected = expected
            .iter()
            .map(|value| value.map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(read_back, expected, "{sql}");
    }
}

/// A `SET` rides the carrier an `ENUM` does and differs in what it stores: any
/// subset of its members. Measured on MySQL 8.4.11: the column reports the
/// fixed-width string type with the SET flag and the width of every member laid
/// end to end with the commas that join them, and a value outside the members
/// answers 1265.
#[cfg(unix)]
#[test]
fn a_set_column_holds_a_subset_of_its_members() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([101; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE s (id INT NOT NULL PRIMARY KEY, ",
            "v SET('read','write','exec'))"
        ))
        .unwrap();
    // The empty string is the empty set, and MySQL stores it.
    adapter
        .execute_query("INSERT INTO s (id, v) VALUES (1, 'read,exec'), (2, ''), (3, 'write')")
        .unwrap();

    // MySQL rewrites what it stores into the members' declared order and
    // spelling, keeping each of them once. Every reading measured on 8.4.11.
    for (written, stored) in [
        ("exec,read", "read,exec"),
        ("read,read", "read"),
        ("Read,EXEC", "read,exec"),
        ("read,exec  ", "read,exec"),
        // A number, written as one or as text, is a bit for each member.
        ("3", "read,write"),
        ("0", ""),
        ("7", "read,write,exec"),
    ] {
        adapter
            .execute_query(&format!("INSERT INTO s (id, v) VALUES (100, '{written}')"))
            .unwrap_or_else(|_| panic!("{written} must be stored"));
        let CommandExecutionResult::ResultSet(read_back) = adapter
            .execute_query("SELECT v FROM s WHERE id = 100")
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read_back.rows[0][0].clone().unwrap()).unwrap(),
            stored,
            "{written}"
        );
        adapter
            .execute_query("DELETE FROM s WHERE id = 100")
            .unwrap();
    }

    // A space around a comma, an empty member and a name that is not one are
    // all refused, measured.
    for value in [
        "fly",
        "read,fly",
        "read, exec",
        " read",
        "read,",
        "read,,exec",
        "8",
    ] {
        assert_eq!(
            adapter.execute_query(&format!("INSERT INTO s (id, v) VALUES (9, '{value}')")),
            Err(FrontendErrorKind::NotAMember),
            "{value}"
        );
    }

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE s").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `s` (\n",
            "  `id` int NOT NULL,\n",
            "  `v` set('read','write','exec') DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // Measured: read + write + exec is 13 characters and the two commas that
    // join them make 15, times the four bytes utf8mb4 reserves.
    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT v FROM s WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let column = &selected.columns[0];
    assert_eq!(column.column_type, MYSQL_TYPE_STRING);
    assert_eq!(column.column_length, 60);
    assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
    assert_eq!(column.flags, MYSQL_SET_FLAG);
    assert_eq!(
        String::from_utf8(selected.rows[0][0].clone().unwrap()).unwrap(),
        "read,exec"
    );
}

/// `JSON_EXTRACT` reads one path out of a document, `JSON_UNQUOTE` takes the
/// quotes off the string it found, and `JSON_VALID` answers one or zero. Every
/// reading and every column below measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn a_json_column_can_be_read_a_path_at_a_time() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([106; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE j (id INT NOT NULL PRIMARY KEY, doc JSON)")
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO j (id, doc) VALUES (1, ",
            r#"'{"a": 1, "s": "x", "n": null, "b": true, "f": 1.5, "arr": [1,2,3], "o": {"k": "v"}}'"#,
            ")"
        ))
        .unwrap();

    for (expression, answer) in [
        ("JSON_EXTRACT(doc, '$.a')", Some("1")),
        ("JSON_EXTRACT(doc, '$.s')", Some("\"x\"")),
        ("JSON_EXTRACT(doc, '$.n')", Some("null")),
        ("JSON_EXTRACT(doc, '$.b')", Some("true")),
        ("JSON_EXTRACT(doc, '$.f')", Some("1.5")),
        // The engine writes a document without MySQL's spacing, so a nested
        // one is written again on the way out.
        ("JSON_EXTRACT(doc, '$.arr')", Some("[1, 2, 3]")),
        ("JSON_EXTRACT(doc, '$.o')", Some("{\"k\": \"v\"}")),
        ("JSON_EXTRACT(doc, '$.arr[1]')", Some("2")),
        ("JSON_EXTRACT(doc, '$.missing')", None),
        ("JSON_UNQUOTE(JSON_EXTRACT(doc, '$.s'))", Some("x")),
        ("JSON_UNQUOTE(JSON_EXTRACT(doc, '$.a'))", Some("1")),
        ("JSON_VALID(doc)", Some("1")),
        // MySQL spells the same two readings with operators too.
        ("doc -> '$.s'", Some("\"x\"")),
        ("doc -> '$.arr'", Some("[1, 2, 3]")),
        ("doc ->> '$.s'", Some("x")),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter
            .execute_query(&format!("SELECT {expression} FROM j WHERE id = 1"))
            .unwrap_or_else(|_| panic!("{expression} must be read"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            read.rows[0][0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap()),
            answer.map(str::to_owned),
            "{expression}"
        );
    }

    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query(concat!(
            "SELECT JSON_EXTRACT(doc, '$.a'), ",
            "JSON_UNQUOTE(JSON_EXTRACT(doc, '$.s')), JSON_VALID(doc) FROM j"
        ))
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.character_set,
                column.decimals,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                MYSQL_TYPE_JSON,
                u32::MAX - 3,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                NOT_FIXED_DECIMALS,
                MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_LONG_BLOB,
                u32::MAX,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                NOT_FIXED_DECIMALS,
                MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_LONGLONG,
                21,
                MYSQL_BINARY_COLLATION,
                0,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );

    // A path MySQL reads and the engine does not is refused rather than read
    // a different way: the wildcards are the ones that differ.
    for expression in [
        "JSON_EXTRACT(doc, '$.*')",
        "JSON_EXTRACT(doc, '$**.k')",
        "JSON_EXTRACT(doc, '$.a', '$.s')",
        "JSON_EXTRACT(doc, doc)",
    ] {
        assert!(
            adapter
                .execute_query(&format!("SELECT {expression} FROM j"))
                .is_err(),
            "{expression}"
        );
    }
}

/// `JSON_TYPE` names a document's kind, `JSON_LENGTH` counts what it holds at
/// the top, `JSON_KEYS` answers an object's keys as a document of their own,
/// and `JSON_QUOTE` writes text as a JSON string. The engine's own JSON
/// functions answer each of these differently, so each is read here instead.
/// Every reading and every column below measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn a_json_document_answers_its_kind_its_length_and_its_keys() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([107; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE j (id INT NOT NULL PRIMARY KEY, doc JSON, txt VARCHAR(40))"
        ))
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO j (id, doc, txt) VALUES (1, ",
            r#"'{"a": 1, "s": "x", "n": null, "b": true, "f": 1.5, "arr": [1,2,3], "o": {"k": "v"}}'"#,
            r#", 'he said "hi"'), (2, '[10, 20, 30]', 'plain'), (3, NULL, NULL)"#
        ))
        .unwrap();

    for (expression, row, answer) in [
        ("JSON_TYPE(doc)", 1, Some("OBJECT")),
        ("JSON_TYPE(doc)", 2, Some("ARRAY")),
        ("JSON_TYPE(doc)", 3, None),
        ("JSON_TYPE(JSON_EXTRACT(doc, '$.a'))", 1, Some("INTEGER")),
        ("JSON_TYPE(JSON_EXTRACT(doc, '$.s'))", 1, Some("STRING")),
        ("JSON_TYPE(JSON_EXTRACT(doc, '$.b'))", 1, Some("BOOLEAN")),
        ("JSON_TYPE(JSON_EXTRACT(doc, '$.f'))", 1, Some("DOUBLE")),
        ("JSON_TYPE(JSON_EXTRACT(doc, '$.o'))", 1, Some("OBJECT")),
        // The JSON null is a word; a path that names nothing is no answer.
        ("JSON_TYPE(doc -> '$.n')", 1, Some("NULL")),
        ("JSON_TYPE(doc -> '$.missing')", 1, None),
        ("JSON_LENGTH(doc)", 1, Some("7")),
        ("JSON_LENGTH(doc)", 2, Some("3")),
        ("JSON_LENGTH(doc)", 3, None),
        ("JSON_LENGTH(doc -> '$.arr')", 1, Some("3")),
        ("JSON_LENGTH(doc -> '$.a')", 1, Some("1")),
        (
            "JSON_KEYS(doc)",
            1,
            Some(r#"["a", "b", "f", "n", "o", "s", "arr"]"#),
        ),
        ("JSON_KEYS(doc)", 2, None),
        ("JSON_KEYS(doc -> '$.o')", 1, Some(r#"["k"]"#)),
        ("JSON_QUOTE(txt)", 1, Some(r#""he said \"hi\"""#)),
        ("JSON_QUOTE(txt)", 2, Some(r#""plain""#)),
        ("JSON_QUOTE(txt)", 3, None),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter
            .execute_query(&format!("SELECT {expression} FROM j WHERE id = {row}"))
            .unwrap_or_else(|_| panic!("{expression} must be read"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            read.rows[0][0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap()),
            answer.map(str::to_owned),
            "{expression} at {row}"
        );
    }

    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query(
            "SELECT JSON_TYPE(doc), JSON_LENGTH(doc), JSON_KEYS(doc), JSON_QUOTE(txt) FROM j",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.character_set,
                column.decimals,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                MYSQL_TYPE_VAR_STRING,
                68,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                NOT_FIXED_DECIMALS,
                MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_LONGLONG,
                21,
                MYSQL_BINARY_COLLATION,
                0,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                MYSQL_TYPE_JSON,
                u32::MAX - 3,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                NOT_FIXED_DECIMALS,
                MYSQL_BINARY_FLAG
            ),
            // Measured: a VARCHAR(40) gives 968 — six characters for each of
            // the forty and its two quotes, times the four bytes utf8mb4
            // reserves for a character.
            (
                MYSQL_TYPE_VAR_STRING,
                968,
                u16::from(DEFAULT_UTF8MB4_COLLATION),
                NOT_FIXED_DECIMALS,
                MYSQL_BINARY_FLAG
            ),
        ]
    );

    // The unquoted reading answers the text inside a string, which is not a
    // document to read again.
    assert!(adapter
        .execute_query("SELECT JSON_TYPE(doc ->> '$.s') FROM j")
        .is_err());
}

/// `STR_TO_DATE` reads a moment out of text by a format, and what it answers
/// is the format's doing rather than the text's: a format naming only day
/// parts answers a DATE, one naming only clock parts a TIME, and one naming
/// both a DATETIME. Every answer and every column below measured on MySQL
/// 8.4.11.
#[cfg(unix)]
#[test]
fn str_to_date_reads_a_moment_the_way_mysql_reads_it() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([111; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, s VARCHAR(40))")
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO t (id, s) VALUES ",
            "(1, '2026-09-06'), (2, '06/09/2026'), (3, '2026-09-06 01:02:03'), ",
            "(4, 'Sunday, September 6th 2026'), (5, '01:02:03 AM'), (6, 'not a date')"
        ))
        .unwrap();

    for (id, format, answer) in [
        (1, "%Y-%m-%d", Some("2026-09-06")),
        (2, "%d/%m/%Y", Some("2026-09-06")),
        (3, "%Y-%m-%d %H:%i:%s", Some("2026-09-06 01:02:03")),
        (4, "%W, %M %D %Y", Some("2026-09-06")),
        (5, "%r", Some("01:02:03")),
        (6, "%Y-%m-%d", None),
        // The text running out leaves the rest of the format reading nothing.
        (1, "%Y-%m-%d %H:%i:%s", Some("2026-09-06 00:00:00")),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter
            .execute_query(&format!(
                "SELECT STR_TO_DATE(s, '{format}') FROM t WHERE id = {id}"
            ))
            .unwrap_or_else(|_| panic!("{format} must be read"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            read.rows[0][0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap()),
            answer.map(str::to_owned),
            "{format} at {id}"
        );
    }

    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query(concat!(
            "SELECT STR_TO_DATE(s, '%Y-%m-%d'), STR_TO_DATE(s, '%H:%i:%s'), ",
            "STR_TO_DATE(s, '%Y-%m-%d %H:%i:%s') FROM t WHERE id = 1"
        ))
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.columns
            .iter()
            .map(|column| (
                column.column_type,
                column.column_length,
                column.character_set,
                column.decimals,
                column.flags
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                MYSQL_TYPE_DATE,
                10,
                MYSQL_BINARY_COLLATION,
                0,
                MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_TIME,
                10,
                MYSQL_BINARY_COLLATION,
                0,
                MYSQL_BINARY_FLAG
            ),
            (
                MYSQL_TYPE_DATETIME,
                19,
                MYSQL_BINARY_COLLATION,
                0,
                MYSQL_BINARY_FLAG
            ),
        ]
    );

    // A format naming a week number names no day on its own, so it is refused
    // rather than answered with a day it did not name.
    for format in ["%U %Y", "%v %x", "%w"] {
        assert!(
            adapter
                .execute_query(&format!("SELECT STR_TO_DATE(s, '{format}') FROM t"))
                .is_err(),
            "{format}"
        );
    }
}

/// `DATE_FORMAT` writes a moment out. The engine's own strftime answers a few
/// of MySQL's specifiers and none of the rest, so the whole of it is written by
/// the dialect. Every answer and every column below measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn date_format_writes_a_moment_the_way_mysql_writes_it() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([110; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, d DATE, dt DATETIME)")
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO t (id, d, dt) VALUES ",
            "(1, '2026-09-06', '2026-09-06 01:02:03'), (2, NULL, NULL)"
        ))
        .unwrap();

    for (format, written) in [
        ("%Y-%m-%d", "2026-09-06"),
        ("%Y-%m-%d %H:%i:%s", "2026-09-06 01:02:03"),
        ("%d/%m/%Y", "06/09/2026"),
        ("%W, %M %D %Y", "Sunday, September 6th 2026"),
        ("%a %b %e", "Sun Sep 6"),
        ("%r", "01:02:03 AM"),
        ("%T", "01:02:03"),
        ("%j %w %%", "249 0 %"),
        ("%U %u %V %v %X %x", "36 36 36 36 2026 2026"),
        ("on %Y", "on 2026"),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter
            .execute_query(&format!(
                "SELECT DATE_FORMAT(dt, '{format}') FROM t WHERE id = 1"
            ))
            .unwrap_or_else(|_| panic!("{format} must be written"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read.rows[0][0].clone().unwrap()).unwrap(),
            written,
            "{format}"
        );
    }

    // A day alone writes a time of midnight, and a NULL writes no value.
    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query("SELECT DATE_FORMAT(d, '%Y-%m-%d %H:%i:%s') FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.rows
            .iter()
            .map(|row| row[0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap()))
            .collect::<Vec<_>>(),
        vec![Some("2026-09-06 00:00:00".to_owned()), None]
    );

    // Measured: a VAR_STRING as wide as the format could make it — the format
    // `%Y-%m-%d %H:%i:%s` reserves twenty-four characters because `%H` alone
    // reserves seven — with the text collation and no flags at all.
    let column = &read.columns[0];
    assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(column.column_length, 96);
    assert_eq!(column.character_set, u16::from(DEFAULT_UTF8MB4_COLLATION));
    assert_eq!(column.decimals, NOT_FIXED_DECIMALS);
    assert_eq!(column.flags, 0);

    // The format has to be a literal, and the column has to hold a moment.
    assert!(adapter
        .execute_query("SELECT DATE_FORMAT(dt, d) FROM t")
        .is_err());
    assert!(adapter
        .execute_query("SELECT DATE_FORMAT(id, '%Y') FROM t")
        .is_err());
}

/// `LOCATE` with a place to start, `RAND`, `UUID` and `MD5`. Every answer and
/// every column below measured on MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn the_plain_scalar_calls_answer_what_mysql_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([114; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE s (id INT NOT NULL PRIMARY KEY, v VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO s (id, v) VALUES (1, 'aBc')")
        .unwrap();

    // Measured: LOCATE counts from the front of the whole haystack however far
    // in it was told to start, and a start before the first character finds
    // nothing.
    for (call, answer) in [
        ("LOCATE('B', v, 1)", "2"),
        ("LOCATE('B', v, 2)", "2"),
        ("LOCATE('B', v, 3)", "0"),
        ("LOCATE('x', v, 1)", "0"),
        ("LOCATE('B', v, 0)", "0"),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter
            .execute_query(&format!("SELECT {call} FROM s"))
            .unwrap_or_else(|_| panic!("{call} must be read"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        // The same column a two-argument LOCATE reports.
        assert_eq!(read.columns[0].column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(read.columns[0].column_length, 11);
    }

    let CommandExecutionResult::ResultSet(digest) =
        adapter.execute_query("SELECT MD5(v) FROM s").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    // Measured: the thirty-two hexadecimal characters of the digest, in a
    // VAR_STRING of 128 with the text collation and no flags.
    assert_eq!(
        String::from_utf8(digest.rows[0][0].clone().unwrap()).unwrap(),
        "dbbbbe4975e026e04a687871f296a2b2"
    );
    assert_eq!(digest.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(digest.columns[0].column_length, 128);
    assert_eq!(digest.columns[0].flags, 0);

    let CommandExecutionResult::ResultSet(rolled) =
        adapter.execute_query("SELECT RAND() FROM s").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let value: f64 = String::from_utf8(rolled.rows[0][0].clone().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert!((0.0..1.0).contains(&value), "RAND answered {value}");
    assert_eq!(rolled.columns[0].column_type, MYSQL_TYPE_DOUBLE);
    assert_eq!(rolled.columns[0].column_length, 23);
    assert_eq!(
        rolled.columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    let CommandExecutionResult::ResultSet(named) =
        adapter.execute_query("SELECT UUID() FROM s").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let identifier = String::from_utf8(named.rows[0][0].clone().unwrap()).unwrap();
    assert_eq!(identifier.len(), 36, "UUID answered {identifier}");
    assert_eq!(named.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(named.columns[0].column_length, 144);

    // A seeded RAND answers a sequence the engine has no way to answer.
    assert!(adapter.execute_query("SELECT RAND(1) FROM s").is_err());
}

/// An `UPDATE` names the rows it changes through a join. Every row below was
/// measured on MySQL 8.4.11 over the same starting rows.
#[cfg(unix)]
#[test]
fn a_joined_update_changes_what_mysql_changes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([113; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE a (id INT NOT NULL PRIMARY KEY, name VARCHAR(8), n INT)")
        .unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE b (id INT NOT NULL PRIMARY KEY, a_id INT, ",
            "tag VARCHAR(8), m INT)"
        ))
        .unwrap();

    for (statement, in_a, in_b) in [
        (
            "UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0",
            "0,20,0",
            "100,110,120",
        ),
        (
            "UPDATE a JOIN b ON a.id = b.a_id SET a.n = 0 WHERE b.tag = 'z'",
            "10,20,0",
            "100,110,120",
        ),
        (
            "UPDATE a LEFT JOIN b ON a.id = b.a_id SET a.n = 0 WHERE b.id IS NULL",
            "10,0,30",
            "100,110,120",
        ),
        (
            "UPDATE a JOIN b ON a.id = b.a_id SET b.m = 0",
            "10,20,30",
            "0,0,0",
        ),
    ] {
        adapter.execute_query("DELETE FROM a").unwrap();
        adapter.execute_query("DELETE FROM b").unwrap();
        adapter
            .execute_query(
                "INSERT INTO a (id, name, n) VALUES (1,'one',10),(2,'two',20),(3,'three',30)",
            )
            .unwrap();
        adapter
            .execute_query(concat!(
                "INSERT INTO b (id, a_id, tag, m) VALUES ",
                "(10,1,'x',100),(11,1,'y',110),(12,3,'z',120)"
            ))
            .unwrap();
        adapter
            .execute_query(statement)
            .unwrap_or_else(|_| panic!("{statement} must run"));
        for (table, column, expected) in [("a", "n", in_a), ("b", "m", in_b)] {
            let CommandExecutionResult::ResultSet(read) = adapter
                .execute_query(&format!("SELECT {column} FROM {table} ORDER BY id"))
                .unwrap()
            else {
                panic!("SELECT must return a result set");
            };
            assert_eq!(
                read.rows
                    .iter()
                    .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
                    .collect::<Vec<_>>()
                    .join(","),
                expected,
                "{statement} left {table}"
            );
        }
    }
}

/// A `DELETE` names the rows it removes through a join. Every row left behind
/// below was measured on MySQL 8.4.11 over the same starting rows.
#[cfg(unix)]
#[test]
fn a_joined_delete_removes_what_mysql_removes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([112; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE a (id INT NOT NULL PRIMARY KEY, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE b (id INT NOT NULL PRIMARY KEY, a_id INT, tag VARCHAR(8))")
        .unwrap();

    for (statement, left_in_a, left_in_b) in [
        ("DELETE a FROM a JOIN b ON a.id = b.a_id", "2", "10,11,12"),
        (
            "DELETE a FROM a JOIN b ON a.id = b.a_id WHERE b.tag = 'z'",
            "1,2",
            "10,11,12",
        ),
        (
            "DELETE FROM a USING a JOIN b ON a.id = b.a_id WHERE b.tag = 'z'",
            "1,2",
            "10,11,12",
        ),
        // The orphan delete, which needs the outer join to answer at all.
        (
            "DELETE a FROM a LEFT JOIN b ON a.id = b.a_id WHERE b.id IS NULL",
            "1,3",
            "10,11,12",
        ),
        ("DELETE a FROM a, b WHERE a.id = b.a_id", "2", "10,11,12"),
        (
            "DELETE b FROM a JOIN b ON a.id = b.a_id WHERE a.name = 'one'",
            "1,2,3",
            "12",
        ),
    ] {
        adapter.execute_query("DELETE FROM a").unwrap();
        adapter.execute_query("DELETE FROM b").unwrap();
        adapter
            .execute_query("INSERT INTO a (id, name) VALUES (1,'one'),(2,'two'),(3,'three')")
            .unwrap();
        adapter
            .execute_query("INSERT INTO b (id, a_id, tag) VALUES (10,1,'x'),(11,1,'y'),(12,3,'z')")
            .unwrap();
        adapter
            .execute_query(statement)
            .unwrap_or_else(|_| panic!("{statement} must run"));
        for (table, left) in [("a", left_in_a), ("b", left_in_b)] {
            let CommandExecutionResult::ResultSet(read) = adapter
                .execute_query(&format!("SELECT id FROM {table} ORDER BY id"))
                .unwrap()
            else {
                panic!("SELECT must return a result set");
            };
            assert_eq!(
                read.rows
                    .iter()
                    .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
                    .collect::<Vec<_>>()
                    .join(","),
                left,
                "{statement} left {table}"
            );
        }
    }

    // Measured: MySQL answers 1109 for a target the join does not read, a
    // syntax error for an ORDER BY on a joined DELETE, and deletes from both
    // tables at once for two targets — which each need their own statement
    // here.
    for statement in [
        "DELETE c FROM a JOIN b ON a.id = b.a_id",
        "DELETE a FROM a JOIN b ON a.id = b.a_id ORDER BY a.id",
        "DELETE a, b FROM a JOIN b ON a.id = b.a_id",
    ] {
        assert!(adapter.execute_query(statement).is_err(), "{statement}");
    }
}

/// A subquery naming the outer statement's column is a correlated one. Every
/// answer below was measured on MySQL 8.4.11 over the same rows.
#[cfg(unix)]
#[test]
fn a_correlated_subquery_answers_what_mysql_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([109; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE a (id INT NOT NULL PRIMARY KEY, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE b (id INT NOT NULL PRIMARY KEY, a_id INT, tag VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO a (id, name) VALUES (1,'one'),(2,'two'),(3,'three')")
        .unwrap();
    adapter
        .execute_query("INSERT INTO b (id, a_id, tag) VALUES (10,1,'x'),(11,1,'y'),(12,3,'z')")
        .unwrap();

    for (sql, answer) in [
        (
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id) ORDER BY id",
            vec!["1", "3"],
        ),
        (
            "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id) ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM a WHERE id IN (SELECT b.a_id FROM b WHERE b.tag = 'z') ORDER BY id",
            vec!["3"],
        ),
        (
            concat!(
                "SELECT id FROM a WHERE EXISTS ",
                "(SELECT 1 FROM b WHERE b.a_id = a.id AND b.tag = 'y') ORDER BY id"
            ),
            vec!["1"],
        ),
        (
            concat!(
                "SELECT a.name FROM a WHERE EXISTS ",
                "(SELECT 1 FROM b WHERE b.a_id = a.id) ORDER BY a.id"
            ),
            vec!["one", "three"],
        ),
        // An unqualified name inside the subquery is the subquery's column
        // when it has one — `tag` is b's — and the outer statement's when it
        // does not, which is how MySQL reads it: `name` is a's.
        (
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE tag = 'z') ORDER BY id",
            vec!["1", "2", "3"],
        ),
        (
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE name = 'one') ORDER BY id",
            vec!["1"],
        ),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter
            .execute_query(sql)
            .unwrap_or_else(|_| panic!("{sql} must be read"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            read.rows
                .iter()
                .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            answer,
            "{sql}"
        );
    }

    // A literal that does not fit the subquery's own column is refused there
    // as it is anywhere else.
    assert!(adapter
        .execute_query("SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE a_id = 'x')")
        .is_err());
}

/// MySQL's comma join is a cross join, and the `WHERE` that names a column on
/// each side is what bounds it. Measured on MySQL 8.4.11: the two spellings
/// answer the same rows in the same order.
#[cfg(unix)]
#[test]
fn a_comma_join_answers_what_the_written_join_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([108; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE users (id INT NOT NULL PRIMARY KEY, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query(concat!(
            "CREATE TABLE accounts (id INT NOT NULL PRIMARY KEY, ",
            "user_id INT, label VARCHAR(8))"
        ))
        .unwrap();
    adapter
        .execute_query("INSERT INTO users (id, name) VALUES (1, 'ann'), (2, 'bo')")
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO accounts (id, user_id, label) VALUES ",
            "(10, 1, 'one'), (11, 2, 'two'), (12, 9, 'none')"
        ))
        .unwrap();

    fn rows_of(result: CommandExecutionResult) -> Vec<Vec<String>> {
        let CommandExecutionResult::ResultSet(result) = result else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| {
                        value
                            .clone()
                            .map_or_else(String::new, |bytes| String::from_utf8(bytes).unwrap())
                    })
                    .collect()
            })
            .collect()
    }

    let bounded = rows_of(
        adapter
            .execute_query(concat!(
                "SELECT users.name, accounts.label FROM users, accounts ",
                "WHERE users.id = accounts.user_id ORDER BY users.id"
            ))
            .unwrap(),
    );
    assert_eq!(
        bounded,
        vec![
            vec!["ann".to_owned(), "one".to_owned()],
            vec!["bo".to_owned(), "two".to_owned()],
        ]
    );

    // The written join answers the same rows.
    let written = rows_of(
        adapter
            .execute_query(concat!(
                "SELECT users.name, accounts.label FROM users ",
                "JOIN accounts ON users.id = accounts.user_id ORDER BY users.id"
            ))
            .unwrap(),
    );
    assert_eq!(bounded, written);

    // Unbounded, it is every pair.
    let every_pair = rows_of(
        adapter
            .execute_query("SELECT users.id, accounts.id FROM users, accounts")
            .unwrap(),
    );
    assert_eq!(every_pair.len(), 6);

    // An unqualified name in a joined projection is the one column that
    // carries it, which is how MySQL resolves it.
    let unqualified = rows_of(
        adapter
            .execute_query(concat!(
                "SELECT name, label FROM users, accounts ",
                "WHERE users.id = accounts.user_id ORDER BY users.id"
            ))
            .unwrap(),
    );
    assert_eq!(
        unqualified,
        vec![
            vec!["ann".to_owned(), "one".to_owned()],
            vec!["bo".to_owned(), "two".to_owned()],
        ]
    );
    // A name both tables carry is ambiguous: measured on MySQL 8.4.11, 1052.
    assert_eq!(
        adapter.execute_query("SELECT id FROM users, accounts WHERE users.id = accounts.user_id"),
        Err(FrontendErrorKind::AmbiguousColumn)
    );

    // A comparison against a literal names the table its column belongs to,
    // which is what says the column's type to check the literal against.
    let narrowed = rows_of(
        adapter
            .execute_query(concat!(
                "SELECT users.name FROM users, accounts ",
                "WHERE users.id = accounts.user_id AND accounts.label = 'two'"
            ))
            .unwrap(),
    );
    assert_eq!(narrowed, vec![vec!["bo".to_owned()]]);

    // A literal that does not fit the column it is compared against is
    // refused, in a join as it is over one table.
    assert!(adapter
        .execute_query(concat!(
            "SELECT users.name FROM users, accounts ",
            "WHERE users.id = accounts.user_id AND accounts.user_id = 'two'"
        ))
        .is_err());
}

/// A `JSON` column holds a document, and MySQL stores what it parsed rather
/// than the text it was given: measured on MySQL 8.4.11, `{"b":1,"a":2}` reads
/// back as `{"a": 2, "b": 1}`. Text that is not a document answers 3140.
#[cfg(unix)]
#[test]
fn a_json_column_holds_the_document_mysql_would_store() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([103; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE j (id INT NOT NULL PRIMARY KEY, doc JSON)")
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO j (id, doc) VALUES ",
            "(1, '{\"b\":1,\"a\":2}'), (2, '[1,  2,3]'), (3, '1e15'), (4, NULL)"
        ))
        .unwrap();

    for text in ["plain", "{oops}", "[1,]", ""] {
        assert_eq!(
            adapter.execute_query(&format!("INSERT INTO j (id, doc) VALUES (9, '{text}')")),
            Err(FrontendErrorKind::InvalidJsonText),
            "{text}"
        );
    }
    // Measured: a value that is not text at all answers the same error, with
    // rapidjson's `not a JSON text, may need CAST`.
    assert_eq!(
        adapter.execute_query("INSERT INTO j (id, doc) VALUES (9, 5)"),
        Err(FrontendErrorKind::InvalidJsonText)
    );

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE j").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `j` (\n",
            "  `id` int NOT NULL,\n",
            "  `doc` json DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT doc FROM j ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    // Measured on MySQL 8.4.11: a JSON column reports type 245, the widest
    // length there is, the binary collation, and the blob and binary flags.
    let column = &selected.columns[0];
    assert_eq!(column.column_type, MYSQL_TYPE_JSON);
    assert_eq!(column.column_length, u32::MAX);
    assert_eq!(column.character_set, MYSQL_BINARY_COLLATION);
    assert_eq!(column.flags, MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG);
    let stored: Vec<Option<String>> = selected
        .rows
        .iter()
        .map(|row| {
            row[0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap())
        })
        .collect();
    assert_eq!(
        stored,
        vec![
            Some(r#"{"a": 2, "b": 1}"#.to_owned()),
            Some("[1, 2, 3]".to_owned()),
            // A document that is a bare number is stored as it reads, not
            // converted: the engine would otherwise hold 1e15 as an integer
            // and read back 1000000000000000.
            Some("1e15".to_owned()),
            None,
        ]
    );

    // An UPDATE runs through the same rewrite an INSERT does.
    adapter
        .execute_query(r#"UPDATE j SET doc = '{"z":1,"y":[2,  3]}' WHERE id = 1"#)
        .unwrap();
    let CommandExecutionResult::ResultSet(updated) = adapter
        .execute_query("SELECT doc FROM j WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        String::from_utf8(updated.rows[0][0].clone().unwrap()).unwrap(),
        r#"{"y": [2, 3], "z": 1}"#
    );

    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM j").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        String::from_utf8(columns.rows[1][1].clone().unwrap()).unwrap(),
        "json"
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

#[cfg(unix)]
#[test]
fn show_count_warnings_and_errors_reports_diagnostics_counts() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([29; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();

    // Initial state: 0 warnings and 0 errors.
    let CommandExecutionResult::ResultSet(warnings_count) =
        adapter.execute_query("SHOW COUNT(*) WARNINGS").unwrap()
    else {
        panic!("SHOW COUNT(*) WARNINGS must return a result set");
    };
    assert_eq!(warnings_count.rows, vec![vec![Some(b"0".to_vec())]]);
    assert_eq!(warnings_count.columns.len(), 1);
    let col = &warnings_count.columns[0];
    assert_eq!(col.name, "@@session.warning_count");
    assert_eq!(col.column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(col.column_length, 21);
    assert_eq!(col.decimals, 0);
    assert_eq!(
        col.flags,
        MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    let CommandExecutionResult::ResultSet(errors_count) =
        adapter.execute_query("SHOW COUNT(*) ERRORS").unwrap()
    else {
        panic!("SHOW COUNT(*) ERRORS must return a result set");
    };
    assert_eq!(errors_count.rows, vec![vec![Some(b"0".to_vec())]]);
    assert_eq!(errors_count.columns.len(), 1);
    let col = &errors_count.columns[0];
    assert_eq!(col.name, "@@session.error_count");
    assert_eq!(col.column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(col.column_length, 21);
    assert_eq!(col.decimals, 0);
    assert_eq!(
        col.flags,
        MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    // Trigger one warning (Note 1051).
    let CommandExecutionResult::Ok(dropped) = adapter
        .execute_query("DROP TABLE IF EXISTS nosuchtable")
        .unwrap()
    else {
        panic!("DROP TABLE must report an OK");
    };
    assert_eq!(dropped.warnings, 1);

    // Warning count is 1, error count remains 0.
    let CommandExecutionResult::ResultSet(after_warn) =
        adapter.execute_query("SHOW COUNT(*) WARNINGS").unwrap()
    else {
        panic!("SHOW COUNT(*) WARNINGS must return a result set");
    };
    assert_eq!(after_warn.rows, vec![vec![Some(b"1".to_vec())]]);

    let CommandExecutionResult::ResultSet(errors_after_warn) =
        adapter.execute_query("SHOW COUNT(*) ERRORS").unwrap()
    else {
        panic!("SHOW COUNT(*) ERRORS must return a result set");
    };
    assert_eq!(errors_after_warn.rows, vec![vec![Some(b"0".to_vec())]]);

    // Neither SHOW COUNT nor SHOW WARNINGS clears the count.
    let CommandExecutionResult::ResultSet(warnings) =
        adapter.execute_query("SHOW WARNINGS").unwrap()
    else {
        panic!("SHOW WARNINGS must return a result set");
    };
    assert_eq!(warnings.rows.len(), 1);

    let CommandExecutionResult::ResultSet(still_one) =
        adapter.execute_query("SHOW COUNT(*) WARNINGS").unwrap()
    else {
        panic!("SHOW COUNT(*) WARNINGS must return a result set");
    };
    assert_eq!(still_one.rows, vec![vec![Some(b"1".to_vec())]]);

    // A normal succeeding statement resets warning count to 0.
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY)")
        .unwrap();

    let CommandExecutionResult::ResultSet(cleared) =
        adapter.execute_query("SHOW COUNT(*) WARNINGS").unwrap()
    else {
        panic!("SHOW COUNT(*) WARNINGS must return a result set");
    };
    assert_eq!(cleared.rows, vec![vec![Some(b"0".to_vec())]]);
}

#[cfg(unix)]
#[test]
fn show_tables_and_full_tables_filter_by_like_pattern_and_format_column_name() {
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
        .execute_query("CREATE TABLE alpha (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE beta (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE Alpaca (id INT NOT NULL PRIMARY KEY)")
        .unwrap();

    // Plain SHOW TABLES: column name has no pattern.
    let CommandExecutionResult::ResultSet(all) = adapter.execute_query("SHOW TABLES").unwrap()
    else {
        panic!("SHOW TABLES must return a result set");
    };
    assert_eq!(all.columns.len(), 1);
    assert_eq!(all.columns[0].name, "Tables_in_reports");
    let names: Vec<Vec<u8>> = all
        .rows
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            b"alpaca".to_vec(),
            b"alpha".to_vec(),
            b"beta".to_vec(),
            b"records".to_vec(),
        ]
    );

    // SHOW TABLES LIKE 'a%': column name is `Tables_in_reports (a%)`.
    let CommandExecutionResult::ResultSet(a_tables) =
        adapter.execute_query("SHOW TABLES LIKE 'a%'").unwrap()
    else {
        panic!("SHOW TABLES LIKE must return a result set");
    };
    assert_eq!(a_tables.columns.len(), 1);
    assert_eq!(a_tables.columns[0].name, "Tables_in_reports (a%)");
    let names: Vec<Vec<u8>> = a_tables
        .rows
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(names, vec![b"alpaca".to_vec(), b"alpha".to_vec()]);

    // Measured on MySQL 8.4.11: `SHOW TABLES LIKE 'A%'` answers nothing, a
    // table name being the one `SHOW ... LIKE` subject matched by case.
    let CommandExecutionResult::ResultSet(upper_a) =
        adapter.execute_query("SHOW TABLES LIKE 'A%'").unwrap()
    else {
        panic!("SHOW TABLES LIKE must return a result set");
    };
    assert_eq!(upper_a.columns[0].name, "Tables_in_reports (A%)");
    assert!(upper_a.rows.is_empty());

    // SHOW TABLES LIKE 'alph_': matches single trailing char `alpha`.
    let CommandExecutionResult::ResultSet(alph) =
        adapter.execute_query("SHOW TABLES LIKE 'alph_'").unwrap()
    else {
        panic!("SHOW TABLES LIKE must return a result set");
    };
    assert_eq!(alph.columns[0].name, "Tables_in_reports (alph_)");
    let names: Vec<Vec<u8>> = alph
        .rows
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(names, vec![b"alpha".to_vec()]);

    // SHOW TABLES LIKE 'nomatch': 0 rows, but column is returned with pattern in name.
    let CommandExecutionResult::ResultSet(nomatch) =
        adapter.execute_query("SHOW TABLES LIKE 'nomatch'").unwrap()
    else {
        panic!("SHOW TABLES LIKE must return a result set");
    };
    assert_eq!(nomatch.columns.len(), 1);
    assert_eq!(nomatch.columns[0].name, "Tables_in_reports (nomatch)");
    assert!(nomatch.rows.is_empty());

    // SHOW FULL TABLES LIKE 'a%': column 1 has pattern, column 2 is Table_type.
    let CommandExecutionResult::ResultSet(full) =
        adapter.execute_query("SHOW FULL TABLES LIKE 'a%'").unwrap()
    else {
        panic!("SHOW FULL TABLES LIKE must return a result set");
    };
    assert_eq!(full.columns.len(), 2);
    assert_eq!(full.columns[0].name, "Tables_in_reports (a%)");
    assert_eq!(full.columns[1].name, "Table_type");
    assert_eq!(
        full.rows,
        vec![
            vec![Some(b"alpaca".to_vec()), Some(b"BASE TABLE".to_vec())],
            vec![Some(b"alpha".to_vec()), Some(b"BASE TABLE".to_vec())],
        ]
    );
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
            256,
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
        vec![vec![Some(b"aXYc".to_vec()), Some(b"aBc".to_vec()),]]
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

    // Measured over a utf8mb4 connection, which is the only one this server
    // serves: HEX answers the connection's own collation, and a width of two
    // hex characters for each of the column's, times the bytes utf8mb4
    // reserves for one — 8 * 8 * 4 for a VARCHAR(8).
    let CommandExecutionResult::ResultSet(hexed) =
        adapter.execute_query("SELECT HEX(v) FROM s").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        hexed.columns[0].character_set,
        u16::from(DEFAULT_UTF8MB4_COLLATION)
    );
    assert_eq!(hexed.columns[0].column_length, 256);
    assert_eq!(hexed.rows, vec![vec![Some(b"614263".to_vec())]]);

    // Measured on MySQL 8.4.11: HEX over a column holding a number writes the
    // number in hexadecimal rather than its text's bytes, and reports 64
    // whatever the number's width is — a number is written in at most sixteen
    // hexadecimal characters.
    let CommandExecutionResult::ResultSet(hexed_number) =
        adapter.execute_query("SELECT HEX(n) FROM s").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(hexed_number.columns[0].column_length, 64);
    // Measured: a negative is written as the sixty-four bits it holds, so -1
    // is FFFFFFFFFFFFFFFF and -7 is FFFFFFFFFFFFFFF9.
    assert_eq!(
        String::from_utf8(hexed_number.rows[0][0].clone().unwrap()).unwrap(),
        "FFFFFFFFFFFFFFF9"
    );

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
        // Measured on MySQL 8.4.11: SIGN answers LONGLONG, length 21, BINARY NUM flags.
        (
            "SELECT SIGN(n) FROM s",
            "SIGN(n)",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT SIGN(id) FROM s",
            "SIGN(id)",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        // Measured on MySQL 8.4.11: SQRT / POW answer DOUBLE of length 23, decimals 31, BINARY NUM flags.
        (
            "SELECT SQRT(n) FROM s",
            "SQRT(n)",
            MYSQL_TYPE_DOUBLE,
            23,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT POW(n, 2) FROM s",
            "POW(n, 2)",
            MYSQL_TYPE_DOUBLE,
            23,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT POWER(n, 2) FROM s",
            "POWER(n, 2)",
            MYSQL_TYPE_DOUBLE,
            23,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        // Measured on MySQL 8.4.11: MOD answers LONGLONG of column length 11, BINARY NUM flags.
        (
            "SELECT MOD(n, 3) FROM s",
            "MOD(n, 3)",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        // Measured on MySQL 8.4.11: GREATEST / LEAST answer widest type / length.
        (
            "SELECT GREATEST(n, 10) FROM s",
            "GREATEST(n, 10)",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT GREATEST(id, 10) FROM s",
            "GREATEST(id, 10)",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT LEAST(n, 10) FROM s",
            "LEAST(n, 10)",
            MYSQL_TYPE_LONGLONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT GREATEST(v, 'xy') FROM s",
            "GREATEST(v, 'xy')",
            MYSQL_TYPE_VAR_STRING,
            32,
            0,
        ),
        // Measured on MySQL 8.4.11: NULLIF preserves column shape but clears NOT_NULL flag.
        (
            "SELECT NULLIF(n, 0) FROM s",
            "NULLIF(n, 0)",
            MYSQL_TYPE_LONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT NULLIF(id, 0) FROM s",
            "NULLIF(id, 0)",
            MYSQL_TYPE_LONG,
            11,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SELECT NULLIF(v, 'abc') FROM s",
            "NULLIF(v, 'abc')",
            MYSQL_TYPE_VAR_STRING,
            32,
            0,
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

    // Measured on MySQL 8.4.11: SIGN(-7) = -1, SQRT(-7) = NULL, POW(-7, 2) = 49, MOD(-7, 3) = -1.
    let CommandExecutionResult::ResultSet(math_vals) = adapter
        .execute_query("SELECT SIGN(n), SQRT(n), POW(n, 2), MOD(n, 3) FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        math_vals.rows,
        vec![vec![
            Some(b"-1".to_vec()),
            None,
            Some(b"49".to_vec()),
            Some(b"-1".to_vec()),
        ]]
    );

    // Measured on MySQL 8.4.11: GREATEST, LEAST, NULLIF values.
    let CommandExecutionResult::ResultSet(extremum_vals) = adapter
        .execute_query("SELECT GREATEST(n, 10), LEAST(n, 10), NULLIF(n, -7), NULLIF(n, 0), NULLIF(v, 'aBc'), NULLIF(v, 'xyz') FROM s")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        extremum_vals.rows,
        vec![vec![
            Some(b"10".to_vec()),
            Some(b"-7".to_vec()),
            None,
            Some(b"-7".to_vec()),
            None,
            Some(b"aBc".to_vec()),
        ]]
    );

    // Mixed types across columns in GREATEST is unsupported.
    assert!(adapter
        .execute_query("SELECT GREATEST(n, v) FROM s")
        .is_err());
    assert!(adapter
        .execute_query("SELECT GREATEST(n, 'x') FROM s")
        .is_err());

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

    // Parenthesised branches answer identical rows and metadata
    let CommandExecutionResult::ResultSet(paren_distinct) = adapter
        .execute_query("(SELECT id FROM ua) UNION (SELECT id FROM ub) ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(paren_distinct.rows, distinct.rows);
    assert_eq!(paren_distinct.columns, distinct.columns);

    let CommandExecutionResult::ResultSet(paren_all) = adapter
        .execute_query("(SELECT id FROM ua) UNION ALL (SELECT id FROM ub) ORDER BY id LIMIT 2")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(paren_all.rows, all.rows);
    assert_eq!(paren_all.columns, all.columns);

    // Branches carrying their own ORDER BY or LIMIT remain refused
    assert!(adapter
        .execute_query("(SELECT id FROM ua ORDER BY id) UNION (SELECT id FROM ub)")
        .is_err());
    assert!(adapter
        .execute_query("(SELECT id FROM ua LIMIT 1) UNION (SELECT id FROM ub)")
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
            "t DATETIME, s TIMESTAMP NULL, v VARCHAR(4), r DOUBLE, f FLOAT, n BIGINT, ",
            "day DATE, span TIME, era YEAR)"
        ))
        .unwrap();
    adapter
        .execute_query(concat!(
            "INSERT INTO b (id, c, d, t, s, v, r, f, n, day, span, era) VALUES ",
            "(1, 'ab', 1.25, '2026-09-06 01:02:03', '2026-09-06 00:00:00', 'zz', 2.5, 1.5, 9, '2026-09-06', '838:59:59', 2026), ",
            // The second row's DECIMAL needs padding to its declared scale,
            // which the binary protocol does as the text one does.
            "(2, 'ab', 1.5, '2026-09-06 01:02:03', '2026-09-06 00:00:00', 'zz', 2.5, 1.5, 9, '2026-09-06', '-01:02:03', 2026)"
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
    // A YEAR crosses as the two bytes a SHORT does, being the number it is.
    assert_eq!(
        binary("SELECT era FROM b WHERE id = 1"),
        BinaryResultValue::Integer(2026)
    );
    // A TIME crosses as its own field form, which carries a sign and the whole
    // days its hours run past — 838 hours is 34 days and 22 hours.
    assert_eq!(
        binary("SELECT span FROM b WHERE id = 1"),
        BinaryResultValue::Time {
            negative: false,
            days: 34,
            hour: 22,
            minute: 59,
            second: 59,
        }
    );
    assert_eq!(
        binary("SELECT span FROM b WHERE id = 2"),
        BinaryResultValue::Time {
            negative: true,
            days: 0,
            hour: 1,
            minute: 2,
            second: 3,
        }
    );
    // A DATE crosses as the same field form with the time left off, which is
    // the four-byte length MySQL sends for one.
    assert_eq!(
        binary("SELECT day FROM b WHERE id = 1"),
        BinaryResultValue::DateTime {
            year: 2026,
            month: 9,
            day: 6,
            hour: 0,
            minute: 0,
            second: 0,
        }
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
        .execute_query(
            "INSERT INTO t (id, team, n) VALUES (1, 'a', 10), (2, 'a', 30), (3, 'b', 20)",
        )
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

/// A `HAVING` over a statement that groups nothing and aggregates nothing
/// filters rows, not groups. Measured on MySQL 8.4.11 over rows
/// (1,5,'a'), (2,3,'b'), (3,9,'c'), (4,NULL,'d'): `SELECT id FROM t HAVING
/// id > 1` answers 2, 3 and 4, and the column it reports is `id` itself —
/// LONG, length 11, NOT_NULL. What it may not name is a column the projection
/// does not carry: `HAVING n > 4` answers 1054 there, with or without a WHERE
/// beside it.
#[cfg(unix)]
#[test]
fn a_having_without_a_group_by_filters_rows_when_nothing_is_aggregated() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([214; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO t (id, n, name) VALUES (1, 5, 'a'), (2, 3, 'b'), (3, 9, 'c'), (4, NULL, 'd')",
        )
        .unwrap();

    let CommandExecutionResult::ResultSet(filtered) = adapter
        .execute_query("SELECT id FROM t HAVING id > 1 ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        filtered.rows,
        vec![
            vec![Some(b"2".to_vec())],
            vec![Some(b"3".to_vec())],
            vec![Some(b"4".to_vec())],
        ]
    );
    assert_eq!(filtered.columns[0].name, "id");
    assert_eq!(filtered.columns[0].column_type, MYSQL_TYPE_LONG);
    assert_eq!(filtered.columns[0].column_length, 11);
    assert!(filtered.columns[0].flags & MYSQL_NOT_NULL_FLAG != 0);

    // A WHERE beside it narrows the rows first; both tests hold.
    let CommandExecutionResult::ResultSet(both) = adapter
        .execute_query("SELECT id, n FROM t WHERE id > 1 HAVING n > 4 ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        both.rows,
        vec![vec![Some(b"3".to_vec()), Some(b"9".to_vec())]]
    );

    // The row holding nothing is found the same way a WHERE finds it.
    let CommandExecutionResult::ResultSet(nothing) = adapter
        .execute_query("SELECT id, n FROM t HAVING n IS NULL")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(nothing.rows, vec![vec![Some(b"4".to_vec()), None]]);

    // 1054: the HAVING names a column the projection does not carry.
    assert!(adapter
        .execute_query("SELECT id FROM t HAVING n > 4 ORDER BY id")
        .is_err());
    assert!(adapter
        .execute_query("SELECT id FROM t WHERE id > 1 HAVING n > 4 ORDER BY id")
        .is_err());
    assert!(adapter
        .execute_query("SELECT id FROM t HAVING name = 'b' ORDER BY id")
        .is_err());
}

/// `a.*` asks for one source's columns, which is how a joined statement takes
/// a whole row from one side. Measured on MySQL 8.4.11 over parents
/// (1,5,'x'), (2,3,'y'), (3,9,'z') and children (1,1,'p'), (2,3,'q'): the
/// columns come in declaration order, each naming its own table, and the
/// wildcard mixes with a plain column and with a second wildcard. On the outer
/// side of a LEFT JOIN the unmatched row answers NULL for every column and the
/// NOT_NULL flag is dropped. Under an alias the columns report the alias as
/// their table and the real name as the original table, and naming the table
/// an alias renamed answers 1051 — refused here as well.
#[cfg(unix)]
#[test]
fn a_qualified_wildcard_takes_one_sources_columns() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([215; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(20))",
        "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT, tag VARCHAR(20))",
        "INSERT INTO parent (id, n, name) VALUES (1, 5, 'x'), (2, 3, 'y'), (3, 9, 'z')",
        "INSERT INTO child (id, parent_id, tag) VALUES (1, 1, 'p'), (2, 3, 'q')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    let CommandExecutionResult::ResultSet(joined) = adapter
        .execute_query(
            "SELECT parent.* FROM parent JOIN child ON child.parent_id = parent.id ORDER BY parent.id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        joined
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.table.as_str()))
            .collect::<Vec<_>>(),
        vec![("id", "parent"), ("n", "parent"), ("name", "parent")]
    );
    assert_eq!(
        joined.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"5".to_vec()),
                Some(b"x".to_vec())
            ],
            vec![
                Some(b"3".to_vec()),
                Some(b"9".to_vec()),
                Some(b"z".to_vec())
            ],
        ]
    );

    // It mixes with a plain column and with a second wildcard, each column
    // still naming the table it came from.
    let CommandExecutionResult::ResultSet(both) = adapter
        .execute_query(
            "SELECT parent.*, child.* FROM parent JOIN child ON child.parent_id = parent.id ORDER BY parent.id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        both.columns
            .iter()
            .map(|column| (column.name.as_str(), column.table.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("id", "parent"),
            ("n", "parent"),
            ("name", "parent"),
            ("id", "child"),
            ("parent_id", "child"),
            ("tag", "child"),
        ]
    );
    assert_eq!(both.rows.len(), 2);
    assert_eq!(both.rows[0].len(), 6);

    // The unmatched row on the outer side answers NULL for every column, and
    // the primary key stops reporting NOT_NULL because of it.
    let CommandExecutionResult::ResultSet(outer) = adapter
        .execute_query(
            "SELECT child.* FROM parent LEFT JOIN child ON child.parent_id = parent.id ORDER BY parent.id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        outer.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"1".to_vec()),
                Some(b"p".to_vec())
            ],
            vec![None, None, None],
            vec![
                Some(b"2".to_vec()),
                Some(b"3".to_vec()),
                Some(b"q".to_vec())
            ],
        ]
    );
    assert_eq!(outer.columns[0].flags & MYSQL_NOT_NULL_FLAG, 0);

    // An alias renames the source, so the columns report it as their table and
    // keep the real name as the original table.
    let CommandExecutionResult::ResultSet(aliased) = adapter
        .execute_query("SELECT t.* FROM parent AS t ORDER BY t.id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        aliased
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.table.as_str(),
                column.original_table.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("id", "t", "parent"),
            ("n", "t", "parent"),
            ("name", "t", "parent"),
        ]
    );
    assert_eq!(aliased.rows.len(), 3);

    // 1051: once an alias renames the source, the table's own name is gone.
    assert!(adapter
        .execute_query("SELECT parent.* FROM parent AS t")
        .is_err());
}

/// An `UPDATE` or `DELETE` may name its rows through a subquery, which is how
/// a test fixture clears out whatever another table points at. Measured on
/// MySQL 8.4.11 over parents (1,10), (2,20), (3,30), (4,40) and children
/// pointing at 1, 3 and nothing: `SET n = 0 WHERE id IN (SELECT parent_id ...)`
/// changes 2 rows, an `EXISTS` over the same match changes the same 2, and
/// `NOT IN` over a list holding NULL matches nothing at all — 0 rows deleted,
/// which is the SQL reading of a comparison against the unknown. What MySQL
/// refuses is a subquery reading the table being changed: 1093, and refused
/// here too, where the engine would answer it.
#[cfg(unix)]
#[test]
fn a_dml_statement_names_its_rows_through_a_subquery() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([216; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, n INT)",
        "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT)",
        "INSERT INTO parent (id, n) VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
        "INSERT INTO child (id, parent_id) VALUES (1, 1), (2, 3), (3, NULL)",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let rows = |adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>| {
        let CommandExecutionResult::ResultSet(set) = adapter
            .execute_query("SELECT id, n FROM parent ORDER BY id")
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        set.rows
    };

    let CommandExecutionResult::Ok(updated) = adapter
        .execute_query("UPDATE parent SET n = 0 WHERE id IN (SELECT parent_id FROM child)")
        .unwrap()
    else {
        panic!("UPDATE must return an OK");
    };
    assert_eq!(updated.affected_rows, 2);
    assert_eq!(
        rows(&mut adapter),
        vec![
            vec![Some(b"1".to_vec()), Some(b"0".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"20".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"0".to_vec())],
            vec![Some(b"4".to_vec()), Some(b"40".to_vec())],
        ]
    );

    // An EXISTS correlated back to the row being changed finds the same two.
    let CommandExecutionResult::Ok(existed) = adapter
        .execute_query(
            "UPDATE parent SET n = 99 WHERE EXISTS (SELECT 1 FROM child WHERE child.parent_id = parent.id)",
        )
        .unwrap()
    else {
        panic!("UPDATE must return an OK");
    };
    assert_eq!(existed.affected_rows, 2);
    assert_eq!(
        rows(&mut adapter),
        vec![
            vec![Some(b"1".to_vec()), Some(b"99".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"20".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"99".to_vec())],
            vec![Some(b"4".to_vec()), Some(b"40".to_vec())],
        ]
    );

    // The child row pointing at nothing makes every NOT IN test unknown, so no
    // row matches — not the two the list does not hold.
    let CommandExecutionResult::Ok(kept) = adapter
        .execute_query("DELETE FROM parent WHERE id NOT IN (SELECT parent_id FROM child)")
        .unwrap()
    else {
        panic!("DELETE must return an OK");
    };
    assert_eq!(kept.affected_rows, 0);

    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query("DELETE FROM parent WHERE id IN (SELECT parent_id FROM child)")
        .unwrap()
    else {
        panic!("DELETE must return an OK");
    };
    assert_eq!(deleted.affected_rows, 2);
    assert_eq!(
        rows(&mut adapter),
        vec![
            vec![Some(b"2".to_vec()), Some(b"20".to_vec())],
            vec![Some(b"4".to_vec()), Some(b"40".to_vec())],
        ]
    );

    // 1093: the subquery reads the table the statement changes.
    assert!(adapter
        .execute_query("DELETE FROM parent WHERE id IN (SELECT id FROM parent WHERE n > 100)")
        .is_err());
    assert!(adapter
        .execute_query("UPDATE parent SET n = 1 WHERE id IN (SELECT id FROM parent WHERE n > 100)")
        .is_err());
}

/// A subquery in a DML `WHERE` sits beside the statement's own comparisons,
/// and the two are checked against different tables: the subquery's own, and
/// the table being written. The subquery is a table the statement reads, so it
/// is authorized like any other and the internal catalog stays out of reach.
/// The column an `IN (SELECT ...)` compares is held to the same rule a
/// `SELECT` holds it to — both sides the same kind — because MySQL coerces
/// where the engine compares by affinity.
#[cfg(unix)]
#[test]
fn a_dml_subquery_is_checked_beside_the_written_tables_own_columns() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([217; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(20))",
        "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT, tag VARCHAR(20))",
        "INSERT INTO parent (id, n, name) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c')",
        "INSERT INTO child (id, parent_id, tag) VALUES (1, 1, 'p'), (2, 3, 'q')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    // The unqualified `n` names the table being written, which the statement
    // does not read; the subquery's `parent_id` names the table it reads.
    let CommandExecutionResult::Ok(updated) = adapter
        .execute_query(
            "UPDATE parent SET n = 0 WHERE n > 5 AND id IN (SELECT parent_id FROM child)",
        )
        .unwrap()
    else {
        panic!("UPDATE must return an OK");
    };
    assert_eq!(updated.affected_rows, 2);

    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query(
            "DELETE FROM parent WHERE name = 'a' AND id IN (SELECT parent_id FROM child)",
        )
        .unwrap()
    else {
        panic!("DELETE must return an OK");
    };
    assert_eq!(deleted.affected_rows, 1);

    // A whole number against a word column is refused, the same as it is in a
    // SELECT: MySQL coerces the word to a number and the engine does not.
    assert!(adapter
        .execute_query("UPDATE parent SET n = 0 WHERE id IN (SELECT tag FROM child)")
        .is_err());

    // The subquery is a table the statement reads, so the internal catalog is
    // out of reach through it.
    assert!(adapter
        .execute_query("DELETE FROM parent WHERE id IN (SELECT name FROM sqlite_schema)")
        .is_err());
}

/// `IFNULL(SUM(amount), 0)` is how a report asks for a total over rows that
/// may not be there, and `COALESCE(MAX(id), 0)` how it asks for the highest of
/// nothing. Measured on MySQL 8.4.11 over rows (1,5,2,1.50), (2,3,4,2.25),
/// (3,9,6,3.00): each answers the shape its aggregate answers on its own — the
/// NEWDECIMAL of length 33 that `SUM` over an INT answers, the length 16 and
/// scale 4 of `AVG`, the length 34 and scale 2 of `SUM` over a DECIMAL(10,2) —
/// plus NOT_NULL, and with any whole number widened to a BIGINT: `MAX` over a
/// SMALLINT answers LONGLONG while keeping the SMALLINT's length 6. Over no
/// rows at all the answer is the fallback rather than NULL, which is the whole
/// reason it is written.
#[cfg(unix)]
#[test]
fn a_defaulted_aggregate_answers_the_aggregates_shape_and_never_null() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([218; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE ag (id INT NOT NULL PRIMARY KEY, n INT, s SMALLINT, amount DECIMAL(10,2), name VARCHAR(20))",
        "INSERT INTO ag (id, n, s, amount, name) VALUES (1, 5, 2, 1.50, 'a'), (2, 3, 4, 2.25, 'b'), (3, 9, 6, 3.00, 'c')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, value, column_type, column_length, decimals) in [
        (
            "SELECT IFNULL(SUM(n), 0) FROM ag",
            "17",
            MYSQL_TYPE_NEWDECIMAL,
            33,
            0,
        ),
        (
            "SELECT COALESCE(MAX(n), 0) FROM ag",
            "9",
            MYSQL_TYPE_LONGLONG,
            11,
            0,
        ),
        (
            "SELECT IFNULL(COUNT(*), 0) FROM ag",
            "3",
            MYSQL_TYPE_LONGLONG,
            21,
            0,
        ),
        (
            "SELECT IFNULL(AVG(n), 0) FROM ag",
            "5.6667",
            MYSQL_TYPE_NEWDECIMAL,
            16,
            4,
        ),
        (
            "SELECT IFNULL(SUM(amount), 0) FROM ag",
            "6.75",
            MYSQL_TYPE_NEWDECIMAL,
            34,
            2,
        ),
        (
            "SELECT IFNULL(MIN(n), 0) FROM ag",
            "3",
            MYSQL_TYPE_LONGLONG,
            11,
            0,
        ),
        // A SMALLINT widens to a BIGINT and keeps its own length.
        (
            "SELECT IFNULL(MAX(s), 0) FROM ag",
            "6",
            MYSQL_TYPE_LONGLONG,
            6,
            0,
        ),
        // No rows at all, so the fallback is the answer.
        (
            "SELECT IFNULL(SUM(n), 0) FROM ag WHERE id > 100",
            "0",
            MYSQL_TYPE_NEWDECIMAL,
            33,
            0,
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            set.rows,
            vec![vec![Some(value.as_bytes().to_vec())]],
            "{sql}"
        );
        assert_eq!(set.columns[0].column_type, column_type, "{sql}");
        assert_eq!(set.columns[0].column_length, column_length, "{sql}");
        assert_eq!(set.columns[0].decimals, decimals, "{sql}");
        assert_eq!(
            set.columns[0].flags,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            "{sql}"
        );
        assert_eq!(
            set.columns[0].character_set, MYSQL_BINARY_COLLATION,
            "{sql}"
        );
    }

    // The result column is named after the call as written, or after an alias.
    let CommandExecutionResult::ResultSet(named) = adapter
        .execute_query("SELECT IFNULL(SUM(n), 0) FROM ag")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(named.columns[0].name, "IFNULL(SUM(n), 0)");
    let CommandExecutionResult::ResultSet(aliased) = adapter
        .execute_query("SELECT IFNULL(SUM(n), 0) AS total FROM ag")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(aliased.columns[0].name, "total");

    // It groups like any other aggregate.
    let CommandExecutionResult::ResultSet(grouped) = adapter
        .execute_query("SELECT name, IFNULL(SUM(n), 0) FROM ag GROUP BY name ORDER BY name")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        grouped.rows,
        vec![
            vec![Some(b"a".to_vec()), Some(b"5".to_vec())],
            vec![Some(b"b".to_vec()), Some(b"3".to_vec())],
            vec![Some(b"c".to_vec()), Some(b"9".to_vec())],
        ]
    );
}

/// `WHERE 1 = 1 AND ...` is how a statement built up in pieces starts its
/// `WHERE`, so every piece after it can be written with an `AND` in front.
/// Measured on MySQL 8.4.11 over rows 1, 2, 3: `1 = 1` keeps every row, `1 = 0`
/// keeps none, `2 > 1` keeps every row, `1 <> 1` keeps none, and the bare
/// `WHERE 1` and `WHERE 0` read the same way. It holds in an `UPDATE` and a
/// `DELETE` as well. A word against a word and a number against a word stay
/// refused: MySQL compares those without regard to case and coerces the word to
/// a number, neither of which the engine does.
#[cfg(unix)]
#[test]
fn a_comparison_between_whole_numbers_needs_no_column() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([220; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE c (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))",
        "INSERT INTO c (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let every_row = vec![
        vec![Some(b"1".to_vec())],
        vec![Some(b"2".to_vec())],
        vec![Some(b"3".to_vec())],
    ];

    for (sql, expected) in [
        ("SELECT id FROM c WHERE 1 = 1 ORDER BY id", &every_row),
        ("SELECT id FROM c WHERE 2 > 1 ORDER BY id", &every_row),
        ("SELECT id FROM c WHERE 1 ORDER BY id", &every_row),
        ("SELECT id FROM c WHERE 1 = 0 ORDER BY id", &vec![]),
        ("SELECT id FROM c WHERE 1 <> 1 ORDER BY id", &vec![]),
        ("SELECT id FROM c WHERE 0 ORDER BY id", &vec![]),
        (
            "SELECT id FROM c WHERE 1 = 1 AND id > 1 ORDER BY id",
            &vec![vec![Some(b"2".to_vec())], vec![Some(b"3".to_vec())]],
        ),
        (
            "SELECT id FROM c WHERE 1 = 0 OR id > 2 ORDER BY id",
            &vec![vec![Some(b"3".to_vec())]],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(&set.rows, expected, "{sql}");
    }

    // The same opening holds in a statement that writes.
    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query("DELETE FROM c WHERE 1 = 1 AND id = 3")
        .unwrap()
    else {
        panic!("DELETE must return an OK");
    };
    assert_eq!(deleted.affected_rows, 1);
    let CommandExecutionResult::Ok(updated) = adapter
        .execute_query("UPDATE c SET name = 'z' WHERE 1 = 1 AND id = 2")
        .unwrap()
    else {
        panic!("UPDATE must return an OK");
    };
    assert_eq!(updated.affected_rows, 1);
    let CommandExecutionResult::ResultSet(left) = adapter
        .execute_query("SELECT id, name FROM c ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        left.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"a".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"z".to_vec())],
        ]
    );

    // Measured: MySQL answers every row for each of these, by rules the engine
    // does not follow. They keep the refusal an uncalibrated comparison has.
    for sql in [
        "SELECT id FROM c WHERE 'a' = 'a' ORDER BY id",
        "SELECT id FROM c WHERE 'a' = 'A' ORDER BY id",
        "SELECT id FROM c WHERE 1 = '1' ORDER BY id",
        "SELECT id FROM c WHERE NULL = NULL ORDER BY id",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `WHERE n = (SELECT MAX(n) FROM t)` is how a statement asks for the row
/// holding the highest of something, and `WHERE (SELECT COUNT(*) FROM c) > 0`
/// how it asks whether another table holds anything at all. Measured on MySQL
/// 8.4.11 over parents (1,5,'a'), (2,3,'b'), (3,9,'c') and children pointing at
/// 1 and 3: the first answers row 3, the lowest of the children answers row 1,
/// the count answers every row, and a count that finds nothing answers none.
/// A subquery of many rows answers 1242 there where the engine takes the first
/// row it finds, so only an aggregate over one implicit group is taken.
#[cfg(unix)]
#[test]
fn a_comparison_takes_a_subquery_that_answers_one_value() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([221; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(20))",
        "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT)",
        "INSERT INTO parent (id, n, name) VALUES (1, 5, 'a'), (2, 3, 'b'), (3, 9, 'c')",
        "INSERT INTO child (id, parent_id) VALUES (1, 1), (2, 3)",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let every_row = vec![
        vec![Some(b"1".to_vec())],
        vec![Some(b"2".to_vec())],
        vec![Some(b"3".to_vec())],
    ];

    for (sql, expected) in [
        (
            "SELECT id FROM parent WHERE n = (SELECT MAX(n) FROM parent) ORDER BY id",
            &vec![vec![Some(b"3".to_vec())]],
        ),
        (
            "SELECT id FROM parent WHERE id = (SELECT MIN(parent_id) FROM child) ORDER BY id",
            &vec![vec![Some(b"1".to_vec())]],
        ),
        (
            "SELECT id FROM parent WHERE (SELECT COUNT(*) FROM child) > 0 ORDER BY id",
            &every_row,
        ),
        // The same test written the other way round.
        (
            "SELECT id FROM parent WHERE 0 < (SELECT COUNT(*) FROM child) ORDER BY id",
            &every_row,
        ),
        (
            "SELECT id FROM parent WHERE (SELECT COUNT(*) FROM child WHERE parent_id > 100) > 0 ORDER BY id",
            &vec![],
        ),
        // The subquery carries its own WHERE.
        (
            "SELECT id FROM parent WHERE id = (SELECT MAX(id) FROM child WHERE parent_id = 1) ORDER BY id",
            &vec![vec![Some(b"1".to_vec())]],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(&set.rows, expected, "{sql}");
    }

    // It holds in a statement that writes, over a table it does not change.
    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query("DELETE FROM parent WHERE n = (SELECT MAX(parent_id) FROM child)")
        .unwrap()
    else {
        panic!("DELETE must return an OK");
    };
    assert_eq!(deleted.affected_rows, 1);
    let CommandExecutionResult::ResultSet(left) = adapter
        .execute_query("SELECT id, n FROM parent ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        left.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"5".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"9".to_vec())],
        ]
    );

    for sql in [
        // 1242 in MySQL, a row of its own choosing in the engine.
        "SELECT id FROM parent WHERE id = (SELECT parent_id FROM child)",
        // MySQL rounds AVG to four places and the engine keeps the fraction.
        "SELECT id FROM parent WHERE n > (SELECT AVG(n) FROM parent)",
        // A word against a number: MySQL coerces it and warns 1292.
        "SELECT id FROM parent WHERE name = (SELECT MAX(parent_id) FROM child)",
        // 1093: the subquery reads the table the statement changes.
        "DELETE FROM parent WHERE n = (SELECT MAX(n) FROM parent)",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `LIKE CONCAT('%', ?, '%')` is how a search filter wraps the value it binds
/// in wildcards, and `LIKE CONCAT('%', 'lph', '%')` the same pattern written
/// out. Measured on MySQL 8.4.11 over 'alpha', 'beta', 'ALPHABET', 'gamma':
/// both answer rows 1 and 3, which is what `LIKE '%lph%'` answers — the pieces
/// spell one pattern, matched without regard to case. `NOT LIKE` answers the
/// rest, and the same pattern holds in a statement that writes.
#[cfg(unix)]
#[test]
fn a_like_pattern_may_be_written_in_pieces() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([222; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE lc (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))",
        "INSERT INTO lc (id, name) VALUES (1, 'alpha'), (2, 'beta'), (3, 'ALPHABET'), (4, 'gamma')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, expected) in [
        (
            "SELECT id FROM lc WHERE name LIKE CONCAT('%', 'lph', '%') ORDER BY id",
            vec![vec![Some(b"1".to_vec())], vec![Some(b"3".to_vec())]],
        ),
        // The same pattern written out answers the same rows.
        (
            "SELECT id FROM lc WHERE name LIKE '%lph%' ORDER BY id",
            vec![vec![Some(b"1".to_vec())], vec![Some(b"3".to_vec())]],
        ),
        (
            "SELECT id FROM lc WHERE name LIKE CONCAT('al', '%') ORDER BY id",
            vec![vec![Some(b"1".to_vec())], vec![Some(b"3".to_vec())]],
        ),
        (
            "SELECT id FROM lc WHERE name LIKE CONCAT('al', 'pha') ORDER BY id",
            vec![vec![Some(b"1".to_vec())]],
        ),
        (
            "SELECT id FROM lc WHERE name NOT LIKE CONCAT('%', 'lph', '%') ORDER BY id",
            vec![vec![Some(b"2".to_vec())], vec![Some(b"4".to_vec())]],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.rows, expected, "{sql}");
    }

    // The form a client actually writes binds the middle piece.
    let prepared = adapter
        .execute_stmt_prepare("SELECT id FROM lc WHERE name LIKE CONCAT('%', ?, '%') ORDER BY id")
        .unwrap();
    let mut payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0];
    payload.extend_from_slice(&[3, b'l', b'p', b'h']);
    assert_eq!(
        prepared_result_set(
            adapter
                .execute_stmt_execute(prepared.statement_id, &payload)
                .unwrap()
        )
        .rows,
        [
            vec![BinaryResultValue::Integer(1)],
            vec![BinaryResultValue::Integer(3)]
        ]
    );

    let CommandExecutionResult::Ok(deleted) = adapter
        .execute_query("DELETE FROM lc WHERE name LIKE CONCAT('gam', '%')")
        .unwrap()
    else {
        panic!("DELETE must return an OK");
    };
    assert_eq!(deleted.affected_rows, 1);

    for sql in [
        // A piece naming a column makes the pattern a different one per row.
        "SELECT id FROM lc WHERE name LIKE CONCAT('%', name, '%')",
        // MySQL answers no rows for a pattern holding nothing; written this
        // way it is not a pattern at all, so it keeps the refusal.
        "SELECT id FROM lc WHERE name LIKE CONCAT('%', NULL, '%')",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// The calendar readings a report writes: which quarter a day falls in, which
/// day of the week or of the year it is, and the day its month ends on. The
/// engine has none of them by name, so each is counted off what it does have.
/// Measured on MySQL 8.4.11 over 2024-03-05 (a Tuesday), 2024-01-01 (a
/// Monday), 2024-12-31 and 2023-02-28: `WEEKDAY` counts the week from Monday
/// as 0 and `DAYOFWEEK` from Sunday as 1, `DAYOFYEAR` answers 366 in a leap
/// year, and `LAST_DAY` answers February's 28th in an ordinary one.
///
/// `EXTRACT(<field> FROM ...)` reads the same parts the call spellings read
/// and reports the same shapes — but for the year, which answers a LONGLONG of
/// length 5 where `YEAR` answers a YEAR of length 4.
#[cfg(unix)]
#[test]
fn the_calendar_readings_answer_what_mysql_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([223; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE cal (id INT NOT NULL PRIMARY KEY, d DATE, m DATETIME)",
        "INSERT INTO cal (id, d, m) VALUES (1, '2024-03-05', '2024-03-05 10:20:30'), (2, '2024-01-01', '2024-01-01 00:00:00'), (3, '2024-12-31', '2024-12-31 23:59:59'), (4, '2023-02-28', '2023-02-28 12:00:00')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, name, column_type, column_length, answers) in [
        (
            "SELECT QUARTER(d) FROM cal ORDER BY id",
            "QUARTER(d)",
            MYSQL_TYPE_LONGLONG,
            2,
            ["1", "1", "4", "1"],
        ),
        (
            "SELECT WEEKDAY(d) FROM cal ORDER BY id",
            "WEEKDAY(d)",
            MYSQL_TYPE_LONGLONG,
            2,
            ["1", "0", "1", "1"],
        ),
        (
            "SELECT DAYOFWEEK(d) FROM cal ORDER BY id",
            "DAYOFWEEK(d)",
            MYSQL_TYPE_LONGLONG,
            2,
            ["3", "2", "3", "3"],
        ),
        (
            "SELECT DAYOFYEAR(d) FROM cal ORDER BY id",
            "DAYOFYEAR(d)",
            MYSQL_TYPE_LONGLONG,
            4,
            ["65", "1", "366", "59"],
        ),
        (
            "SELECT DAYOFMONTH(d) FROM cal ORDER BY id",
            "DAYOFMONTH(d)",
            MYSQL_TYPE_LONGLONG,
            3,
            ["5", "1", "31", "28"],
        ),
        (
            "SELECT LAST_DAY(d) FROM cal ORDER BY id",
            "LAST_DAY(d)",
            MYSQL_TYPE_DATE,
            10,
            ["2024-03-31", "2024-01-31", "2024-12-31", "2023-02-28"],
        ),
        (
            "SELECT EXTRACT(YEAR FROM d) FROM cal ORDER BY id",
            "EXTRACT(YEAR FROM d)",
            MYSQL_TYPE_LONGLONG,
            5,
            ["2024", "2024", "2024", "2023"],
        ),
        (
            "SELECT EXTRACT(MONTH FROM d) FROM cal ORDER BY id",
            "EXTRACT(MONTH FROM d)",
            MYSQL_TYPE_LONGLONG,
            3,
            ["3", "1", "12", "2"],
        ),
        (
            "SELECT EXTRACT(DAY FROM d) FROM cal ORDER BY id",
            "EXTRACT(DAY FROM d)",
            MYSQL_TYPE_LONGLONG,
            3,
            ["5", "1", "31", "28"],
        ),
        (
            "SELECT EXTRACT(HOUR FROM m) FROM cal ORDER BY id",
            "EXTRACT(HOUR FROM m)",
            MYSQL_TYPE_LONGLONG,
            4,
            ["10", "0", "23", "12"],
        ),
        (
            "SELECT EXTRACT(MINUTE FROM m) FROM cal ORDER BY id",
            "EXTRACT(MINUTE FROM m)",
            MYSQL_TYPE_LONGLONG,
            3,
            ["20", "0", "59", "0"],
        ),
        (
            "SELECT EXTRACT(SECOND FROM m) FROM cal ORDER BY id",
            "EXTRACT(SECOND FROM m)",
            MYSQL_TYPE_LONGLONG,
            3,
            ["30", "0", "59", "0"],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.columns[0].name, name, "{sql}");
        assert_eq!(set.columns[0].column_type, column_type, "{sql}");
        assert_eq!(set.columns[0].column_length, column_length, "{sql}");
        assert_eq!(
            set.rows,
            answers
                .iter()
                .map(|answer| vec![Some(answer.as_bytes().to_vec())])
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }

    // Each reading says what it answers, so a value it meets is held to that.
    for (sql, expected) in [
        (
            "SELECT id FROM cal WHERE QUARTER(d) = 4 ORDER BY id",
            vec![vec![Some(b"3".to_vec())]],
        ),
        (
            "SELECT id FROM cal WHERE EXTRACT(YEAR FROM d) = 2023 ORDER BY id",
            vec![vec![Some(b"4".to_vec())]],
        ),
        (
            "SELECT id FROM cal WHERE LAST_DAY(d) = '2024-03-31' ORDER BY id",
            vec![vec![Some(b"1".to_vec())]],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.rows, expected, "{sql}");
    }
}

/// An index hint says which key to plan with and nothing about which rows come
/// back, so it is dropped and the statement answers what it answers without
/// one. Measured on MySQL 8.4.11 over rows (1,5,'a'), (2,3,'b'), (3,9,'c'):
/// `USE`, `FORCE` and `IGNORE` each answer the same rows, on either side of a
/// join, under an alias, with two keys named at once and with a `FOR ORDER BY`
/// scope. What a hint does say is that the key exists — one naming a key the
/// table has not got answers 1176 there, and is turned away here.
#[cfg(unix)]
#[test]
fn an_index_hint_names_a_key_and_changes_no_rows() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([225; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE h (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(20))",
        "CREATE TABLE hc (id INT NOT NULL PRIMARY KEY, parent_id INT)",
        "INSERT INTO h (id, n, name) VALUES (1, 5, 'a'), (2, 3, 'b'), (3, 9, 'c')",
        "INSERT INTO hc (id, parent_id) VALUES (1, 1), (2, 3)",
        "CREATE INDEX by_name ON h (name)",
        "CREATE INDEX by_parent ON hc (parent_id)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, expected) in [
        (
            "SELECT id FROM h FORCE INDEX (PRIMARY) WHERE id > 1 ORDER BY id",
            vec![vec![Some(b"2".to_vec())], vec![Some(b"3".to_vec())]],
        ),
        (
            "SELECT id FROM h USE INDEX (by_name) WHERE name = 'b' ORDER BY id",
            vec![vec![Some(b"2".to_vec())]],
        ),
        (
            "SELECT id FROM h IGNORE INDEX (by_name) WHERE name = 'b' ORDER BY id",
            vec![vec![Some(b"2".to_vec())]],
        ),
        (
            "SELECT id FROM h USE INDEX (PRIMARY, by_name) WHERE id > 1 ORDER BY id",
            vec![vec![Some(b"2".to_vec())], vec![Some(b"3".to_vec())]],
        ),
        // A hint on each side of a join.
        (
            "SELECT h.id FROM h FORCE INDEX (PRIMARY) JOIN hc FORCE INDEX (by_parent) ON hc.parent_id = h.id ORDER BY h.id",
            vec![vec![Some(b"1".to_vec())], vec![Some(b"3".to_vec())]],
        ),
        // An alias renames the source and the hint still names its keys.
        (
            "SELECT t.id FROM h AS t USE INDEX (by_name) WHERE t.name = 'c' ORDER BY t.id",
            vec![vec![Some(b"3".to_vec())]],
        ),
        // A scope on the hint, and the `KEY` spelling of it.
        (
            "SELECT id FROM h USE INDEX FOR ORDER BY (PRIMARY) ORDER BY id",
            vec![
                vec![Some(b"1".to_vec())],
                vec![Some(b"2".to_vec())],
                vec![Some(b"3".to_vec())],
            ],
        ),
        (
            "SELECT id FROM h USE KEY (PRIMARY) WHERE id > 1 ORDER BY id",
            vec![vec![Some(b"2".to_vec())], vec![Some(b"3".to_vec())]],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.rows, expected, "{sql}");
    }

    // 1176: the table has no such key.
    assert!(adapter
        .execute_query("SELECT id FROM h FORCE INDEX (by_nothing) WHERE id > 1 ORDER BY id")
        .is_err());
    // The key exists, but on the other table.
    assert!(adapter
        .execute_query("SELECT id FROM h USE INDEX (by_parent) ORDER BY id")
        .is_err());
}

/// `TIMESTAMPDIFF(<unit>, a, b)` counts whole units from the first moment to
/// the second, dropping whatever is left over. Measured on MySQL 8.4.11 over
/// four pairs — two and a half days apart, two hours apart across midnight,
/// two days apart backwards, and one second short of a day — and matched: the
/// backward pair answers −2 days rather than −3, and the one a second short
/// answers 0 days and 23 hours. Every unit reports a whole number of length
/// 21, where `DATEDIFF` reports 9.
///
/// Only the units of fixed length are taken. A month, a quarter and a year are
/// counted by the calendar rather than by their length, which is not a rule
/// the engine follows, so those are refused.
#[cfg(unix)]
#[test]
fn timestampdiff_counts_whole_units_between_two_moments() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([226; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE td (id INT NOT NULL PRIMARY KEY, a DATETIME, b DATETIME, d DATE, e DATE)",
        "INSERT INTO td (id, a, b, d, e) VALUES (1, '2024-01-01 00:00:00', '2024-01-03 12:00:00', '2024-01-01', '2024-01-03'), (2, '2024-01-01 23:00:00', '2024-01-02 01:00:00', '2024-01-01', '2024-01-02'), (3, '2024-01-03 00:00:00', '2024-01-01 00:00:00', '2024-01-03', '2024-01-01'), (4, '2024-02-29 12:34:56', '2024-03-01 12:34:55', '2024-02-29', '2024-03-01')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, answers) in [
        (
            "SELECT TIMESTAMPDIFF(SECOND, a, b) FROM td ORDER BY id",
            ["216000", "7200", "-172800", "86399"],
        ),
        (
            "SELECT TIMESTAMPDIFF(MINUTE, a, b) FROM td ORDER BY id",
            ["3600", "120", "-2880", "1439"],
        ),
        (
            "SELECT TIMESTAMPDIFF(HOUR, a, b) FROM td ORDER BY id",
            ["60", "2", "-48", "23"],
        ),
        (
            "SELECT TIMESTAMPDIFF(DAY, a, b) FROM td ORDER BY id",
            ["2", "0", "-2", "0"],
        ),
        (
            "SELECT TIMESTAMPDIFF(WEEK, a, b) FROM td ORDER BY id",
            ["0", "0", "0", "0"],
        ),
        // Two dates carry no time, so a whole day is always whole.
        (
            "SELECT TIMESTAMPDIFF(DAY, d, e) FROM td ORDER BY id",
            ["2", "1", "-2", "1"],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.columns[0].column_type, MYSQL_TYPE_LONGLONG, "{sql}");
        assert_eq!(set.columns[0].column_length, 21, "{sql}");
        assert_eq!(
            set.rows,
            answers
                .iter()
                .map(|answer| vec![Some(answer.as_bytes().to_vec())])
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }

    // The count says it answers a whole number, so a value it meets is held to
    // that rather than to a column's declared type.
    let CommandExecutionResult::ResultSet(compared) = adapter
        .execute_query("SELECT id FROM td WHERE TIMESTAMPDIFF(DAY, a, b) = 2 ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(compared.rows, vec![vec![Some(b"1".to_vec())]]);

    for sql in [
        "SELECT TIMESTAMPDIFF(MONTH, a, b) FROM td",
        "SELECT TIMESTAMPDIFF(QUARTER, a, b) FROM td",
        "SELECT TIMESTAMPDIFF(YEAR, a, b) FROM td",
        "SELECT TIMESTAMPDIFF(MICROSECOND, a, b) FROM td",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `DATE_FORMAT(NOW(), '%Y-%m-%d')` is how a statement asks for today written
/// out, and `STR_TO_DATE('2024-03-05', '%Y-%m-%d')` how it reads a day out of
/// a word it wrote itself. Neither reads a column, and neither needs to:
/// measured on MySQL 8.4.11, both report exactly what the same call over a
/// column reports — the shape comes from the format. `DATE_FORMAT` answers a
/// VAR_STRING as wide as the format could make it with no flags at all, and
/// `STR_TO_DATE` the type its format names, each with the binary collation.
#[cfg(unix)]
#[test]
fn a_moment_written_out_or_read_back_need_not_come_from_a_column() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([227; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE mf (id INT NOT NULL PRIMARY KEY, m DATETIME, w VARCHAR(30))",
        "INSERT INTO mf (id, m, w) VALUES (1, '2024-03-05 10:20:30', '2024-03-05')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, answer, column_type, column_length) in [
        (
            "SELECT DATE_FORMAT(m, '%Y-%m-%d') FROM mf",
            "2024-03-05",
            MYSQL_TYPE_VAR_STRING,
            40,
        ),
        (
            "SELECT DATE_FORMAT('2024-03-05', '%Y-%m-%d')",
            "2024-03-05",
            MYSQL_TYPE_VAR_STRING,
            40,
        ),
        (
            "SELECT STR_TO_DATE(w, '%Y-%m-%d') FROM mf",
            "2024-03-05",
            MYSQL_TYPE_DATE,
            10,
        ),
        (
            "SELECT STR_TO_DATE('2024-03-05', '%Y-%m-%d')",
            "2024-03-05",
            MYSQL_TYPE_DATE,
            10,
        ),
        (
            "SELECT STR_TO_DATE('10:20:30', '%H:%i:%s')",
            "10:20:30",
            MYSQL_TYPE_TIME,
            10,
        ),
        (
            "SELECT STR_TO_DATE('2024-03-05 10:20:30', '%Y-%m-%d %H:%i:%s')",
            "2024-03-05 10:20:30",
            MYSQL_TYPE_DATETIME,
            19,
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            set.rows,
            vec![vec![Some(answer.as_bytes().to_vec())]],
            "{sql}"
        );
        assert_eq!(set.columns[0].column_type, column_type, "{sql}");
        assert_eq!(set.columns[0].column_length, column_length, "{sql}");
    }

    // A clock reading carries the current moment, so only its shape is
    // asserted here — the answer is whatever the clock says.
    for sql in [
        "SELECT DATE_FORMAT(NOW(), '%Y-%m-%d')",
        "SELECT DATE_FORMAT(CURDATE(), '%Y-%m-%d')",
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.columns[0].column_type, MYSQL_TYPE_VAR_STRING, "{sql}");
        assert_eq!(set.columns[0].column_length, 40, "{sql}");
        assert_eq!(set.columns[0].flags, 0, "{sql}");
        assert_eq!(set.rows.len(), 1, "{sql}");
        let [Some(written)] = set.rows[0].as_slice() else {
            panic!("one written moment: {sql}");
        };
        assert_eq!(written.len(), "2024-03-05".len(), "{sql}");
    }

    for sql in [
        // A clock reading is a moment, not the text a STR_TO_DATE reads.
        "SELECT STR_TO_DATE(NOW(), '%Y-%m-%d')",
        // `CURTIME()` holds no day, and what MySQL writes for one has not
        // been measured.
        "SELECT DATE_FORMAT(CURTIME(), '%Y-%m-%d')",
        // A call inside the call is not one of the three shapes.
        "SELECT DATE_FORMAT(DATE(m), '%Y-%m-%d') FROM mf",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Once a statement aggregates and groups nothing, every row it read has gone
/// into one answer, so a bare column has no single row to come from. Measured
/// on MySQL 8.4.11: `SELECT id, SUM(n) FROM t` answers 1140, and so do the
/// same column after the total, a column inside arithmetic over the total, a
/// column inside a call beside a count, and a column beside a total that
/// carries a fallback. A literal is fine, having no row to come from either.
///
/// A window does not aggregate the statement, nor does a subquery whose
/// aggregate belongs to the statement inside it, and a `GROUP BY` gives every
/// column a group to come from — all three keep answering a row per row.
#[cfg(unix)]
#[test]
fn an_aggregated_projection_may_name_no_ungrouped_column() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([228; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE ag (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(20))",
        "INSERT INTO ag (id, n, name) VALUES (1, 5, 'a'), (2, 3, 'b'), (3, 9, 'c')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    // 1140: a column anywhere in the projection of an aggregated statement.
    for sql in [
        "SELECT id, SUM(n) FROM ag",
        "SELECT id, COUNT(*) FROM ag",
        "SELECT SUM(n), id FROM ag",
        "SELECT id + SUM(n) FROM ag",
        "SELECT UPPER(name), COUNT(*) FROM ag",
        "SELECT IFNULL(SUM(n), 0), id FROM ag",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }

    // A literal has no row to come from either, so it crosses.
    for (sql, expected) in [
        (
            "SELECT SUM(n), 1 FROM ag",
            vec![vec![Some(b"17".to_vec()), Some(b"1".to_vec())]],
        ),
        (
            "SELECT SUM(n), 'x' FROM ag",
            vec![vec![Some(b"17".to_vec()), Some(b"x".to_vec())]],
        ),
        // The aggregate alone, with the whole projection aggregated.
        (
            "SELECT IFNULL(SUM(n), 0) FROM ag",
            vec![vec![Some(b"17".to_vec())]],
        ),
        // An ORDER BY over a column is not a projection, and MySQL takes it.
        (
            "SELECT SUM(n) FROM ag ORDER BY id",
            vec![vec![Some(b"17".to_vec())]],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.rows, expected, "{sql}");
    }

    // A window answers a row per row, so the statement is not aggregated.
    let CommandExecutionResult::ResultSet(windowed) = adapter
        .execute_query("SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM ag ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(windowed.rows.len(), 3);

    // A subquery's aggregate belongs to the statement inside it.
    let CommandExecutionResult::ResultSet(counted) = adapter
        .execute_query("SELECT id, (SELECT COUNT(*) FROM ag) FROM ag ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(counted.rows.len(), 3);

    // A GROUP BY gives every column a group to come from.
    let CommandExecutionResult::ResultSet(grouped) = adapter
        .execute_query("SELECT id, SUM(n) FROM ag GROUP BY id ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(grouped.rows.len(), 3);
}

/// `SELECT SUM(amount) * 2` is how a report adjusts a total, and the aggregate
/// stands there where a column stands. Measured on MySQL 8.4.11 over rows
/// (5, 2, 1.50), (3, 4, 2.25), (9, 6, 3.00) and matched throughout.
///
/// Three rules cover it. Adding and subtracting keep the widest whole part and
/// the widest scale and add a digit; multiplying adds both precisions and both
/// scales; dividing widens the left side by four digits and four places.
/// Whether the answer is a decimal is not the same question as whether it
/// carries places: `SUM(n)` over an `INT` answers a NEWDECIMAL with none, so
/// `SUM(n) + 1` answers one too, where `COUNT(*) + 1` and `MAX(n) + 1` each
/// answer a whole number.
#[cfg(unix)]
#[test]
fn arithmetic_takes_an_aggregate_where_it_takes_a_column() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([229; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE ar (id INT NOT NULL PRIMARY KEY, n INT, m INT, amount DECIMAL(10,2))",
        "INSERT INTO ar (id, n, m, amount) VALUES (1, 5, 2, 1.50), (2, 3, 4, 2.25), (3, 9, 6, 3.00)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, answer, column_type, column_length, decimals) in [
        // A SUM answers a decimal whatever it was given, so the sum of an INT
        // adjusted by a digit is a decimal with no places.
        (
            "SELECT SUM(n) + 1 FROM ar",
            "18",
            MYSQL_TYPE_NEWDECIMAL,
            34,
            0,
        ),
        (
            "SELECT SUM(n) - 1 FROM ar",
            "16",
            MYSQL_TYPE_NEWDECIMAL,
            34,
            0,
        ),
        (
            "SELECT SUM(n) * 2 FROM ar",
            "34",
            MYSQL_TYPE_NEWDECIMAL,
            34,
            0,
        ),
        (
            "SELECT SUM(n) + SUM(m) FROM ar",
            "29",
            MYSQL_TYPE_NEWDECIMAL,
            34,
            0,
        ),
        // A COUNT and a MIN or MAX over a whole number stay whole numbers.
        (
            "SELECT COUNT(*) + 1 FROM ar",
            "4",
            MYSQL_TYPE_LONGLONG,
            22,
            0,
        ),
        (
            "SELECT COUNT(*) * 2 FROM ar",
            "6",
            MYSQL_TYPE_LONGLONG,
            22,
            0,
        ),
        (
            "SELECT MAX(n) + 1 FROM ar",
            "10",
            MYSQL_TYPE_LONGLONG,
            12,
            0,
        ),
        ("SELECT MIN(n) * 3 FROM ar", "9", MYSQL_TYPE_LONGLONG, 12, 0),
        // An AVG carries four places of its own, and they survive.
        (
            "SELECT AVG(n) + 1 FROM ar",
            "6.6667",
            MYSQL_TYPE_NEWDECIMAL,
            17,
            4,
        ),
        (
            "SELECT AVG(n) * 2 FROM ar",
            "11.3333",
            MYSQL_TYPE_NEWDECIMAL,
            17,
            4,
        ),
        (
            "SELECT AVG(n) / 2 FROM ar",
            "2.83333333",
            MYSQL_TYPE_NEWDECIMAL,
            20,
            8,
        ),
        // A total over a decimal column keeps the column's places.
        (
            "SELECT SUM(amount) + 1 FROM ar",
            "7.75",
            MYSQL_TYPE_NEWDECIMAL,
            35,
            2,
        ),
        (
            "SELECT SUM(amount) * 2 FROM ar",
            "13.50",
            MYSQL_TYPE_NEWDECIMAL,
            35,
            2,
        ),
        (
            "SELECT SUM(n) + SUM(amount) FROM ar",
            "23.75",
            MYSQL_TYPE_NEWDECIMAL,
            37,
            2,
        ),
        // A division widens by four places whichever side it was given.
        (
            "SELECT SUM(n) / 2 FROM ar",
            "8.5000",
            MYSQL_TYPE_NEWDECIMAL,
            38,
            4,
        ),
        (
            "SELECT SUM(amount) / 2 FROM ar",
            "3.375000",
            MYSQL_TYPE_NEWDECIMAL,
            38,
            6,
        ),
        (
            "SELECT MAX(n) / 2 FROM ar",
            "4.5000",
            MYSQL_TYPE_NEWDECIMAL,
            16,
            4,
        ),
        // The same three rules over a decimal column with no aggregate at all.
        (
            "SELECT amount + 1 FROM ar WHERE id = 1",
            "2.50",
            MYSQL_TYPE_NEWDECIMAL,
            13,
            2,
        ),
        (
            "SELECT amount * 2 FROM ar WHERE id = 1",
            "3.00",
            MYSQL_TYPE_NEWDECIMAL,
            13,
            2,
        ),
        (
            "SELECT n + amount FROM ar WHERE id = 1",
            "6.50",
            MYSQL_TYPE_NEWDECIMAL,
            15,
            2,
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            set.rows,
            vec![vec![Some(answer.as_bytes().to_vec())]],
            "{sql}"
        );
        assert_eq!(set.columns[0].column_type, column_type, "{sql}");
        assert_eq!(set.columns[0].column_length, column_length, "{sql}");
        assert_eq!(set.columns[0].decimals, decimals, "{sql}");
    }

    // A COUNT cannot be null and neither can a digit, so their sum reports
    // NOT NULL; an aggregate over a column can be, and does not.
    let CommandExecutionResult::ResultSet(counted) = adapter
        .execute_query("SELECT COUNT(*) + 1 FROM ar")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        counted.columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
    let CommandExecutionResult::ResultSet(totalled) =
        adapter.execute_query("SELECT SUM(n) + 1 FROM ar").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        totalled.columns[0].flags,
        MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
}

/// `name REGEXP 'a.c'` asks whether a pattern matches anywhere in a column.
/// Measured on MySQL 8.4.11 over 'Alpha', 'beta', 'cafe', 'a1b' and nothing,
/// and matched: anchors, a character class, a repeat, two choices, any
/// character, the negated form and the `RLIKE` spelling all answer the same
/// rows.
///
/// The match ignores case and does not ignore accents — measured, `'Alpha'
/// REGEXP 'alpha'` answers 1 while `'café' REGEXP 'cafe'` answers 0, where the
/// same collation ignores both when comparing. So the case-folding flag goes
/// in front of the pattern and nothing else does.
///
/// Refused: a pattern looking ahead or naming a group again, which MySQL reads
/// and the engine's matching does not; a pattern that does not close, which
/// MySQL answers 3696 for; and a pattern over a number, which MySQL matches by
/// coercing it to text.
#[cfg(unix)]
#[test]
fn a_regexp_matches_a_pattern_the_way_mysql_matches_one() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([230; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE rx (id INT NOT NULL PRIMARY KEY, name VARCHAR(30), n INT)",
        "INSERT INTO rx (id, name, n) VALUES (1, 'Alpha', 5), (2, 'beta', 3), (3, 'cafe', 9), (4, 'a1b', 2), (5, NULL, 7)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, expected) in [
        (
            "SELECT id FROM rx WHERE name REGEXP 'alpha' ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM rx WHERE name RLIKE 'ALPHA' ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM rx WHERE name REGEXP '^a' ORDER BY id",
            vec!["1", "4"],
        ),
        (
            "SELECT id FROM rx WHERE name REGEXP 'a$' ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM rx WHERE name REGEXP '[[:digit:]]' ORDER BY id",
            vec!["4"],
        ),
        (
            "SELECT id FROM rx WHERE name REGEXP 'a{1,2}' ORDER BY id",
            vec!["1", "2", "3", "4"],
        ),
        (
            "SELECT id FROM rx WHERE name REGEXP 'alpha|beta' ORDER BY id",
            vec!["1", "2"],
        ),
        // Every name holds an `a`, and the one holding nothing answers
        // nothing, so a negated match finds no row at all.
        (
            "SELECT id FROM rx WHERE name NOT REGEXP 'a' ORDER BY id",
            vec![],
        ),
        (
            "SELECT id FROM rx WHERE name REGEXP 'caf.' ORDER BY id",
            vec!["3"],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            set.rows,
            expected
                .iter()
                .map(|id| vec![Some(id.as_bytes().to_vec())])
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }

    for sql in [
        // MySQL reads these and the engine's matching does not.
        "SELECT id FROM rx WHERE name REGEXP 'a(?=1)' ORDER BY id",
        "SELECT id FROM rx WHERE name REGEXP '(a)\\\\1' ORDER BY id",
        // 3696 there: the bracket never closes.
        "SELECT id FROM rx WHERE name REGEXP '[' ORDER BY id",
        // MySQL matches a number by coercing it to text.
        "SELECT id FROM rx WHERE n REGEXP '5' ORDER BY id",
        // A bound pattern carries nothing until it binds.
        "SELECT id FROM rx WHERE name REGEXP ? ORDER BY id",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// The readings that turn an angle round, take a square root, and name the
/// circle. Measured on MySQL 8.4.11 and matched: `PI()` answers 3.141593, six
/// places rather than the whole of the number, as its reported six decimals
/// say, and reports NOT NULL where every other reading here does not.
/// `DEGREES`, `RADIANS` and `SQRT` answer the same digits over 0.1, 7 and
/// 123.456.
///
/// The readings a maths library rounds for itself are refused. Measured
/// against the engine, `ATAN(10)` answers 1.4711276743037347 in MySQL and
/// 1.4711276743037345 here, and `TAN(10)` 0.6483608274590866 against
/// 0.6483608274590867 — a last-place difference between two libraries. The
/// rest of that family comes from the same library, so agreeing at the points
/// tried would not be a promise.
#[cfg(unix)]
#[test]
fn the_math_readings_the_two_work_out_alike() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([231; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE mt (id INT NOT NULL PRIMARY KEY, x DOUBLE)",
        "INSERT INTO mt (id, x) VALUES (1, 0.1), (2, 7), (3, 123.456)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    let CommandExecutionResult::ResultSet(circle) = adapter.execute_query("SELECT PI()").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(circle.rows, vec![vec![Some(b"3.141593".to_vec())]]);
    assert_eq!(circle.columns[0].column_type, MYSQL_TYPE_DOUBLE);
    assert_eq!(circle.columns[0].column_length, 8);
    assert_eq!(circle.columns[0].decimals, 6);
    assert_eq!(
        circle.columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    for (sql, answers) in [
        (
            "SELECT DEGREES(x) FROM mt ORDER BY id",
            [
                "5.729577951308233",
                "401.07045659157626",
                "7073.507755567091",
            ],
        ),
        (
            "SELECT RADIANS(x) FROM mt ORDER BY id",
            [
                "0.0017453292519943296",
                "0.12217304763960307",
                "2.1547136813421197",
            ],
        ),
        (
            "SELECT SQRT(x) FROM mt ORDER BY id",
            [
                "0.31622776601683794",
                "2.6457513110645907",
                "11.111075555498667",
            ],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.columns[0].column_type, MYSQL_TYPE_DOUBLE, "{sql}");
        assert_eq!(set.columns[0].column_length, 23, "{sql}");
        assert_eq!(
            set.rows,
            answers
                .iter()
                .map(|answer| vec![Some(answer.as_bytes().to_vec())])
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }

    // The readings a maths library rounds for itself.
    for sql in [
        "SELECT SIN(x) FROM mt",
        "SELECT COS(x) FROM mt",
        "SELECT TAN(x) FROM mt",
        "SELECT ASIN(x) FROM mt",
        "SELECT ACOS(x) FROM mt",
        "SELECT ATAN(x) FROM mt",
        "SELECT EXP(x) FROM mt",
        "SELECT LN(x) FROM mt",
        "SELECT LOG(x) FROM mt",
        "SELECT LOG2(x) FROM mt",
        "SELECT LOG10(x) FROM mt",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `ORDER BY name COLLATE utf8mb4_bin` asks for byte order where the statement
/// would otherwise get the collation's own. Measured on MySQL 8.4.11 over
/// 'beta', 'Alpha', 'alpha', 'Beta', 'Zulu' and 'apple': naming no collation
/// orders them without regard to case, `utf8mb4_bin` puts every capital first,
/// and `utf8mb4_0900_ai_ci` and `utf8mb4_general_ci` each order them the way
/// naming none does. Backwards is the same order reversed, and a collation
/// over a column of numbers changes nothing.
#[cfg(unix)]
#[test]
fn an_ordering_takes_the_collation_it_names() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([232; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE co (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), n INT)",
        "INSERT INTO co (id, name, n) VALUES (1, 'beta', 1), (2, 'Alpha', 2), (3, 'alpha', 3), (4, 'Beta', 4), (5, 'Zulu', 5), (6, 'apple', 6)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, order) in [
        (
            "SELECT id FROM co ORDER BY name, id",
            ["2", "3", "6", "1", "4", "5"],
        ),
        (
            "SELECT id FROM co ORDER BY name COLLATE utf8mb4_bin, id",
            ["2", "4", "5", "3", "6", "1"],
        ),
        (
            "SELECT id FROM co ORDER BY name COLLATE utf8mb4_0900_ai_ci, id",
            ["2", "3", "6", "1", "4", "5"],
        ),
        (
            "SELECT id FROM co ORDER BY name COLLATE utf8mb4_general_ci, id",
            ["2", "3", "6", "1", "4", "5"],
        ),
        (
            "SELECT id FROM co ORDER BY name COLLATE utf8mb4_bin DESC, id",
            ["1", "6", "3", "5", "4", "2"],
        ),
        // A column of numbers has no collation to order by.
        (
            "SELECT id FROM co ORDER BY n COLLATE utf8mb4_bin, id",
            ["1", "2", "3", "4", "5", "6"],
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            set.rows,
            order
                .iter()
                .map(|id| vec![Some(id.as_bytes().to_vec())])
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }

    for sql in [
        // 1253 in MySQL: the collation belongs to another character set.
        "SELECT id FROM co ORDER BY name COLLATE latin1_swedish_ci, id",
        // A collation over something that is not a column has not been measured.
        "SELECT id FROM co ORDER BY LOWER(name) COLLATE utf8mb4_bin, id",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `COUNT(*) OVER ()` is how a paged query asks for the count of the whole
/// result beside each row. Measured on MySQL 8.4.11 over three rows and
/// matched: the count answers 3 on all three, and `SUM`, `MAX`, `MIN` and
/// `AVG` each answer the whole set's value the same way, in the shapes their
/// windowed forms already report. A `PARTITION BY` still narrows the window.
///
/// A ranking over an empty window keeps its refusal: `ROW_NUMBER() OVER ()`
/// numbers the rows in whatever order they were read, and the two need not
/// read them alike.
#[cfg(unix)]
#[test]
fn a_window_over_the_whole_set_answers_it_beside_every_row() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([233; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE wf (id INT NOT NULL PRIMARY KEY, n INT, team VARCHAR(10))",
        "INSERT INTO wf (id, n, team) VALUES (1, 5, 'a'), (2, 3, 'a'), (3, 9, 'b')",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, answer, column_type, column_length, decimals) in [
        (
            "SELECT id, COUNT(*) OVER () FROM wf ORDER BY id",
            "3",
            MYSQL_TYPE_LONGLONG,
            21,
            0,
        ),
        (
            "SELECT id, SUM(n) OVER () FROM wf ORDER BY id",
            "17",
            MYSQL_TYPE_NEWDECIMAL,
            33,
            0,
        ),
        (
            "SELECT id, MAX(n) OVER () FROM wf ORDER BY id",
            "9",
            MYSQL_TYPE_LONGLONG,
            11,
            0,
        ),
        (
            "SELECT id, MIN(n) OVER () FROM wf ORDER BY id",
            "3",
            MYSQL_TYPE_LONGLONG,
            11,
            0,
        ),
        (
            "SELECT id, AVG(n) OVER () FROM wf ORDER BY id",
            "5.6667",
            MYSQL_TYPE_NEWDECIMAL,
            16,
            4,
        ),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(set.rows.len(), 3, "{sql}");
        for row in &set.rows {
            assert_eq!(row[1], Some(answer.as_bytes().to_vec()), "{sql}");
        }
        assert_eq!(set.columns[1].column_type, column_type, "{sql}");
        assert_eq!(set.columns[1].column_length, column_length, "{sql}");
        assert_eq!(set.columns[1].decimals, decimals, "{sql}");
    }

    // A partition still narrows the window to the rows sharing its value.
    let CommandExecutionResult::ResultSet(parted) = adapter
        .execute_query("SELECT id, COUNT(*) OVER (PARTITION BY team) FROM wf ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        parted.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"2".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"2".to_vec())],
            vec![Some(b"3".to_vec()), Some(b"1".to_vec())],
        ]
    );

    // A ranking over an empty window has no order to rank by.
    for sql in [
        "SELECT id, ROW_NUMBER() OVER () FROM wf",
        "SELECT id, RANK() OVER () FROM wf",
        "SELECT id, NTILE(2) OVER () FROM wf",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Arithmetic touching a float answers a float. Measured on MySQL 8.4.11 over
/// a `DOUBLE` holding 1.5 and matched: `d + 1`, `d - 1`, `d * 2`, `d / 2`,
/// `d + e`, `d + n` over an `INT`, `d + amount` over a `DECIMAL(10,2)` and
/// `n + d` all answer a DOUBLE of length 23 with 31 decimals, whichever side
/// the float was on and whichever operator it was. A float swallows the
/// precision rules rather than taking part in them, and so does an aggregate
/// over one.
#[cfg(unix)]
#[test]
fn arithmetic_touching_a_float_answers_a_float() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([234; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE db (id INT NOT NULL PRIMARY KEY, d DOUBLE, e DOUBLE, n INT, amount DECIMAL(10,2))",
        "INSERT INTO db (id, d, e, n, amount) VALUES (1, 1.5, 0.25, 3, 2.50), (2, 10.0, 4.0, 7, 1.25)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    for (sql, answer) in [
        ("SELECT d + 1 FROM db WHERE id = 1", "2.5"),
        ("SELECT d - 1 FROM db WHERE id = 1", "0.5"),
        ("SELECT d * 2 FROM db WHERE id = 1", "3"),
        ("SELECT d / 2 FROM db WHERE id = 1", "0.75"),
        ("SELECT d + e FROM db WHERE id = 1", "1.75"),
        ("SELECT d + n FROM db WHERE id = 1", "4.5"),
        ("SELECT d + amount FROM db WHERE id = 1", "4"),
        ("SELECT n + d FROM db WHERE id = 1", "4.5"),
        // An aggregate over a float answers a float, and arithmetic over it
        // stays one.
        ("SELECT SUM(d) FROM db WHERE id = 1", "1.5"),
        ("SELECT SUM(d) + 1 FROM db WHERE id = 1", "2.5"),
        ("SELECT MAX(d) * 2 FROM db WHERE id = 1", "3"),
    ] {
        let CommandExecutionResult::ResultSet(set) = adapter.execute_query(sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            set.rows,
            vec![vec![Some(answer.as_bytes().to_vec())]],
            "{sql}"
        );
        assert_eq!(set.columns[0].column_type, MYSQL_TYPE_DOUBLE, "{sql}");
        assert_eq!(set.columns[0].column_length, 23, "{sql}");
        assert_eq!(set.columns[0].decimals, NOT_FIXED_DECIMALS, "{sql}");
        assert_eq!(
            set.columns[0].flags,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            "{sql}"
        );
    }
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
    assert_eq!(analyzed.columns[0].column_length, 512);
    assert_eq!(analyzed.columns[3].column_type, MYSQL_TYPE_MEDIUM_BLOB);
    assert_eq!(analyzed.columns[3].column_length, 1_572_864);
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

    // Written without a column list, the statement means every column of the
    // table in order, which is what MySQL makes it. The read table is still
    // checked the same way.
    adapter
        .execute_query("INSERT INTO dst SELECT id, n FROM src")
        .unwrap();
    assert!(adapter
        .execute_query("INSERT INTO dst SELECT id, n FROM sqlite_schema")
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

    // Measured on MySQL 8.4.11: the pattern names the tables to report, the
    // qualifier is spelled `FROM` or `IN`, and a pattern nothing matches
    // answers no rows rather than an error.
    adapter
        .execute_query("CREATE TABLE other (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    for sql in [
        "SHOW TABLE STATUS LIKE 't'",
        "SHOW TABLE STATUS FROM REPORTS LIKE 't'",
        "SHOW TABLE STATUS IN REPORTS LIKE 't'",
    ] {
        let CommandExecutionResult::ResultSet(filtered) = adapter.execute_query(sql).unwrap()
        else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            filtered
                .rows
                .iter()
                .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            ["t"],
            "{sql}"
        );
    }
    let CommandExecutionResult::ResultSet(unmatched) = adapter
        .execute_query("SHOW TABLE STATUS LIKE 'zzz'")
        .unwrap()
    else {
        panic!("SHOW TABLE STATUS must return a result set");
    };
    assert!(unmatched.rows.is_empty());
    assert_eq!(unmatched.columns.len(), 18);

    // Another database's tables are not this session's to describe, and the
    // `WHERE` filter is not read.
    assert!(adapter
        .execute_query("SHOW TABLE STATUS FROM archive")
        .is_err());
    assert!(adapter
        .execute_query("SHOW TABLE STATUS WHERE Name = 't'")
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

/// MySQL does not always refuse a value wider than its column. Measured on
/// 8.4.11: an overflow made only of trailing spaces is cut back to the declared
/// width and reported as note 1265, and a `CHAR` gives back no trailing space
/// at all, whatever it was written with.
#[cfg(unix)]
#[test]
fn a_text_column_cuts_the_trailing_space_mysql_cuts() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([104; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v VARCHAR(4), c CHAR(4))")
        .unwrap();

    for (id, column, written, stored) in [
        (1, "v", "abcd  ", "abcd"),
        (2, "v", "     ", "    "),
        (3, "v", "abc ", "abc "),
        (4, "v", "abcd", "abcd"),
        (5, "c", "ab  ", "ab"),
        (6, "c", "abcd ", "abcd"),
        (7, "c", "    ", ""),
    ] {
        adapter
            .execute_query(&format!(
                "INSERT INTO t (id, {column}) VALUES ({id}, '{written}')"
            ))
            .unwrap_or_else(|_| panic!("{written} must be stored"));
        let CommandExecutionResult::ResultSet(read_back) = adapter
            .execute_query(&format!("SELECT {column} FROM t WHERE id = {id}"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read_back.rows[0][0].clone().unwrap()).unwrap(),
            stored,
            "{written}"
        );
    }

    // An overflow with anything but spaces past the width is still refused.
    for (column, written) in [("v", "abcd e"), ("v", "abcde"), ("c", "abcde")] {
        assert_eq!(
            adapter.execute_query(&format!(
                "INSERT INTO t (id, {column}) VALUES (9, '{written}')"
            )),
            Err(FrontendErrorKind::DataTooLong),
            "{written}"
        );
    }
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

    // Measured: a value that reads as a number is stored as it stands, where
    // the engine's own affinity rules would have made `'007'` the number 7,
    // and a trailing space is not the one a VARCHAR would have cut.
    for (id, written) in [(2, "007"), (3, "1e15"), (4, "42")] {
        adapter
            .execute_query(&format!("INSERT INTO b (id, v) VALUES ({id}, '{written}')"))
            .unwrap();
        let CommandExecutionResult::ResultSet(read_back) = adapter
            .execute_query(&format!("SELECT v FROM b WHERE id = {id}"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(read_back.rows[0][0], Some(written.as_bytes().to_vec()));
    }

    // The declared count is bytes, so a longer value is refused.
    adapter
        .execute_query("CREATE TABLE n (id INT NOT NULL PRIMARY KEY, v VARBINARY(2))")
        .unwrap();
    for written in ["abc", "ab "] {
        assert!(adapter
            .execute_query(&format!("INSERT INTO n (id, v) VALUES (1, '{written}')"))
            .is_err());
    }

    // BINARY(n) pads, and the engine does not.
    assert!(adapter
        .execute_query("CREATE TABLE f (id INT NOT NULL PRIMARY KEY, v BINARY(16))")
        .is_err());
}

/// A foreign key is enforced, which is what makes taking the syntax honest.
/// Measured on MySQL 8.4.11: a child row naming a parent that is not there
/// answers 1452, and the constraint prints as `<table>_ibfk_<n>`.
#[cfg(unix)]
#[test]
fn a_foreign_key_is_enforced_the_way_mysql_enforces_one() {
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
    adapter
        .execute_query(concat!(
            "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT, ",
            "FOREIGN KEY (parent_id) REFERENCES parent (id))"
        ))
        .unwrap();
    adapter
        .execute_query("INSERT INTO parent (id) VALUES (1)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO child (id, parent_id) VALUES (1, 1)")
        .unwrap();

    // A child naming a parent that is not there is refused, and a parent still
    // named by a child cannot be removed.
    assert_eq!(
        adapter.execute_query("INSERT INTO child (id, parent_id) VALUES (2, 999)"),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );
    assert_eq!(
        adapter.execute_query("DELETE FROM parent WHERE id = 1"),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );

    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE child").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `child` (\n",
            "  `id` int NOT NULL,\n",
            "  `parent_id` int DEFAULT NULL,\n",
            "  PRIMARY KEY (`id`),\n",
            "  CONSTRAINT `child_ibfk_1` FOREIGN KEY (`parent_id`) REFERENCES `parent` (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // A named constraint keeps its name, which is what SHOW CREATE TABLE
    // prints back and what a later DROP FOREIGN KEY names.
    adapter
        .execute_query(concat!(
            "CREATE TABLE named (id INT NOT NULL PRIMARY KEY, parent_id INT, ",
            "CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parent (id))"
        ))
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE named").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert!(String::from_utf8(created.rows[0][1].clone().unwrap())
        .unwrap()
        .contains("CONSTRAINT `fk_parent` FOREIGN KEY (`parent_id`) REFERENCES `parent` (`id`)"));
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

/// `ALTER TABLE` runs against a table with an ordinary `PRIMARY KEY`, which is
/// what nearly every table a test suite migrates has.
///
/// The durable DDL such a table is remembered by carries the key inline, and
/// rewriting it was refused because the renderer that writes a table back could
/// not write a `PRIMARY KEY`. It can now, so the ordinary operations run and
/// the key survives them.
#[cfg(unix)]
#[test]
fn alter_table_runs_against_a_table_with_a_primary_key() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([117; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE k (id INT NOT NULL PRIMARY KEY, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO k (id, name) VALUES (1, 'ann')")
        .unwrap();

    for statement in [
        "ALTER TABLE k ADD COLUMN note TEXT",
        "ALTER TABLE k MODIFY COLUMN name VARCHAR(20)",
        "ALTER TABLE k RENAME COLUMN note TO memo",
        "ALTER TABLE k DROP COLUMN memo",
    ] {
        adapter
            .execute_query(statement)
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
    }

    // The key is still there, printed the way MySQL prints it.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE k").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    let printed = String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap();
    assert!(printed.contains("`id` int NOT NULL"), "{printed}");
    assert!(printed.contains("PRIMARY KEY (`id`)"), "{printed}");
    assert!(printed.contains("`name` varchar(20)"), "{printed}");

    // It is still a key: a second row under the same id is refused.
    assert!(adapter
        .execute_query("INSERT INTO k (id, name) VALUES (1, 'bo')")
        .is_err());
    let CommandExecutionResult::ResultSet(kept) =
        adapter.execute_query("SELECT id, name FROM k").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        kept.rows,
        vec![vec![Some(b"1".to_vec()), Some(b"ann".to_vec())]]
    );

    // Replacing the key column itself is refused: MySQL keeps the key through
    // a MODIFY and the engine's ALTER COLUMN would drop it.
    assert!(adapter
        .execute_query("ALTER TABLE k MODIFY COLUMN id BIGINT")
        .is_err());
    assert!(adapter
        .execute_query("ALTER TABLE k CHANGE COLUMN id key_id INT")
        .is_err());
}

/// `JSON_ARRAY` and `JSON_OBJECT` build a document out of what they are given.
///
/// Every answer and every column below measured on MySQL 8.4.11 over a utf8mb4
/// connection.
#[cfg(unix)]
#[test]
fn the_json_builders_write_what_mysql_writes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([118; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE j (id INT, name VARCHAR(8), n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO j (id, name, n) VALUES (1, 'ann', 7)")
        .unwrap();

    for (call, answer) in [
        ("JSON_ARRAY()", "[]"),
        ("JSON_ARRAY(1, 'a', NULL, 1.5)", "[1, \"a\", null, 1.5]"),
        ("JSON_ARRAY(name, n)", "[\"ann\", 7]"),
        ("JSON_OBJECT()", "{}"),
        ("JSON_OBJECT('a', 1, 'b', 'x')", "{\"a\": 1, \"b\": \"x\"}"),
        // Measured: an object's keys come back sorted, shorter first, and a
        // key written twice keeps the value written last.
        (
            "JSON_OBJECT('bb', 1, 'a', 2, 'c', 3)",
            "{\"a\": 2, \"c\": 3, \"bb\": 1}",
        ),
        ("JSON_OBJECT('a', 1, 'a', 2)", "{\"a\": 2}"),
        ("JSON_OBJECT('a', NULL)", "{\"a\": null}"),
        ("JSON_OBJECT('name', name)", "{\"name\": \"ann\"}"),
        // A string argument is a JSON string rather than a document to parse.
        ("JSON_ARRAY('{\"a\":1}')", "[\"{\\\"a\\\":1}\"]"),
    ] {
        let CommandExecutionResult::ResultSet(built) = adapter
            .execute_query(&format!("SELECT {call} FROM j"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(built.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        // Measured: the JSON type at the widest a document can be, whatever
        // the arguments were, with the text collation and the binary flag.
        assert_eq!(built.columns[0].column_type, MYSQL_TYPE_JSON, "{call}");
        assert_eq!(built.columns[0].column_length, u32::MAX - 3, "{call}");
        assert_eq!(built.columns[0].decimals, NOT_FIXED_DECIMALS, "{call}");
        assert_eq!(built.columns[0].flags, MYSQL_BINARY_FLAG, "{call}");
    }

    // MySQL answers 1582 for an odd number of arguments to JSON_OBJECT; this
    // refuses the statement rather than building a different document.
    assert!(adapter
        .execute_query("SELECT JSON_OBJECT('a') FROM j")
        .is_err());
    // A boolean literal is refused: MySQL writes `true` where the engine has
    // only the number one to write.
    assert!(adapter
        .execute_query("SELECT JSON_ARRAY(TRUE) FROM j")
        .is_err());
    // A nested call is not read here.
    assert!(adapter
        .execute_query("SELECT JSON_ARRAY(JSON_ARRAY(1)) FROM j")
        .is_err());
}

/// `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE` and `JSON_REMOVE` change one
/// member of a document.
///
/// Every answer below measured on MySQL 8.4.11 over a utf8mb4 connection. Only
/// a path naming one member of the top-level object is taken, which is where
/// the engine and MySQL agree.
#[cfg(unix)]
#[test]
fn the_json_changers_change_what_mysql_changes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([119; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE m (id INT, d JSON)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO m (id, d) VALUES (1, '{\"a\": 1, \"b\": [1, 2, 3]}')")
        .unwrap();

    for (call, answer) in [
        ("JSON_SET(d, '$.a', 9)", "{\"a\": 9, \"b\": [1, 2, 3]}"),
        (
            "JSON_SET(d, '$.c', 9)",
            "{\"a\": 1, \"b\": [1, 2, 3], \"c\": 9}",
        ),
        (
            "JSON_SET(d, '$.a', 9, '$.c', 8)",
            "{\"a\": 9, \"b\": [1, 2, 3], \"c\": 8}",
        ),
        (
            "JSON_SET(d, '$.a', NULL)",
            "{\"a\": null, \"b\": [1, 2, 3]}",
        ),
        (
            "JSON_SET(d, '$.a', 'x')",
            "{\"a\": \"x\", \"b\": [1, 2, 3]}",
        ),
        // Measured: INSERT leaves a member that is there and adds one that is
        // not; REPLACE does the opposite.
        ("JSON_INSERT(d, '$.a', 9)", "{\"a\": 1, \"b\": [1, 2, 3]}"),
        (
            "JSON_INSERT(d, '$.c', 9)",
            "{\"a\": 1, \"b\": [1, 2, 3], \"c\": 9}",
        ),
        ("JSON_REPLACE(d, '$.a', 9)", "{\"a\": 9, \"b\": [1, 2, 3]}"),
        ("JSON_REPLACE(d, '$.c', 9)", "{\"a\": 1, \"b\": [1, 2, 3]}"),
        ("JSON_REMOVE(d, '$.a')", "{\"b\": [1, 2, 3]}"),
        ("JSON_REMOVE(d, '$.nope')", "{\"a\": 1, \"b\": [1, 2, 3]}"),
        ("JSON_REMOVE(d, '$.a', '$.b')", "{}"),
        // A document written out rather than read from a column.
        ("JSON_SET('{\"a\": 1}', '$.a', 2)", "{\"a\": 2}"),
    ] {
        let CommandExecutionResult::ResultSet(changed) = adapter
            .execute_query(&format!("SELECT {call} FROM m"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(changed.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        // Measured: the same JSON column the builders report.
        assert_eq!(changed.columns[0].column_type, MYSQL_TYPE_JSON, "{call}");
        assert_eq!(changed.columns[0].column_length, u32::MAX - 3, "{call}");
        assert_eq!(changed.columns[0].flags, MYSQL_BINARY_FLAG, "{call}");
    }

    // A path past the top-level object is refused rather than answered
    // differently: measured, MySQL leaves `JSON_SET('{}', '$.x.y', 1)` alone
    // where the engine builds the missing parent, and MySQL appends
    // `JSON_SET('[1,2]', '$[5]', 9)` where the engine leaves it.
    for sql in [
        "SELECT JSON_SET(d, '$.a.b', 1) FROM m",
        "SELECT JSON_SET(d, '$.b[0]', 1) FROM m",
        "SELECT JSON_SET(d, '$[0]', 1) FROM m",
        "SELECT JSON_SET(d, '$', 1) FROM m",
        "SELECT JSON_REMOVE(d, '$.b[1]') FROM m",
        // A path has to be written out, and a value and a path have to come in
        // pairs.
        "SELECT JSON_SET(d, '$.a') FROM m",
        "SELECT JSON_REMOVE(d) FROM m",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `JSON_CONTAINS` and `JSON_CONTAINS_PATH` answer whether a document holds
/// something.
///
/// Every answer and every column below measured on MySQL 8.4.11 over a utf8mb4
/// connection.
#[cfg(unix)]
#[test]
fn the_json_searches_answer_what_mysql_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([122; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE s (id INT, d JSON, empty JSON)")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO s (id, d) VALUES \
             (1, '{\"a\": 1, \"b\": [1, 2, 3], \"s\": \"x\", \"n\": null, \"o\": {\"k\": 1}}')",
        )
        .unwrap();

    for (call, answer) in [
        ("JSON_CONTAINS_PATH(d, 'one', '$.a')", "1"),
        ("JSON_CONTAINS_PATH(d, 'one', '$.z')", "0"),
        // A member holding the JSON null counts as being there.
        ("JSON_CONTAINS_PATH(d, 'one', '$.n')", "1"),
        ("JSON_CONTAINS_PATH(d, 'all', '$.a', '$.b')", "1"),
        ("JSON_CONTAINS_PATH(d, 'all', '$.a', '$.z')", "0"),
        ("JSON_CONTAINS_PATH(d, 'one', '$.z', '$.a')", "1"),
        // The keyword is read without regard to case.
        ("JSON_CONTAINS_PATH(d, 'ONE', '$.a')", "1"),
        ("JSON_CONTAINS_PATH(d, 'one', '$.o.k')", "1"),
        ("JSON_CONTAINS_PATH(d, 'one', '$.b[1]')", "1"),
        ("JSON_CONTAINS(d, '1', '$.a')", "1"),
        ("JSON_CONTAINS(d, '2', '$.a')", "0"),
        ("JSON_CONTAINS(d, '[1,2]', '$.b')", "1"),
        ("JSON_CONTAINS(d, '[1,4]', '$.b')", "0"),
        ("JSON_CONTAINS(d, '2', '$.b')", "1"),
        ("JSON_CONTAINS(d, '\"x\"', '$.s')", "1"),
        ("JSON_CONTAINS(d, 'null', '$.n')", "1"),
        ("JSON_CONTAINS(d, '{\"k\": 1}', '$.o')", "1"),
        ("JSON_CONTAINS(d, '{\"k\": 2}', '$.o')", "0"),
        ("JSON_CONTAINS(d, '{\"a\": 1}')", "1"),
        ("JSON_CONTAINS(d, '{\"a\": 1, \"s\": \"x\"}')", "1"),
        ("JSON_CONTAINS(d, '1')", "0"),
        ("JSON_CONTAINS('[1,2,3]', '[1,3]')", "1"),
        ("JSON_CONTAINS('[1,2,3]', '2')", "1"),
        ("JSON_CONTAINS('[[1,2]]', '[1]')", "1"),
        // Two numbers are the same when they count the same.
        ("JSON_CONTAINS('1', '1.0')", "1"),
    ] {
        let CommandExecutionResult::ResultSet(answered) = adapter
            .execute_query(&format!("SELECT {call} FROM s"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(answered.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        // Measured: a LONGLONG of 21 carrying the binary and numeric flags.
        assert_eq!(
            answered.columns[0].column_type, MYSQL_TYPE_LONGLONG,
            "{call}"
        );
        assert_eq!(answered.columns[0].column_length, 21, "{call}");
        assert_eq!(
            answered.columns[0].flags,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            "{call}"
        );
    }

    // Measured: a NULL document answers NULL rather than 0, and so does a path
    // the target does not have.
    for call in [
        "JSON_CONTAINS_PATH(empty, 'one', '$.a')",
        "JSON_CONTAINS(d, '1', '$.z')",
        "JSON_CONTAINS(empty, '1')",
    ] {
        let CommandExecutionResult::ResultSet(answered) = adapter
            .execute_query(&format!("SELECT {call} FROM s"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(answered.rows[0][0], None, "{call}");
    }

    // The keyword and the paths have to be written out, and the keyword has to
    // be one MySQL takes — measured, anything else answers 3154.
    for sql in [
        "SELECT JSON_CONTAINS_PATH(d, 'some', '$.a') FROM s",
        "SELECT JSON_CONTAINS_PATH(d, 'one') FROM s",
        "SELECT JSON_CONTAINS_PATH(d, 'one', '$.*') FROM s",
        "SELECT JSON_CONTAINS(d, '1', '$.*') FROM s",
        "SELECT JSON_CONTAINS(d) FROM s",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `JSON_OVERLAPS` answers whether two documents share anything, and the two
/// merges join one into another.
///
/// Every answer and every column below measured on MySQL 8.4.11 over a utf8mb4
/// connection.
#[cfg(unix)]
#[test]
fn the_json_joins_answer_what_mysql_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([123; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE g (id INT, d JSON, empty JSON)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO g (id, d) VALUES (1, '{\"a\": 1, \"b\": [1, 2, 3]}')")
        .unwrap();

    for (call, answer) in [
        ("JSON_OVERLAPS('[1,2,3]', '[3,4]')", "1"),
        ("JSON_OVERLAPS('[1,2,3]', '[4,5]')", "0"),
        ("JSON_OVERLAPS('[1,2,3]', '2')", "1"),
        (
            "JSON_OVERLAPS('{\"a\":1,\"b\":2}', '{\"a\":1,\"c\":3}')",
            "1",
        ),
        ("JSON_OVERLAPS('{\"a\":1}', '{\"a\":2}')", "0"),
        // An array and an object share nothing, and sharing is equality rather
        // than containment.
        ("JSON_OVERLAPS('[1,2]', '{\"a\":1}')", "0"),
        ("JSON_OVERLAPS('[[1,2]]', '[1]')", "0"),
        ("JSON_OVERLAPS(d, '{\"a\":1}')", "1"),
    ] {
        let CommandExecutionResult::ResultSet(answered) = adapter
            .execute_query(&format!("SELECT {call} FROM g"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(answered.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        // Measured: a LONGLONG of 1 — the width of the one digit it writes —
        // where JSON_CONTAINS answers one of 21.
        assert_eq!(
            answered.columns[0].column_type, MYSQL_TYPE_LONGLONG,
            "{call}"
        );
        assert_eq!(answered.columns[0].column_length, 1, "{call}");
    }

    for (call, answer) in [
        (
            "JSON_MERGE_PATCH('{\"a\":1,\"b\":2}', '{\"b\":3,\"c\":4}')",
            "{\"a\": 1, \"b\": 3, \"c\": 4}",
        ),
        // A member patched with the JSON null is taken out.
        ("JSON_MERGE_PATCH('{\"a\":1}', '{\"a\":null}')", "{}"),
        ("JSON_MERGE_PATCH('[1,2]', '[3]')", "[3]"),
        (
            "JSON_MERGE_PATCH('{\"a\":1}', '{\"b\":2}', '{\"c\":3}')",
            "{\"a\": 1, \"b\": 2, \"c\": 3}",
        ),
        (
            "JSON_MERGE_PRESERVE('{\"a\":1,\"b\":2}', '{\"b\":3,\"c\":4}')",
            "{\"a\": 1, \"b\": [2, 3], \"c\": 4}",
        ),
        ("JSON_MERGE_PRESERVE('[1,2]', '[3]')", "[1, 2, 3]"),
        ("JSON_MERGE_PRESERVE('1', '2')", "[1, 2]"),
        ("JSON_MERGE_PRESERVE('{\"a\":1}', '[2]')", "[{\"a\": 1}, 2]"),
        (
            "JSON_MERGE_PRESERVE('{\"a\":1}', '{\"a\":2}', '{\"a\":3}')",
            "{\"a\": [1, 2, 3]}",
        ),
        // MySQL's deprecated spelling of JSON_MERGE_PRESERVE, still taken.
        ("JSON_MERGE('{\"a\":1}', '{\"a\":2}')", "{\"a\": [1, 2]}"),
        (
            "JSON_MERGE_PATCH(d, '{\"a\":9}')",
            "{\"a\": 9, \"b\": [1, 2, 3]}",
        ),
    ] {
        let CommandExecutionResult::ResultSet(merged) = adapter
            .execute_query(&format!("SELECT {call} FROM g"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(merged.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        // Measured: the same JSON column the builders report.
        assert_eq!(merged.columns[0].column_type, MYSQL_TYPE_JSON, "{call}");
        assert_eq!(merged.columns[0].column_length, u32::MAX - 3, "{call}");
        assert_eq!(merged.columns[0].flags, MYSQL_BINARY_FLAG, "{call}");
    }

    // Measured: a NULL document answers NULL rather than a document.
    for call in ["JSON_OVERLAPS(empty, '[1]')", "JSON_MERGE_PATCH(d, empty)"] {
        let CommandExecutionResult::ResultSet(answered) = adapter
            .execute_query(&format!("SELECT {call} FROM g"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(answered.rows[0][0], None, "{call}");
    }

    // `JSON_SEARCH` answers the paths to the strings a pattern matches.
    // Measured: only strings are looked at, the match tells one case of a
    // letter from the other, `one` answers the first path and `all` an array
    // of them — except that a single match answers the one path on its own —
    // and nothing found answers no value at all.
    adapter
        .execute_query("CREATE TABLE h (id INT, d JSON)")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO h (id, d) VALUES \
             (1, '{\"a\": \"x\", \"b\": [\"y\", \"x\"], \"o\": {\"k\": \"x\"}, \"n\": 1, \"t\": \"xyz\"}')",
        )
        .unwrap();
    for (call, answer) in [
        ("JSON_SEARCH(d, 'one', 'x')", "\"$.a\""),
        (
            "JSON_SEARCH(d, 'all', 'x')",
            "[\"$.a\", \"$.b[1]\", \"$.o.k\"]",
        ),
        ("JSON_SEARCH(d, 'one', 'x%')", "\"$.a\""),
        (
            "JSON_SEARCH(d, 'all', 'x%')",
            "[\"$.a\", \"$.b[1]\", \"$.o.k\", \"$.t\"]",
        ),
        // The keyword is read without regard to case.
        ("JSON_SEARCH(d, 'ONE', 'x')", "\"$.a\""),
        ("JSON_SEARCH('[\"a\",\"b\"]', 'all', 'a')", "\"$[0]\""),
        ("JSON_SEARCH('\"a\"', 'one', 'a')", "\"$\""),
        // The escape character is the one it was given.
        (
            "JSON_SEARCH('{\"a\":\"x_y\"}', 'one', 'x!_y', '!')",
            "\"$.a\"",
        ),
    ] {
        let CommandExecutionResult::ResultSet(found) = adapter
            .execute_query(&format!("SELECT {call} FROM h"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(found.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        // Measured: the same JSON column the merges report.
        assert_eq!(found.columns[0].column_type, MYSQL_TYPE_JSON, "{call}");
        assert_eq!(found.columns[0].column_length, u32::MAX - 3, "{call}");
    }
    for call in [
        "JSON_SEARCH(d, 'one', 'z')",
        "JSON_SEARCH(d, 'all', 'z')",
        // Only strings are looked at, so the number is never found.
        "JSON_SEARCH(d, 'one', '1')",
        // The match tells one case of a letter from the other.
        "JSON_SEARCH('{\"a\":\"X\"}', 'one', 'x')",
    ] {
        let CommandExecutionResult::ResultSet(found) = adapter
            .execute_query(&format!("SELECT {call} FROM h"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(found.rows[0][0], None, "{call}");
    }
    // The keyword has to be one MySQL takes, and a path to search inside is
    // not read here.
    assert!(adapter
        .execute_query("SELECT JSON_SEARCH(d, 'some', 'x') FROM h")
        .is_err());
    assert!(adapter
        .execute_query("SELECT JSON_SEARCH(d, 'all', 'x', NULL, '$.b') FROM h")
        .is_err());

    // A merge takes two documents at least, and an overlap exactly two.
    assert!(adapter
        .execute_query("SELECT JSON_MERGE_PATCH(d) FROM g")
        .is_err());
    assert!(adapter
        .execute_query("SELECT JSON_OVERLAPS(d, '[1]', '[2]') FROM g")
        .is_err());
}

/// `UNIX_TIMESTAMP` counts the seconds from the epoch to a moment, and
/// `FROM_UNIXTIME` reads one back.
///
/// Every answer and every column below measured on MySQL 8.4.11 over a utf8mb4
/// connection with `time_zone = '+00:00'`, which is the only zone this session
/// takes.
#[cfg(unix)]
#[test]
fn the_epoch_readings_count_what_mysql_counts() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([124; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query(
            "CREATE TABLE e (d DATETIME, day DATE, old DATETIME, n INT, name VARCHAR(8))",
        )
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO e (d, day, old, n, name) VALUES \
             ('2026-09-07 01:02:03', '2026-09-07', '1969-12-31 23:59:59', 1788735723, 'ann')",
        )
        .unwrap();

    for (call, answer) in [
        ("UNIX_TIMESTAMP(d)", "1788742923"),
        // A DATE reads as its midnight.
        ("UNIX_TIMESTAMP(day)", "1788739200"),
        // Measured: a moment before the epoch answers 0 rather than a negative
        // count.
        ("UNIX_TIMESTAMP(old)", "0"),
        ("FROM_UNIXTIME(n)", "2026-09-06 23:02:03"),
        ("FROM_UNIXTIME(0)", "1970-01-01 00:00:00"),
    ] {
        let CommandExecutionResult::ResultSet(read) = adapter
            .execute_query(&format!("SELECT {call} FROM e"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(read.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
    }

    // Measured: the count is a LONGLONG of 21 with the binary and numeric
    // flags, and the moment a DATETIME of 19 with the binary flag alone.
    let CommandExecutionResult::ResultSet(counted) = adapter
        .execute_query("SELECT UNIX_TIMESTAMP(d) FROM e")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(counted.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(counted.columns[0].column_length, 21);
    assert_eq!(counted.columns[0].flags, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG);
    let CommandExecutionResult::ResultSet(moment) = adapter
        .execute_query("SELECT FROM_UNIXTIME(n) FROM e")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(moment.columns[0].column_type, MYSQL_TYPE_DATETIME);
    assert_eq!(moment.columns[0].column_length, 19);
    assert_eq!(moment.columns[0].flags, MYSQL_BINARY_FLAG);

    // Measured: `UNIX_TIMESTAMP()` reads now and reports NOT NULL.
    let CommandExecutionResult::ResultSet(now) = adapter
        .execute_query("SELECT UNIX_TIMESTAMP() FROM e")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let seconds: i64 = String::from_utf8(now.rows[0][0].clone().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        seconds > 1_700_000_000,
        "UNIX_TIMESTAMP() answered {seconds}"
    );
    assert_eq!(
        now.columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    // The count is read out of a moment and nothing else, and the moment out
    // of a number: MySQL reads either by coercing the other, which this has
    // not measured. A second argument to FROM_UNIXTIME is a format, which is
    // not read here.
    for sql in [
        "SELECT UNIX_TIMESTAMP(n) FROM e",
        "SELECT UNIX_TIMESTAMP(name) FROM e",
        "SELECT FROM_UNIXTIME(d) FROM e",
        "SELECT FROM_UNIXTIME(name) FROM e",
        "SELECT FROM_UNIXTIME(n, '%Y') FROM e",
        "SELECT FROM_UNIXTIME() FROM e",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `ALTER TABLE` adds a foreign key to a table that already has rows, and
/// takes one away, which is what a migration writes.
///
/// Every answer below measured on MySQL 8.4.11 over a utf8mb4 connection.
#[cfg(unix)]
#[test]
fn alter_table_adds_and_drops_a_foreign_key() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([125; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE p (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE c (a INT, b INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO p (id) VALUES (1)")
        .unwrap();

    adapter
        .execute_query("ALTER TABLE c ADD CONSTRAINT fk_b FOREIGN KEY (b) REFERENCES p (id)")
        .unwrap();
    // Measured: the key prints under the name it was given.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE c").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    let printed = String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap();
    assert!(
        printed.contains("CONSTRAINT `fk_b` FOREIGN KEY (`b`) REFERENCES `p` (`id`)"),
        "{printed}"
    );

    // The key is enforced from that point on: a child row naming no parent
    // answers 1452, and one naming a parent is taken.
    assert_eq!(
        adapter.execute_query("INSERT INTO c (a, b) VALUES (1, 9)"),
        Err(FrontendErrorKind::ForeignKeyViolation)
    );
    adapter
        .execute_query("INSERT INTO c (a, b) VALUES (1, 1)")
        .unwrap();

    // Dropped by the name it was given, under either of MySQL's spellings.
    adapter
        .execute_query("ALTER TABLE c DROP FOREIGN KEY fk_b")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE c").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    let printed = String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap();
    assert!(!printed.contains("FOREIGN KEY"), "{printed}");
    // The rows the key was holding are still there, and one it would have
    // refused is taken now.
    adapter
        .execute_query("INSERT INTO c (a, b) VALUES (2, 9)")
        .unwrap();
    let CommandExecutionResult::ResultSet(kept) =
        adapter.execute_query("SELECT COUNT(*) FROM c").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(kept.rows, vec![vec![Some(b"2".to_vec())]]);

    // `DROP CONSTRAINT` is MySQL's other spelling and drops the same key.
    adapter
        .execute_query("ALTER TABLE c ADD CONSTRAINT fk_a FOREIGN KEY (a) REFERENCES p (id)")
        .unwrap();
    adapter
        .execute_query("ALTER TABLE c DROP CONSTRAINT fk_a")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE c").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert!(!String::from_utf8(created.rows[0][1].clone().unwrap())
        .unwrap()
        .contains("FOREIGN KEY"));

    // A name the table does not carry has nothing to drop, and a name it
    // already carries cannot be added twice.
    assert!(adapter
        .execute_query("ALTER TABLE c DROP FOREIGN KEY nope")
        .is_err());
    adapter
        .execute_query("ALTER TABLE c ADD CONSTRAINT fk_b FOREIGN KEY (b) REFERENCES p (id)")
        .unwrap();
    assert!(adapter
        .execute_query("ALTER TABLE c ADD CONSTRAINT fk_b FOREIGN KEY (a) REFERENCES p (id)")
        .is_err());
}

/// An `information_schema` query names the columns it wants, in the order it
/// wants them, and is answered that way.
///
/// The catalog answers three of MySQL's twenty-one `TABLES` columns and seven
/// of its twenty-two `COLUMNS` ones. Which of those a query names, and in what
/// order, is up to the query; a column outside the set is refused rather than
/// answered with a value that would be made up.
#[cfg(unix)]
#[test]
fn an_information_schema_query_is_answered_in_the_order_it_asked() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([126; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT, label TEXT)")
        .unwrap();

    // One column of TABLES, and no ORDER BY: the rows come back in table-name
    // order either way.
    let CommandExecutionResult::ResultSet(named) = adapter
        .execute_query(
            "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(named.columns.len(), 1);
    assert_eq!(named.columns[0].name, "TABLE_NAME");
    // The catalog this test opens already carries `records`.
    assert_eq!(
        named.rows,
        vec![vec![Some(b"records".to_vec())], vec![Some(b"t".to_vec())]]
    );

    // Two of them, in the other order.
    let CommandExecutionResult::ResultSet(swapped) = adapter
        .execute_query(concat!(
            "SELECT TABLE_TYPE, TABLE_NAME FROM information_schema.TABLES ",
            "WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME"
        ))
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let names = swapped
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["TABLE_TYPE", "TABLE_NAME"]);
    assert_eq!(
        swapped.rows,
        vec![
            vec![Some(b"BASE TABLE".to_vec()), Some(b"records".to_vec())],
            vec![Some(b"BASE TABLE".to_vec()), Some(b"t".to_vec())],
        ]
    );

    // The same for COLUMNS.
    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query(concat!(
            "SELECT IS_NULLABLE, COLUMN_NAME FROM information_schema.COLUMNS ",
            "WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 't'"
        ))
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let names = read
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["IS_NULLABLE", "COLUMN_NAME"]);
    assert_eq!(
        read.rows,
        vec![
            vec![Some(b"YES".to_vec()), Some(b"id".to_vec())],
            vec![Some(b"YES".to_vec()), Some(b"label".to_vec())],
        ]
    );

    // A column MySQL has and this does not answer is refused, and so is the
    // same column named twice — which MySQL answers twice.
    for sql in [
        "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        "SELECT TABLE_NAME, TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        "SELECT * FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        "SELECT COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 't'",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// An `information_schema` query goes through the ordinary `SELECT` path, so
/// it filters and orders by any column it likes.
///
/// The shape-matching catalog answers one written form per table. This one is
/// scanned by the engine, so a `WHERE` over `TABLE_TYPE` and an `ORDER BY`
/// over any column are answered by the query engine rather than recognized.
#[cfg(unix)]
#[test]
fn an_information_schema_table_is_read_through_the_select_path() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([127; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE alpha (id INT)")
        .unwrap();

    // A WHERE over a column the shape-matching catalog could not filter on,
    // and an ORDER BY it could not sort by.
    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query(concat!(
            "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES ",
            "WHERE TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME DESC"
        ))
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let names = read
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["TABLE_NAME", "TABLE_TYPE"]);
    // The catalog this test opens already carries `records`.
    assert_eq!(
        read.rows,
        vec![
            vec![Some(b"records".to_vec()), Some(b"BASE TABLE".to_vec())],
            vec![Some(b"alpha".to_vec()), Some(b"BASE TABLE".to_vec())],
        ]
    );

    // Measured on MySQL 8.4.11: the columns report the shapes MySQL reports
    // for them, which are pinned rather than worked out from a declared type —
    // `TABLE_NAME` is a VAR_STRING of 256 whose original table is `tables`.
    assert_eq!(read.columns[0].column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(read.columns[0].column_length, 256);
    assert_eq!(read.columns[0].original_table, "tables");
    assert_eq!(read.columns[0].schema, "information_schema");

    // The logical database the connection selected.
    let CommandExecutionResult::ResultSet(scoped) = adapter
        .execute_query(concat!(
            "SELECT TABLE_SCHEMA FROM information_schema.TABLES ",
            "WHERE TABLE_NAME = 'alpha'"
        ))
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(scoped.rows, vec![vec![Some(b"reports".to_vec())]]);

    // An aggregate or a call over one of these columns has not been measured,
    // so it is refused rather than answered with a shape worked out from a
    // declared type this table does not have.
    // Counting the rows works, because a count does not depend on what the
    // column holds.
    let CommandExecutionResult::ResultSet(counted) = adapter
        .execute_query("SELECT COUNT(TABLE_NAME) FROM information_schema.TABLES")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(counted.rows, vec![vec![Some(b"2".to_vec())]]);

    // A call over one of these columns has not been measured, so it is refused
    // rather than answered with a shape worked out from a declared type this
    // table does not have. So is a table `information_schema` does not have,
    // and any other schema's table.
    for sql in [
        "SELECT UPPER(TABLE_NAME) FROM information_schema.TABLES",
        "SELECT ROUTINE_NAME FROM information_schema.ROUTINES",
        "SELECT id FROM other.alpha",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `TRUNCATE` cuts a number off at a count of places.
///
/// Every answer and every column below measured on MySQL 8.4.11 over a utf8mb4
/// connection.
#[cfg(unix)]
#[test]
fn truncate_cuts_a_number_where_mysql_cuts_it() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([121; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE u (i INT, d DOUBLE, small DOUBLE, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO u (i, d, small, name) VALUES (1234, 1234.5678, 0.29, 'ann')")
        .unwrap();

    for (call, answer) in [
        ("TRUNCATE(d, 2)", "1234.56"),
        ("TRUNCATE(d, 0)", "1234"),
        // Nothing is rounded on the way: 1234.5678 cut at three is 1234.567.
        ("TRUNCATE(d, 3)", "1234.567"),
        // The double behind 0.29 is under it, and cutting the double would
        // answer 0.28.
        ("TRUNCATE(small, 2)", "0.29"),
        // A negative count zeroes digits left of the point.
        ("TRUNCATE(d, -2)", "1200"),
        ("TRUNCATE(i, -2)", "1200"),
        ("TRUNCATE(i, 2)", "1234"),
    ] {
        let CommandExecutionResult::ResultSet(cut) = adapter
            .execute_query(&format!("SELECT {call} FROM u"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(cut.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
    }

    // Measured: a LONGLONG of 21 over an integer column and a DOUBLE of 23
    // over a double, both carrying the binary and numeric flags.
    for (call, column_type, width) in [
        ("TRUNCATE(i, 2)", MYSQL_TYPE_LONGLONG, 21),
        ("TRUNCATE(d, 2)", MYSQL_TYPE_DOUBLE, 23),
    ] {
        let CommandExecutionResult::ResultSet(cut) = adapter
            .execute_query(&format!("SELECT {call} FROM u"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(cut.columns[0].column_type, column_type, "{call}");
        assert_eq!(cut.columns[0].column_length, width, "{call}");
        assert_eq!(
            cut.columns[0].flags,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            "{call}"
        );
    }

    // A text column is refused, and so is a count read from a column.
    assert!(adapter
        .execute_query("SELECT TRUNCATE(name, 2) FROM u")
        .is_err());
    assert!(adapter
        .execute_query("SELECT TRUNCATE(d, i) FROM u")
        .is_err());

    // Over a DECIMAL the answer is a DECIMAL of its own, whose scale is the
    // count held to the column's. Measured: `DECIMAL(10,3)` cut at two reports
    // 11 with a scale of 2, at five the column's own 12 and 3, and at zero or
    // below 8 with no scale at all.
    adapter
        .execute_query("CREATE TABLE p (a DECIMAL(10,3), b DECIMAL(5,0), c DECIMAL(3,2))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO p (a, b, c) VALUES (1234.5678, 12345, 1.99)")
        .unwrap();
    for (call, answer, width, scale) in [
        ("TRUNCATE(a, 2)", "1234.56", 11, 2),
        ("TRUNCATE(a, 0)", "1234", 8, 0),
        ("TRUNCATE(a, -2)", "1200", 8, 0),
        ("TRUNCATE(a, 5)", "1234.568", 12, 3),
        ("TRUNCATE(b, 2)", "12345", 6, 0),
        ("TRUNCATE(c, 1)", "1.9", 4, 1),
    ] {
        let CommandExecutionResult::ResultSet(cut) = adapter
            .execute_query(&format!("SELECT {call} FROM p"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(cut.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        assert_eq!(cut.columns[0].column_type, MYSQL_TYPE_NEWDECIMAL, "{call}");
        assert_eq!(cut.columns[0].column_length, width, "{call}");
        assert_eq!(cut.columns[0].decimals, scale, "{call}");
    }
}

/// `FORMAT` writes a number for a person to read.
///
/// Every answer and every column below measured on MySQL 8.4.11 over a utf8mb4
/// connection.
#[cfg(unix)]
#[test]
fn format_writes_a_number_grouped_in_threes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([120; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE f (i INT, b BIGINT, d DOUBLE, name VARCHAR(8))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO f (i, b, d, name) VALUES (1234, 1234567, 1234.5678, 'ann')")
        .unwrap();

    for (call, answer) in [
        ("FORMAT(d, 2)", "1,234.57"),
        ("FORMAT(d, 0)", "1,235"),
        ("FORMAT(d, 10)", "1,234.5678000000"),
        ("FORMAT(i, 2)", "1,234.00"),
        ("FORMAT(b, 0)", "1,234,567"),
        // Measured: a negative count answers no fraction rather than rounding
        // to a whole ten.
        ("FORMAT(d, -1)", "1,235"),
    ] {
        let CommandExecutionResult::ResultSet(written) = adapter
            .execute_query(&format!("SELECT {call} FROM f"))
            .unwrap_or_else(|error| panic!("{call}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(
            String::from_utf8(written.rows[0][0].clone().unwrap()).unwrap(),
            answer,
            "{call}"
        );
        assert_eq!(
            written.columns[0].column_type, MYSQL_TYPE_VAR_STRING,
            "{call}"
        );
        assert_eq!(written.columns[0].flags, 0, "{call}");
    }

    // Measured: the width is the column's own length plus a comma for every
    // three of its digits plus thirty-two, and the count does not change it —
    // 184 over an INT of 11, 232 over a BIGINT of 20, 244 over a DOUBLE of 22.
    for (call, width) in [
        ("FORMAT(i, 2)", 184),
        ("FORMAT(i, 0)", 184),
        ("FORMAT(b, 2)", 232),
        ("FORMAT(d, 2)", 244),
    ] {
        let CommandExecutionResult::ResultSet(written) = adapter
            .execute_query(&format!("SELECT {call} FROM f"))
            .unwrap()
        else {
            panic!("SELECT must return a result set");
        };
        assert_eq!(written.columns[0].column_length, width, "{call}");
    }

    // MySQL formats a text column by coercing it, which this has not measured.
    assert!(adapter
        .execute_query("SELECT FORMAT(name, 2) FROM f")
        .is_err());
    // The count is written out rather than read from a column.
    assert!(adapter.execute_query("SELECT FORMAT(d, i) FROM f").is_err());
}

/// `BIGINT UNSIGNED` takes 0 to `i64::MAX` where MySQL takes twice as much.
///
/// The engine holds an integer as an `i64`, so the top half of MySQL's range
/// has nowhere to go. What is under it behaves as MySQL does — measured on
/// 8.4.11, a LONGLONG of 20 reporting UNSIGNED, printed `bigint unsigned`, and
/// a negative answering 1264. Above `i64::MAX` this answers 1264 too, which is
/// the divergence: MySQL stores those.
#[cfg(unix)]
#[test]
fn bigint_unsigned_takes_the_half_of_mysqls_range_an_i64_holds() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([116; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE ub (id INT, u BIGINT UNSIGNED)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO ub (id, u) VALUES (1, 0), (2, 9223372036854775807)")
        .unwrap();

    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT u FROM ub ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(selected.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(selected.columns[0].column_length, 20);
    assert_eq!(
        selected.columns[0].flags & MYSQL_UNSIGNED_FLAG,
        MYSQL_UNSIGNED_FLAG
    );
    assert_eq!(
        selected.rows,
        vec![
            vec![Some(b"0".to_vec())],
            vec![Some(b"9223372036854775807".to_vec())],
        ]
    );

    let CommandExecutionResult::ResultSet(columns) =
        adapter.execute_query("SHOW COLUMNS FROM ub").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        String::from_utf8(columns.rows[1][1].clone().unwrap()).unwrap(),
        "bigint unsigned"
    );

    // Measured: a negative answers 1264.
    assert_eq!(
        adapter.execute_query("INSERT INTO ub (id, u) VALUES (3, -1)"),
        Err(FrontendErrorKind::OutOfRange)
    );

    // The divergence: MySQL stores 9223372036854775808 and this cannot, so it
    // answers rather than storing something else.
    assert!(adapter
        .execute_query("INSERT INTO ub (id, u) VALUES (4, 9223372036854775808)")
        .is_err());
    let CommandExecutionResult::ResultSet(kept) =
        adapter.execute_query("SELECT COUNT(*) FROM ub").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(kept.rows, vec![vec![Some(b"2".to_vec())]]);
    // Measured: the sign prints as a second lower-case word here too.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE ub").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert!(
        String::from_utf8(created.rows[0][1].clone().unwrap())
            .unwrap()
            .contains("`u` bigint unsigned"),
        "{:?}",
        created.rows[0][1]
    );
}

/// `MODIFY COLUMN` and `CHANGE COLUMN` restate one column whole, which is how a
/// migration widens a type or renames a column. Every answer below measured on
/// MySQL 8.4.11.
#[cfg(unix)]
#[test]
fn alter_table_restates_a_column_whole() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([115; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    adapter
        .execute_query("CREATE TABLE t (id INT, name VARCHAR(8), n INT DEFAULT 5)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, name, n) VALUES (1, 'ann', 10), (2, 'bo', 20)")
        .unwrap();

    fn described(adapter: &mut impl AuthenticatedCommandExecutor, column: &str) -> Vec<String> {
        let CommandExecutionResult::ResultSet(result) =
            adapter.execute_query("SHOW COLUMNS FROM t").unwrap()
        else {
            panic!("SHOW COLUMNS must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        Some(bytes) => String::from_utf8(bytes.clone()).unwrap(),
                        None => "NULL".to_owned(),
                    })
                    .collect::<Vec<_>>()
            })
            .find(|row| row[0] == column)
            .unwrap_or_else(|| panic!("no column {column}"))
    }

    // A widened type keeps the rows that were already there.
    adapter
        .execute_query("ALTER TABLE t MODIFY COLUMN name VARCHAR(20) NOT NULL")
        .unwrap();
    let name = described(&mut adapter, "name");
    assert_eq!(name[1], "varchar(20)");
    assert_eq!(name[2], "NO");

    // Measured: an attribute the statement does not restate is gone — the
    // `DEFAULT 5` does not survive a MODIFY that does not say it again.
    adapter
        .execute_query("ALTER TABLE t MODIFY COLUMN n BIGINT")
        .unwrap();
    let widened = described(&mut adapter, "n");
    assert_eq!(widened[1], "bigint");
    assert_eq!(widened[2], "YES");
    assert_eq!(widened[4], "NULL");

    // CHANGE renames the column as well as restating it.
    adapter
        .execute_query("ALTER TABLE t CHANGE COLUMN name label VARCHAR(20)")
        .unwrap();
    let renamed = described(&mut adapter, "label");
    assert_eq!(renamed[1], "varchar(20)");
    assert_eq!(renamed[2], "YES");
    let CommandExecutionResult::ResultSet(kept) = adapter
        .execute_query("SELECT label, n FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        kept.rows,
        vec![
            vec![Some(b"ann".to_vec()), Some(b"10".to_vec())],
            vec![Some(b"bo".to_vec()), Some(b"20".to_vec())],
        ]
    );

    // Measured: a column the table does not have answers 1054.
    assert_eq!(
        adapter.execute_query("ALTER TABLE t MODIFY COLUMN nope INT"),
        Err(FrontendErrorKind::UnknownColumn)
    );

    // MySQL moves a column with FIRST or AFTER and the engine has no way to,
    // so it is refused rather than quietly leaving the column where it was.
    for sql in [
        "ALTER TABLE t MODIFY COLUMN n BIGINT FIRST",
        "ALTER TABLE t CHANGE COLUMN label label VARCHAR(20) AFTER id",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
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
    // Measured over a utf8mb4 connection, which is the only one this server
    // serves: each width counts the four bytes utf8mb4 reserves for a
    // character, and the columns carry the connection's own collation.
    for (ordinal, length, not_null) in [
        (0, 256, true),
        (1, 32, true),
        (2, 320, true),
        (3, 12, false),
        (4, 12, false),
        (5, 12, false),
    ] {
        let column = &engines.columns[ordinal];
        assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING, "{ordinal}");
        assert_eq!(column.column_length, length, "{ordinal}");
        assert_eq!(
            column.character_set,
            u16::from(DEFAULT_UTF8MB4_COLLATION),
            "{ordinal}"
        );
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

    // An AUTO_INCREMENT table takes it too. The allocator reads only the
    // column-list form, so the SET one is written out as that before it gets
    // there. Measured on MySQL 8.4.11: `INSERT INTO ai SET v = 1, s = 'a'`
    // numbers the row 1 and LAST_INSERT_ID answers 1, and a second SET
    // numbers 2.
    adapter
        .execute_query(
            "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT, s VARCHAR(8))",
        )
        .unwrap();
    let CommandExecutionResult::Ok(numbered) = adapter
        .execute_query("INSERT INTO t SET v = 1, s = 'a'")
        .unwrap()
    else {
        panic!("INSERT must return OK");
    };
    assert_eq!(numbered.affected_rows, 1);
    assert_eq!(numbered.last_insert_id, 1);
    adapter.execute_query("INSERT INTO t SET v = 2").unwrap();
    // Naming the key itself is refused on both forms alike, which is what
    // keeps them the same statement: the allocator reserves before the row is
    // written, and a row carrying its own key would not go through it.
    assert_eq!(
        adapter.execute_query("INSERT INTO t SET id = 10, v = 3"),
        adapter.execute_query("INSERT INTO t (id, v) VALUES (10, 3)")
    );
    assert!(adapter
        .execute_query("INSERT INTO t SET id = 10, v = 3")
        .is_err());
    let CommandExecutionResult::ResultSet(numbered) = adapter
        .execute_query("SELECT id, v, s FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(
        numbered.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"1".to_vec()),
                Some(b"a".to_vec())
            ],
            vec![Some(b"2".to_vec()), Some(b"2".to_vec()), None],
        ]
    );

    // An upsert clause on an AUTO_INCREMENT table is refused on both forms
    // alike: the allocator reserves before the clause can turn the row into an
    // update.
    assert!(adapter
        .execute_query("INSERT INTO t SET v = 1 ON DUPLICATE KEY UPDATE v = 2")
        .is_err());

    // Measured on MySQL 8.4.11 over a table that allocates nothing:
    // `INSERT INTO k SET id = 1, v = 20 ON DUPLICATE KEY UPDATE n = 999`
    // leaves v at what the row already held and writes n, which is what the
    // column-list form does with the same clause.
    adapter
        .execute_query("CREATE TABLE k (id INT NOT NULL PRIMARY KEY, v INT, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO k SET id = 1, v = 10, n = 100")
        .unwrap();
    adapter
        .execute_query("INSERT INTO k SET id = 1, v = 20 ON DUPLICATE KEY UPDATE n = 999")
        .unwrap();
    let CommandExecutionResult::ResultSet(upserted) = adapter
        .execute_query("SELECT id, v, n FROM k ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(
        upserted.rows,
        vec![vec![
            Some(b"1".to_vec()),
            Some(b"10".to_vec()),
            Some(b"999".to_vec())
        ]]
    );
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

    // Measured: SHOW CREATE TABLE prints the sign the same way.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE u").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    let printed = String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap();
    for column in [
        "`a` tinyint unsigned",
        "`b` smallint unsigned",
        "`c` mediumint unsigned",
        "`d` int unsigned",
    ] {
        assert!(printed.contains(column), "{printed}");
    }

    // Measured on MySQL 8.4.11: one past the top value answers 1264, and so
    // does a negative.
    assert_eq!(
        adapter.execute_query("INSERT INTO u (id, d) VALUES (2, 4294967296)"),
        Err(FrontendErrorKind::OutOfRange)
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO u (id, d) VALUES (3, -1)"),
        Err(FrontendErrorKind::OutOfRange)
    );

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
        (
            "SELECT id FROM t WHERE name IN ('a', 'C') ORDER BY id",
            vec!["2", "3"],
        ),
        (
            "SELECT id FROM t WHERE id IN (1, 3) ORDER BY id",
            vec!["1", "3"],
        ),
        ("SELECT id FROM t WHERE id IN (1, NULL)", vec!["1"]),
        ("SELECT id FROM t WHERE id NOT IN (1, NULL)", vec![]),
        (
            "SELECT id FROM t WHERE id NOT IN (1) ORDER BY id",
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

    // A name both tables carry says which of them it came from or it is
    // ambiguous: measured on MySQL 8.4.11, 1052.
    assert_eq!(
        adapter.execute_query(
            "SELECT o.name, MAX(id) FROM owners AS o JOIN pets AS p ON o.id = p.owner_id GROUP BY name"
        ),
        Err(FrontendErrorKind::AmbiguousColumn)
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

    // A name both tables carry and no `USING` merges is ambiguous, which is
    // 1052 on MySQL 8.4.11.
    assert_eq!(
        adapter.execute_query("SELECT id, a.x, b.y FROM a JOIN b ON a.id = b.id"),
        Err(FrontendErrorKind::AmbiguousColumn)
    );
    // A `USING` naming a column one side does not carry is refused; MySQL
    // answers 1054 there and this answers its own refusal.
    assert!(adapter
        .execute_query("SELECT id, a.x FROM a JOIN b USING (y)")
        .is_err());
}

/// A CROSS JOIN computes the full Cartesian product without an ON clause, and
/// preserves NOT NULL flags on both sides because neither side can go missing.
#[cfg(unix)]
#[test]
fn cross_join_computes_cartesian_product_and_preserves_not_null() {
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
        .execute_query("CREATE TABLE a (id INT NOT NULL PRIMARY KEY, x VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query("CREATE TABLE b (id INT NOT NULL PRIMARY KEY, y VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO a (id, x) VALUES (1, 'p'), (2, 'q')")
        .unwrap();
    adapter
        .execute_query("INSERT INTO b (id, y) VALUES (10, 'r'), (20, 's')")
        .unwrap();

    let CommandExecutionResult::ResultSet(result) = adapter
        .execute_query("SELECT a.id, b.id FROM a CROSS JOIN b ORDER BY a.id, b.id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        result.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"10".to_vec())],
            vec![Some(b"1".to_vec()), Some(b"20".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"10".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"20".to_vec())],
        ]
    );

    // Both sides keep NOT NULL flag (measured on MySQL 8.4.11)
    assert_eq!(
        result.columns[0].flags & MYSQL_NOT_NULL_FLAG,
        MYSQL_NOT_NULL_FLAG
    );
    assert_eq!(
        result.columns[1].flags & MYSQL_NOT_NULL_FLAG,
        MYSQL_NOT_NULL_FLAG
    );
    assert_eq!(result.columns[0].table, "a");
    assert_eq!(result.columns[0].original_table, "a");
    assert_eq!(result.columns[1].table, "b");
    assert_eq!(result.columns[1].original_table, "b");

    // Projections of non-key columns
    let CommandExecutionResult::ResultSet(text_result) = adapter
        .execute_query("SELECT a.x, b.y FROM a CROSS JOIN b ORDER BY a.id, b.id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        text_result.rows,
        vec![
            vec![Some(b"p".to_vec()), Some(b"r".to_vec())],
            vec![Some(b"p".to_vec()), Some(b"s".to_vec())],
            vec![Some(b"q".to_vec()), Some(b"r".to_vec())],
            vec![Some(b"q".to_vec()), Some(b"s".to_vec())],
        ]
    );

    // COUNT(*) over cross join
    let CommandExecutionResult::ResultSet(count_result) = adapter
        .execute_query("SELECT COUNT(*) FROM a CROSS JOIN b")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(count_result.rows, vec![vec![Some(b"4".to_vec())]]);

    // Both tables must exist and be authorized
    assert!(adapter
        .execute_query("SELECT a.id, non_existent.id FROM a CROSS JOIN non_existent")
        .is_err());
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
    // A denied session reads `information_schema` and sees nothing in it,
    // which is what MySQL shows a user with no grants — not an error.
    let CommandExecutionResult::ResultSet(empty) = adapter
        .execute_query("SELECT table_name FROM information_schema.tables")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert!(empty.rows.is_empty());
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            RecordedDatabaseAction::Query("reports".to_owned()),
            // The `information_schema` read is authorized as a query, and then
            // what it may see is decided against the grants it holds on the
            // tables its rows would name.
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

/// A scalar subquery in a projection answers what its aggregate answers.
#[cfg(unix)]
#[test]
fn a_scalar_subquery_answers_the_shape_its_aggregate_answers() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([32; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE inner_t (id INT NOT NULL, n INT, d DECIMAL(10,2))",
        "CREATE TABLE outer_t (id INT NOT NULL)",
        "INSERT INTO inner_t (id, n, d) VALUES (1, 10, 1.5), (2, 20, 2.5)",
        "INSERT INTO outer_t (id) VALUES (1)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    // Measured on MySQL 8.4.11: each answers the shape its aggregate answers
    // on its own — MAX over an INT a LONG of 11, COUNT a LONGLONG of 21, SUM
    // over a DECIMAL(10,2) a NEWDECIMAL of 34 with its scale — and each is
    // nullable, where a plain COUNT is NOT NULL.
    let CommandExecutionResult::ResultSet(answered) = adapter
        .execute_query(
            "SELECT id, (SELECT MAX(n) FROM inner_t) AS m, (SELECT COUNT(*) FROM inner_t) AS c, (SELECT SUM(d) FROM inner_t) AS s FROM outer_t",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        answered
            .columns
            .iter()
            .skip(1)
            .map(|column| (
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags
            ))
            .collect::<Vec<_>>(),
        [
            (MYSQL_TYPE_LONG, 11, 0, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG),
            (
                MYSQL_TYPE_LONGLONG,
                21,
                0,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                MYSQL_TYPE_NEWDECIMAL,
                34,
                2,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );
    assert_eq!(
        answered.rows,
        vec![vec![
            Some(b"1".to_vec()),
            Some(b"20".to_vec()),
            Some(b"2".to_vec()),
            Some(b"4.00".to_vec()),
        ]]
    );

    // Unaliased, MySQL names the column after the subquery's own text.
    let CommandExecutionResult::ResultSet(named) = adapter
        .execute_query("SELECT (SELECT MAX(n) FROM inner_t) FROM outer_t")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(named.columns[0].name, "(SELECT MAX(n) FROM inner_t)");

    // The table the subquery reads is authorized and checked like any other,
    // which is what carrying it as a read table is for.
    assert!(adapter
        .execute_query("SELECT (SELECT COUNT(*) FROM sqlite_schema) FROM outer_t")
        .is_err());

    // A subquery answering a column rather than an aggregate would answer the
    // column's own shape and a missing row as NULL, which is unmeasured here.
    assert!(adapter
        .execute_query("SELECT (SELECT n FROM inner_t) FROM outer_t")
        .is_err());
}

/// `INSERT INTO t <SELECT>` with no column list means every column of the
/// table, in order.
#[cfg(unix)]
#[test]
fn insert_select_without_a_column_list_takes_every_column() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([31; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE src (id INT NOT NULL, name VARCHAR(8), n INT)",
        "CREATE TABLE dst (id INT NOT NULL, name VARCHAR(8), n INT)",
        "INSERT INTO src (id, name, n) VALUES (1, 'a', 10), (2, 'b', 20)",
    ] {
        adapter.execute_query(sql).unwrap();
    }

    // Measured on MySQL 8.4.11: all three columns are copied and ROW_COUNT()
    // is the rows copied.
    let CommandExecutionResult::Ok(copied) = adapter
        .execute_query("INSERT INTO dst SELECT * FROM src")
        .unwrap()
    else {
        panic!("INSERT must return OK");
    };
    assert_eq!(copied.affected_rows, 2);
    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id, name, n FROM dst ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(
        rows.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"a".to_vec()),
                Some(b"10".to_vec())
            ],
            vec![
                Some(b"2".to_vec()),
                Some(b"b".to_vec()),
                Some(b"20".to_vec())
            ],
        ]
    );

    // A named projection works the same way, and a REPLACE writes over the
    // row it collides with.
    adapter.execute_query("DELETE FROM dst").unwrap();
    adapter
        .execute_query("INSERT INTO dst SELECT id, name, n FROM src WHERE id = 1")
        .unwrap();
    let CommandExecutionResult::ResultSet(one) = adapter
        .execute_query("SELECT id FROM dst ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(one.rows.len(), 1);

    // A SELECT answering a different number of columns is refused rather than
    // written into the wrong ones — MySQL answers 1136.
    assert!(adapter
        .execute_query("INSERT INTO dst SELECT id, name FROM src")
        .is_err());

    // The forms that carry their own rules are refused where they are
    // written, not written out with a column list.
    for sql in [
        "INSERT IGNORE INTO dst SELECT * FROM src",
        "INSERT INTO dst SELECT * FROM src ON DUPLICATE KEY UPDATE n = 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// An unsigned `DOUBLE` or `FLOAT` takes no negative value.
#[cfg(unix)]
#[test]
fn an_unsigned_real_refuses_a_negative() {
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
            "CREATE TABLE u (id INT NOT NULL, b DOUBLE UNSIGNED, c FLOAT UNSIGNED, d DECIMAL(10,2) UNSIGNED)",
        )
        .unwrap();
    adapter
        .execute_query("INSERT INTO u (id, b, c, d) VALUES (1, 2.5, 3.5, 1.5), (2, 0, 0, 0)")
        .unwrap();

    // Measured on MySQL 8.4.11: the same type and the same width the signed
    // form reports — a DOUBLE 22 and a FLOAT 12, both with the not-fixed
    // decimals value — with the unsigned flag beside them.
    let CommandExecutionResult::ResultSet(selected) = adapter
        .execute_query("SELECT b, c, d FROM u ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        (
            selected.columns[0].column_type,
            selected.columns[0].column_length,
            selected.columns[0].decimals,
            selected.columns[0].flags & MYSQL_UNSIGNED_FLAG
        ),
        (
            MYSQL_TYPE_DOUBLE,
            22,
            NOT_FIXED_DECIMALS,
            MYSQL_UNSIGNED_FLAG
        )
    );
    assert_eq!(
        (
            selected.columns[1].column_type,
            selected.columns[1].column_length,
            selected.columns[1].flags & MYSQL_UNSIGNED_FLAG
        ),
        (MYSQL_TYPE_FLOAT, 12, MYSQL_UNSIGNED_FLAG)
    );
    // Measured: a DECIMAL spends a character on the sign, so an unsigned one
    // is a digit narrower — 11 for (10,2) against the signed 12.
    assert_eq!(
        (
            selected.columns[2].column_type,
            selected.columns[2].column_length,
            selected.columns[2].decimals,
            selected.columns[2].flags & MYSQL_UNSIGNED_FLAG
        ),
        (MYSQL_TYPE_NEWDECIMAL, 11, 2, MYSQL_UNSIGNED_FLAG)
    );
    // The scale still decides the text form, so 1.5 reads back as 1.50.
    assert_eq!(
        String::from_utf8(selected.rows[0][2].clone().unwrap()).unwrap(),
        "1.50"
    );

    // Measured: the sign prints as a second lower-case word.
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE u").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `u` (\n",
            "  `id` int NOT NULL,\n",
            "  `b` double unsigned DEFAULT NULL,\n",
            "  `c` float unsigned DEFAULT NULL,\n",
            "  `d` decimal(10,2) unsigned DEFAULT NULL\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // Measured: zero is taken and a negative answers 1264.
    assert_eq!(
        adapter.execute_query("INSERT INTO u (id, b, c, d) VALUES (3, -0.5, 1, 1)"),
        Err(FrontendErrorKind::OutOfRange)
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO u (id, b, c, d) VALUES (3, 1, -1, 1)"),
        Err(FrontendErrorKind::OutOfRange)
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO u (id, b, c, d) VALUES (3, 1, 1, -1.5)"),
        Err(FrontendErrorKind::OutOfRange)
    );
    assert_eq!(
        adapter.execute_query("UPDATE u SET b = -1 WHERE id = 1"),
        Err(FrontendErrorKind::OutOfRange)
    );
    let CommandExecutionResult::ResultSet(kept) = adapter
        .execute_query("SELECT b FROM u ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return rows");
    };
    assert_eq!(kept.rows.len(), 2);
}

/// A `DOUBLE` reads back in MySQL's own text form.
#[test]
fn a_double_reads_back_the_way_mysql_writes_one() {
    // Every pair is measured on MySQL 8.4.11 by storing the value in a
    // `DOUBLE` column and reading it back.
    for (value, text) in [
        (1.0, "1"),
        (-0.0, "0"),
        (0.0, "0"),
        (0.25, "0.25"),
        (0.1, "0.1"),
        (-1.5, "-1.5"),
        (1.0 / 3.0, "0.3333333333333333"),
        (2.0 / 3.0, "0.6666666666666666"),
        (1e-3, "0.001"),
        (1e-8, "0.00000001"),
        (1.5e-5, "0.000015"),
        // The point is fifteen places to the left here and sixteen there.
        (1e-15, "0.000000000000001"),
        (1.25e-15, "0.00000000000000125"),
        (1e-16, "1e-16"),
        (1.25e-16, "1.25e-16"),
        (5e-324, "5e-324"),
        (1.5e-300, "1.5e-300"),
        // Fifteen digits before the point are written out, sixteen are not,
        // and a sixteenth digit after the point still is.
        (1e14, "100000000000000"),
        (999_999_999_999_999.0, "999999999999999"),
        (123_456_789_012_345.6, "123456789012345.6"),
        (1e15, "1e15"),
        (1_234_567_890_123_456.0, "1.234567890123456e15"),
        (9_999_999_999_999_999.0, "1e16"),
        (12_345_678_901_234_567.0, "1.2345678901234568e16"),
        (-1e15, "-1e15"),
        (1.5e20, "1.5e20"),
        (1e100, "1e100"),
        (1e308, "1e308"),
    ] {
        assert_eq!(mysql_double_text(value), text, "{value:?}");
    }
}

/// The window calls number, rank, shift and total the rows a `SELECT` answers.
#[cfg(unix)]
#[test]
fn window_calls_answer_the_shape_mysql_answers() {
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
        .execute_query("CREATE TABLE w (id INT NOT NULL PRIMARY KEY, g VARCHAR(4), n INT)")
        .unwrap();
    adapter
        .execute_query(
            "INSERT INTO w (id, g, n) VALUES (1,'a',10), (2,'A',30), (3,'b',20), (4,'b',20)",
        )
        .unwrap();

    let CommandExecutionResult::ResultSet(numbered) = adapter
        .execute_query("SELECT id, ROW_NUMBER() OVER (ORDER BY n) FROM w ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    // Measured on MySQL 8.4.11: a LONGLONG of length 21 with no decimals,
    // carrying the NOT NULL, unsigned and numeric flags, named after the call
    // with its whole OVER clause.
    let ranked = &numbered.columns[1];
    assert_eq!(ranked.name, "ROW_NUMBER() OVER (ORDER BY n)");
    assert_eq!(ranked.column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(ranked.column_length, 21);
    assert_eq!(ranked.decimals, 0);
    assert_eq!(
        ranked.flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG
    );
    assert_eq!(
        numbered
            .rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["1", "4", "2", "3"]
    );
    // A divergence, recorded in COMPAT.md: the engine answers every column of
    // a windowed statement out of its own sorter, so the other columns lose
    // the table they came from and the key flags that go with it. MySQL
    // reports `id` here against `w` with NOT_NULL and PRI_KEY.
    let carried = &numbered.columns[0];
    assert_eq!(carried.name, "id");
    assert_eq!(carried.table, "");
    assert_eq!(carried.original_table, "");
    // Only the numeric flag survives, because that one comes from the type.
    assert_eq!(carried.flags, MYSQL_NUM_FLAG);

    // Measured: 'a' and 'A' are one partition, because MySQL's default
    // collation ignores case when it groups just as when it compares.
    let CommandExecutionResult::ResultSet(partitioned) = adapter
        .execute_query("SELECT id, RANK() OVER (PARTITION BY g ORDER BY n) AS r FROM w ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(partitioned.columns[1].name, "r");
    assert_eq!(
        partitioned
            .rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["1", "2", "1", "1"]
    );

    // Measured: DENSE_RANK closes the gap RANK leaves.
    let CommandExecutionResult::ResultSet(dense) = adapter
        .execute_query("SELECT id, DENSE_RANK() OVER (ORDER BY n) AS d FROM w ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        dense
            .rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["1", "3", "2", "2"]
    );

    // Measured: NTILE answers the same shape the ranking three do, and
    // PERCENT_RANK and CUME_DIST a DOUBLE of length 23 with the not-fixed
    // decimals value, NOT NULL and numeric but not binary.
    let CommandExecutionResult::ResultSet(tiled) = adapter
        .execute_query("SELECT id, NTILE(2) OVER (ORDER BY n) FROM w ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(tiled.columns[1].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(tiled.columns[1].column_length, 21);
    assert_eq!(
        tiled.columns[1].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG
    );
    assert_eq!(
        tiled
            .rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["1", "2", "1", "2"]
    );

    // Measured: PERCENT_RANK and CUME_DIST answer a DOUBLE of length 23 with
    // the not-fixed decimals value, NOT NULL and numeric but not binary, and
    // their values read back the way MySQL writes a double.
    for (sql, expected) in [
        (
            "SELECT id, PERCENT_RANK() OVER (ORDER BY n) FROM w ORDER BY id",
            ["0", "1", "0.3333333333333333", "0.3333333333333333"],
        ),
        (
            "SELECT id, CUME_DIST() OVER (ORDER BY n) FROM w ORDER BY id",
            ["0.25", "1", "0.75", "0.75"],
        ),
    ] {
        let CommandExecutionResult::ResultSet(fraction) = adapter.execute_query(sql).unwrap()
        else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(fraction.columns[1].column_type, MYSQL_TYPE_DOUBLE, "{sql}");
        assert_eq!(fraction.columns[1].column_length, 23, "{sql}");
        assert_eq!(fraction.columns[1].decimals, NOT_FIXED_DECIMALS, "{sql}");
        assert_eq!(
            fraction.columns[1].flags,
            MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
            "{sql}"
        );
        assert_eq!(
            fraction
                .rows
                .iter()
                .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            expected,
            "{sql}"
        );
    }

    // Measured: LAG and LEAD answer the column's own shape widened to
    // LONGLONG, always nullable, numeric but not binary.
    let CommandExecutionResult::ResultSet(shifted) = adapter
        .execute_query(
            "SELECT LAG(n) OVER (ORDER BY id), LEAD(n) OVER (ORDER BY id) FROM w ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(shifted.columns[0].name, "LAG(n) OVER (ORDER BY id)");
    assert_eq!(shifted.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(shifted.columns[0].column_length, 11);
    assert_eq!(shifted.columns[0].decimals, 0);
    assert_eq!(shifted.columns[0].flags, MYSQL_NUM_FLAG);
    assert_eq!(
        shifted.rows,
        vec![
            vec![None, Some(b"30".to_vec())],
            vec![Some(b"10".to_vec()), Some(b"20".to_vec())],
            vec![Some(b"30".to_vec()), Some(b"20".to_vec())],
            vec![Some(b"20".to_vec()), None],
        ]
    );

    // Measured: a windowed aggregate answers the shape its plain form does,
    // apart from the binary flag, which it does not carry, and MIN and MAX,
    // which widen an INT to LONGLONG where the plain form leaves it LONG.
    // With no frame written, MySQL runs the aggregate from the start of the
    // partition to the current row when the window orders, and over the whole
    // partition when it does not; the engine does the same.
    let CommandExecutionResult::ResultSet(running) = adapter
        .execute_query(
            "SELECT SUM(n) OVER (ORDER BY id), COUNT(*) OVER (ORDER BY id), MIN(n) OVER (ORDER BY id) FROM w ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(running.columns[0].column_type, MYSQL_TYPE_NEWDECIMAL);
    assert_eq!(running.columns[0].column_length, 33);
    assert_eq!(running.columns[0].decimals, 0);
    assert_eq!(running.columns[0].flags, MYSQL_NUM_FLAG);
    assert_eq!(running.columns[1].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(running.columns[1].column_length, 21);
    assert_eq!(
        running.columns[1].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG
    );
    assert_eq!(running.columns[2].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(running.columns[2].column_length, 11);
    assert_eq!(running.columns[2].flags, MYSQL_NUM_FLAG);
    assert_eq!(
        running
            .rows
            .iter()
            .map(|row| row
                .iter()
                .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            ["10", "1", "10"],
            ["40", "2", "10"],
            ["60", "3", "10"],
            ["80", "4", "10"],
        ]
    );

    // A window that only partitions runs the aggregate over the whole
    // partition, and 'a' and 'A' are one partition.
    let CommandExecutionResult::ResultSet(grouped) = adapter
        .execute_query(
            "SELECT MAX(n) OVER (PARTITION BY g), AVG(n) OVER (PARTITION BY g) FROM w ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    // Measured: AVG answers a NEWDECIMAL of length 16 and four decimal places,
    // which is what its plain form answers too.
    assert_eq!(grouped.columns[1].column_type, MYSQL_TYPE_NEWDECIMAL);
    assert_eq!(grouped.columns[1].column_length, 16);
    assert_eq!(grouped.columns[1].decimals, 4);
    assert_eq!(grouped.columns[1].flags, MYSQL_NUM_FLAG);
    assert_eq!(
        grouped
            .rows
            .iter()
            .map(|row| row
                .iter()
                .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            ["30", "20.0000"],
            ["30", "20.0000"],
            ["20", "20.0000"],
            ["20", "20.0000"],
        ]
    );

    // Measured: FIRST_VALUE, LAST_VALUE and NTH_VALUE answer the same shape
    // LAG does, and read the ends of the frame the window leaves by default —
    // so LAST_VALUE answers the current row while the window orders, which is
    // MySQL's answer too.
    let CommandExecutionResult::ResultSet(ends) = adapter
        .execute_query(
            "SELECT FIRST_VALUE(n) OVER (ORDER BY id), LAST_VALUE(n) OVER (ORDER BY id), NTH_VALUE(n, 2) OVER (ORDER BY id) FROM w ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    for column in &ends.columns {
        assert_eq!(
            (column.column_type, column.column_length, column.flags),
            (MYSQL_TYPE_LONGLONG, 11, MYSQL_NUM_FLAG),
            "{}",
            column.name
        );
    }
    assert_eq!(
        ends.rows
            .iter()
            .map(|row| row
                .iter()
                .map(|value| value
                    .as_ref()
                    .map(|bytes| String::from_utf8(bytes.clone()).unwrap()))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            [Some("10".to_owned()), Some("10".to_owned()), None],
            [
                Some("10".to_owned()),
                Some("30".to_owned()),
                Some("30".to_owned())
            ],
            [
                Some("10".to_owned()),
                Some("20".to_owned()),
                Some("30".to_owned())
            ],
            [
                Some("10".to_owned()),
                Some("20".to_owned()),
                Some("30".to_owned())
            ],
        ]
    );

    // A frame says which rows around this one the call reads. Measured on
    // MySQL 8.4.11 over the four rows: the engine answers the same for every
    // one of these, including `RANGE`, which counts a row's peers in where
    // `ROWS` counts positions.
    adapter
        .execute_query("CREATE TABLE f (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO f (id, n) VALUES (1,10), (2,30), (3,20), (4,5)")
        .unwrap();
    for (frame, expected) in [
        (
            "ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
            ["10", "40", "60", "65"],
        ),
        ("ROWS UNBOUNDED PRECEDING", ["10", "40", "60", "65"]),
        (
            "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
            ["10", "40", "60", "65"],
        ),
        (
            "ROWS BETWEEN 1 PRECEDING AND CURRENT ROW",
            ["10", "40", "50", "25"],
        ),
        (
            "ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING",
            ["65", "55", "25", "5"],
        ),
        (
            "ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
            ["40", "60", "55", "25"],
        ),
    ] {
        let sql = format!("SELECT SUM(n) OVER (ORDER BY id {frame}) FROM f ORDER BY id");
        let CommandExecutionResult::ResultSet(framed) = adapter.execute_query(&sql).unwrap() else {
            panic!("{sql} must return a result set");
        };
        assert_eq!(
            framed
                .rows
                .iter()
                .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
                .collect::<Vec<_>>(),
            expected,
            "{sql}"
        );
    }

    // Measured: where the ordering column ties, `RANGE` takes a row's peers
    // in with it and `ROWS` does not, and the engine draws the same line.
    adapter
        .execute_query("CREATE TABLE p (id INT NOT NULL PRIMARY KEY, n INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO p (id, n) VALUES (1,10), (2,10), (3,20), (4,20)")
        .unwrap();
    let CommandExecutionResult::ResultSet(peers) = adapter
        .execute_query(
            "SELECT SUM(id) OVER (ORDER BY n RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), SUM(id) OVER (ORDER BY n ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), SUM(id) OVER (ORDER BY n RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING) FROM p ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        peers
            .rows
            .iter()
            .map(|row| row
                .iter()
                .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            ["3", "1", "3"],
            ["3", "3", "3"],
            ["10", "6", "7"],
            ["10", "10", "7"],
        ]
    );

    // A `WINDOW` clause names a window the calls reach for by name. Measured
    // on MySQL 8.4.11: the answers are the ones the same window written out
    // gives, and the column is named after the call with its `OVER win`.
    let CommandExecutionResult::ResultSet(named) = adapter
        .execute_query(
            "SELECT SUM(n) OVER win, ROW_NUMBER() OVER win FROM f WINDOW win AS (ORDER BY id) ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(named.columns[0].name, "SUM(n) OVER win");
    assert_eq!(named.columns[0].column_type, MYSQL_TYPE_NEWDECIMAL);
    assert_eq!(named.columns[1].name, "ROW_NUMBER() OVER win");
    assert_eq!(
        named
            .rows
            .iter()
            .map(|row| row
                .iter()
                .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [["10", "1"], ["40", "2"], ["60", "3"], ["65", "4"]]
    );

    // The window has to be written out, over plain columns, and the shapes
    // beyond these are not measured here.
    for sql in [
        "SELECT ROW_NUMBER() OVER nosuch FROM w WINDOW win AS (ORDER BY n)",
        "SELECT ROW_NUMBER() OVER () FROM w",
        "SELECT ROW_NUMBER() OVER (ORDER BY n + 1) FROM w",
        // Measured: MySQL answers 1235 for GROUPS.
        "SELECT SUM(n) OVER (ORDER BY id GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
        "SELECT SUM(n) OVER (ORDER BY id ROWS BETWEEN -1 PRECEDING AND CURRENT ROW) FROM w",
        "SELECT SUM(n) OVER (ORDER BY id ROWS BETWEEN n PRECEDING AND CURRENT ROW) FROM w",
        // Measured: NTILE(0) answers 1210, so a count below one is refused.
        "SELECT NTILE(0) OVER (ORDER BY n) FROM w",
        // An offset or a default argument brings rules of its own.
        "SELECT LAG(n, 2) OVER (ORDER BY id) FROM w",
        "SELECT LAG(n, 1, 0) OVER (ORDER BY id) FROM w",
        "SELECT SUM(n + 1) OVER (ORDER BY id) FROM w",
        // Measured: NTH_VALUE(col, 0) answers 1210, like NTILE(0).
        "SELECT NTH_VALUE(n, 0) OVER (ORDER BY id) FROM w",
        "SELECT NTH_VALUE(n) OVER (ORDER BY id) FROM w",
        "SELECT FIRST_VALUE(n, 2) OVER (ORDER BY id) FROM w",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
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

    // Measured on MySQL 8.4.11: integer arithmetic makes a BIGINT, NOT NULL
    // when every column it names is NOT NULL and carrying a zero default
    // there, and nullable otherwise. `src.id` is NOT NULL and `src.n` is not.
    adapter
        .execute_query("CREATE TABLE computed AS SELECT id + 1 AS s, n * 2 AS d FROM src")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE computed").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `computed` (\n",
            "  `s` bigint NOT NULL DEFAULT '0',\n",
            "  `d` bigint DEFAULT NULL\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // Division makes a decimal on a rule of its own, and an unaliased
    // expression column is named after its own text.
    for sql in [
        "CREATE TABLE from_div AS SELECT id / 2 AS s FROM src",
        "CREATE TABLE from_expr AS SELECT id + 1 FROM src",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }

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

    // Measured on MySQL 8.4.11: an unnamed key is named after its first
    // column, and where that is taken it gains `_2`, `_3` and so on, counting
    // the names the table already carries and the ones this statement has
    // named. `KEY (a), KEY a_2 (b), KEY (a)` names the three a, a_2 and a_3.
    adapter
        .execute_query("CREATE TABLE n (a INT, b INT)")
        .unwrap();
    adapter
        .execute_query("ALTER TABLE n ADD INDEX (a), ADD KEY a_2 (b), ADD INDEX (a)")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE n").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `n` (\n",
            "  `a` int DEFAULT NULL,\n",
            "  `b` int DEFAULT NULL,\n",
            "  KEY `a` (`a`),\n",
            "  KEY `a_2` (`b`),\n",
            "  KEY `a_3` (`a`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    // `DROP KEY` is MySQL's other spelling for `DROP INDEX` and drops the same
    // key. The parser library reads only the second, so the words are swapped
    // before it sees them.
    adapter
        .execute_query("ALTER TABLE n DROP KEY a_2, DROP KEY a_3")
        .unwrap();
    let CommandExecutionResult::ResultSet(created) =
        adapter.execute_query("SHOW CREATE TABLE n").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    assert_eq!(
        String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `n` (\n",
            "  `a` int DEFAULT NULL,\n",
            "  `b` int DEFAULT NULL,\n",
            "  KEY `a` (`a`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    // A name the table does not carry answers 1091 under either spelling.
    assert_eq!(
        adapter.execute_query("ALTER TABLE n DROP KEY a_2"),
        Err(FrontendErrorKind::CantDropKey)
    );

    // The spellings and shapes this does not take.
    for sql in [
        "ALTER TABLE t ADD COLUMN e INT, ADD INDEX idx_e (e)",
        "ALTER TABLE t DROP INDEX `PRIMARY`",
        "ALTER TABLE t DROP KEY `PRIMARY`",
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

/// A user variable is the connection's own: it survives from one statement to
/// the next and another connection never sees it. Measured on MySQL 8.4.11,
/// which also answers NULL rather than an error for one never set.
#[test]
fn a_user_variable_belongs_to_the_connection_that_set_it() {
    let mut mine = adapter();
    let mut other = adapter();
    mine.execute_query("SET @label = 'held'").unwrap();

    let CommandExecutionResult::ResultSet(held) = mine.execute_query("SELECT @label").unwrap()
    else {
        panic!("SELECT of a user variable must return a result set");
    };
    assert_eq!(held.rows, vec![vec![Some(b"held".to_vec())]]);

    let CommandExecutionResult::ResultSet(theirs) = other.execute_query("SELECT @label").unwrap()
    else {
        panic!("SELECT of a user variable must return a result set");
    };
    assert_eq!(theirs.rows, vec![vec![None]]);

    // Resetting the connection takes it away, which is what MySQL does.
    mine.execute_reset_connection().unwrap();
    let CommandExecutionResult::ResultSet(after) = mine.execute_query("SELECT @label").unwrap()
    else {
        panic!("SELECT of a user variable must return a result set");
    };
    assert_eq!(after.rows, vec![vec![None]]);

    // A system variable still reaches its own reader.
    let CommandExecutionResult::ResultSet(system) = mine.execute_query("SELECT @@version").unwrap()
    else {
        panic!("SELECT of a system variable must return a result set");
    };
    assert_eq!(system.columns[0].name, "@@version");
}

/// A savepoint marks a point inside a transaction and, unlike a plain
/// ROLLBACK, rolling back to one leaves the transaction open. Measured on
/// MySQL 8.4.11 over rows written around a savepoint.
///
/// This runs against a real database file rather than the in-memory fixture: a
/// savepoint needs the pager's sub-journal, and over the in-memory one the
/// engine takes `SAVEPOINT` and `ROLLBACK TO` without undoing anything.
#[cfg(unix)]
#[test]
fn a_savepoint_rolls_back_part_of_a_transaction_and_keeps_it_open() {
    let (_directory, _catalog, mut adapter) = savepoint_adapter([90; 32]);
    adapter.execute_query("BEGIN").unwrap();
    adapter
        .execute_query("INSERT INTO sp (id) VALUES (1)")
        .unwrap();
    adapter.execute_query("SAVEPOINT s1").unwrap();
    adapter
        .execute_query("INSERT INTO sp (id) VALUES (2)")
        .unwrap();

    let CommandExecutionResult::Ok(rolled_back) =
        adapter.execute_query("ROLLBACK TO SAVEPOINT s1").unwrap()
    else {
        panic!("ROLLBACK TO must produce an OK result");
    };
    // The transaction is still open, which is the whole difference from a
    // plain ROLLBACK: a client reading this flag must not be told otherwise.
    assert_eq!(
        rolled_back.status_flags,
        SERVER_STATUS_IN_TRANS | SERVER_STATUS_AUTOCOMMIT
    );

    adapter
        .execute_query("INSERT INTO sp (id) VALUES (3)")
        .unwrap();
    adapter.execute_query("RELEASE SAVEPOINT s1").unwrap();
    adapter.execute_query("COMMIT").unwrap();

    let CommandExecutionResult::ResultSet(rows) = adapter
        .execute_query("SELECT id FROM sp ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert_eq!(
        rows.rows,
        vec![vec![Some(b"1".to_vec())], vec![Some(b"3".to_vec())]]
    );
}

/// Measured on MySQL 8.4.11: a savepoint that is not there answers 1305, a
/// `ROLLBACK TO` forgets every savepoint taken after the one it names, and a
/// COMMIT forgets them all.
#[cfg(unix)]
#[test]
fn a_missing_savepoint_answers_the_error_mysql_answers() {
    let (_directory, _catalog, mut adapter) = savepoint_adapter([91; 32]);
    adapter.execute_query("BEGIN").unwrap();
    assert_eq!(
        adapter.execute_query("ROLLBACK TO SAVEPOINT nosuch"),
        Err(FrontendErrorKind::NoSuchSavepoint)
    );
    assert_eq!(
        adapter.execute_query("RELEASE SAVEPOINT nosuch"),
        Err(FrontendErrorKind::NoSuchSavepoint)
    );

    adapter.execute_query("SAVEPOINT s1").unwrap();
    adapter.execute_query("SAVEPOINT s2").unwrap();
    adapter.execute_query("ROLLBACK TO s1").unwrap();
    assert_eq!(
        adapter.execute_query("RELEASE SAVEPOINT s2"),
        Err(FrontendErrorKind::NoSuchSavepoint)
    );
    adapter.execute_query("COMMIT").unwrap();

    adapter.execute_query("BEGIN").unwrap();
    assert_eq!(
        adapter.execute_query("ROLLBACK TO s1"),
        Err(FrontendErrorKind::NoSuchSavepoint)
    );
    adapter.execute_query("ROLLBACK").unwrap();

    // With autocommit on and no transaction open, a SAVEPOINT answers OK and
    // nothing survives it, so rolling back to it is the same 1305.
    let CommandExecutionResult::Ok(lone) = adapter.execute_query("SAVEPOINT lone").unwrap() else {
        panic!("SAVEPOINT must produce an OK result");
    };
    assert_eq!(lone.status_flags, SERVER_STATUS_AUTOCOMMIT);
    assert_eq!(
        adapter.execute_query("ROLLBACK TO lone"),
        Err(FrontendErrorKind::NoSuchSavepoint)
    );
}

/// With autocommit off the transaction is the session's, so a savepoint taken
/// before the first write survives to roll that write back. Measured on MySQL
/// 8.4.11.
#[cfg(unix)]
#[test]
fn a_savepoint_works_inside_an_implicit_transaction() {
    let (_directory, _catalog, mut adapter) = savepoint_adapter([92; 32]);
    adapter.execute_query("SET autocommit = 0").unwrap();
    adapter.execute_query("SAVEPOINT s1").unwrap();
    adapter
        .execute_query("INSERT INTO sp (id) VALUES (7)")
        .unwrap();
    adapter.execute_query("ROLLBACK TO s1").unwrap();
    adapter.execute_query("COMMIT").unwrap();

    let CommandExecutionResult::ResultSet(rows) =
        adapter.execute_query("SELECT id FROM sp").unwrap()
    else {
        panic!("SELECT must produce a result set");
    };
    assert!(rows.rows.is_empty());
}

/// One authorized adapter over a real database file, holding the table the
/// savepoint tests write to. The directory and catalog own the files, so the
/// caller keeps them alive for as long as it uses the adapter.
#[cfg(unix)]
fn savepoint_adapter(
    account: [u8; 32],
) -> (
    tempfile::TempDir,
    Arc<MySqlDatabaseCatalog>,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes(account),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();
    adapter.execute_query("CREATE TABLE sp (id INT)").unwrap();
    (directory, catalog, adapter)
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

    // Measured on MySQL 8.4.11: `EXPLAIN t` prints exactly what `DESCRIBE t`
    // prints, and anything else after EXPLAIN is the optimizer's plan.
    assert_eq!(
        adapter.execute_query("EXPLAIN records"),
        adapter.execute_query("DESCRIBE records")
    );

    // Measured: the pattern names the columns to report, `DESCRIBE t <name>`
    // reads it the way `SHOW COLUMNS FROM t LIKE <name>` does, and one nothing
    // matches answers no rows rather than an error.
    let CommandExecutionResult::ResultSet(matched) = adapter
        .execute_query("SHOW COLUMNS FROM records LIKE 'lab%'")
        .unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        matched
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["label"]
    );
    assert_eq!(
        adapter.execute_query("DESCRIBE records 'lab%'"),
        adapter.execute_query("SHOW COLUMNS FROM records LIKE 'lab%'")
    );
    assert_eq!(
        adapter.execute_query("DESCRIBE records label"),
        adapter.execute_query("SHOW COLUMNS FROM records LIKE 'label'")
    );
    let CommandExecutionResult::ResultSet(unmatched) = adapter
        .execute_query("SHOW COLUMNS FROM records LIKE 'zzz'")
        .unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert!(unmatched.rows.is_empty());
    assert_eq!(unmatched.columns.len(), 6);

    // Measured on MySQL 8.4.11: `FULL` puts `Collation` third and appends
    // `Privileges` and `Comment`. The collation is the text one for a VARCHAR,
    // CHAR or TEXT and NULL for every other type. The comment is empty, which
    // is the only comment a column here can have. `Privileges` is answered
    // NULL, a divergence recorded in COMPAT.md: MySQL reports the user's
    // grants on the column and this server's grants are not per column.
    let CommandExecutionResult::ResultSet(full) = adapter
        .execute_query("SHOW FULL COLUMNS FROM records")
        .unwrap()
    else {
        panic!("SHOW FULL COLUMNS must return a result set");
    };
    assert_eq!(
        full.columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Field",
            "Type",
            "Collation",
            "Null",
            "Key",
            "Default",
            "Extra",
            "Privileges",
            "Comment"
        ]
    );
    assert_eq!(
        full.rows
            .iter()
            .map(|row| (
                String::from_utf8(row[0].clone().unwrap()).unwrap(),
                row[2]
                    .as_ref()
                    .map(|value| String::from_utf8(value.clone()).unwrap()),
                row[7].clone(),
                row[8].clone(),
            ))
            .collect::<Vec<_>>(),
        [
            ("id".to_owned(), None, None, Some(Vec::new())),
            (
                "label".to_owned(),
                Some("utf8mb4_0900_ai_ci".to_owned()),
                None,
                Some(Vec::new())
            ),
        ]
    );
    for sql in [
        "EXPLAIN SELECT id FROM records",
        "EXPLAIN FORMAT = JSON SELECT 1",
        "EXPLAIN ANALYZE SELECT 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
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
    let result = show_columns_result(columns, SERVER_STATUS_AUTOCOMMIT, false).unwrap();

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
    // Measured over a utf8mb4 connection: `Type` and `Default` are blobs and
    // `Key` is a fixed-width string, and every one carries the connection's own
    // collation.
    assert_eq!(
        definitions
            .iter()
            .map(|column| (column.column_type, column.column_length))
            .collect::<Vec<_>>(),
        [
            (MYSQL_TYPE_VAR_STRING, 256),
            (MYSQL_TYPE_BLOB, 67_108_860),
            (MYSQL_TYPE_VAR_STRING, 12),
            (MYSQL_TYPE_STRING, 12),
            (MYSQL_TYPE_BLOB, 262_140),
            (MYSQL_TYPE_VAR_STRING, 1024),
        ]
    );
    assert!(definitions
        .iter()
        .all(|column| column.character_set == u16::from(DEFAULT_UTF8MB4_COLLATION)));

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
        show_columns_result(
            vec![bounded[0].clone(); MAX_DISPATCH_RESULT_ROWS + 1],
            SERVER_STATUS_AUTOCOMMIT,
            false,
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
        show_columns_result(oversized_default, SERVER_STATUS_AUTOCOMMIT, false),
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
        show_columns_result(packet_bound, SERVER_STATUS_AUTOCOMMIT, false),
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
        show_columns_result(
            vec![retained[0].clone(); MAX_DISPATCH_RESULT_ROWS],
            SERVER_STATUS_AUTOCOMMIT,
            false,
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
            None,
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
fn show_tables_like_filters_by_case_and_names_its_column_after_the_pattern() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([84; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_query("USE reports").unwrap();
    adapter
        .execute_query("CREATE TABLE ledger (id INT)")
        .unwrap();

    // Measured on MySQL 8.4.11: the pattern goes into the column name and a
    // table name is matched by case, so `LIKE 'REC%'` answers nothing.
    let CommandExecutionResult::ResultSet(matched) =
        adapter.execute_query("SHOW TABLES LIKE 'rec%'").unwrap()
    else {
        panic!("SHOW TABLES must return a result set");
    };
    assert_eq!(matched.columns[0].name, "Tables_in_reports (rec%)");
    assert_eq!(matched.rows, vec![vec![Some(b"records".to_vec())]]);

    let CommandExecutionResult::ResultSet(unmatched) =
        adapter.execute_query("SHOW TABLES LIKE 'REC%'").unwrap()
    else {
        panic!("SHOW TABLES must return a result set");
    };
    assert_eq!(unmatched.columns[0].name, "Tables_in_reports (REC%)");
    assert!(unmatched.rows.is_empty());

    let CommandExecutionResult::ResultSet(full) = adapter
        .execute_query("SHOW FULL TABLES LIKE 'ledger'")
        .unwrap()
    else {
        panic!("SHOW FULL TABLES must return a result set");
    };
    assert_eq!(full.columns[0].name, "Tables_in_reports (ledger)");
    assert_eq!(
        full.rows,
        vec![vec![Some(b"ledger".to_vec()), Some(b"BASE TABLE".to_vec())]]
    );

    assert_eq!(
        adapter.execute_query("SHOW TABLES LIKE ledger"),
        Err(FrontendErrorKind::Syntax)
    );
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
            columns: vec![show_tables_column("reports", None)],
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
fn information_schema_tables_refuses_what_it_cannot_answer_and_reads_the_rest() {
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
        // A wildcard asks for MySQL's twenty-one columns and this answers
        // three, so a row of a different width would come back.
        "SELECT * FROM information_schema.TABLES",
        // A column MySQL has and this does not answer.
        "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME; SELECT 1",
    ] {
        assert!(
            adapter.execute_query(query).is_err(),
            "information_schema.TABLES query this cannot answer must be refused: {query}"
        );
    }

    // An ordering the recognized shape does not carry is refused while it
    // still carries the `WHERE TABLE_SCHEMA = DATABASE()`: the checked
    // `SELECT` surface, which would sort by any column it is given, does not
    // read `DATABASE()` in a `WHERE` yet.
    for query in [
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_SCHEMA",
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME DESC",
    ] {
        assert!(adapter.execute_query(query).is_err(), "{query}");
    }

    // Written without that predicate, the same orderings are answered by the
    // ordinary `SELECT` path.
    for query in [
        "SELECT TABLE_NAME FROM information_schema.TABLES ORDER BY TABLE_SCHEMA",
        "SELECT TABLE_NAME FROM information_schema.TABLES ORDER BY TABLE_NAME DESC",
    ] {
        assert!(adapter.execute_query(query).is_ok(), "{query}");
    }
    assert_eq!(
        authorizer.actions(),
        vec![
            RecordedDatabaseAction::Connect(None),
            RecordedDatabaseAction::Connect(Some("reports".to_owned())),
            // Each query that reaches the ordinary `SELECT` path is authorized
            // there, whether it is then answered or refused; only the wildcard
            // is turned away before it gets that far.
            RecordedDatabaseAction::Query("reports".to_owned()),
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
fn information_schema_statistics_reports_every_index_column_with_measured_shapes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("metadata").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([51; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("metadata").unwrap();
    for sql in [
        "CREATE TABLE records (id INT NOT NULL PRIMARY KEY, code VARCHAR(32) NOT NULL, label VARCHAR(64))",
        "CREATE UNIQUE INDEX uk_code ON records (code)",
        "CREATE INDEX idx_label ON records (label)",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let query = "SELECT TABLE_SCHEMA, TABLE_NAME, NON_UNIQUE, INDEX_NAME, SEQ_IN_INDEX, COLUMN_NAME, NULLABLE, INDEX_TYPE, PACKED FROM information_schema.STATISTICS ORDER BY INDEX_NAME, SEQ_IN_INDEX";
    let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap() else {
        panic!("information_schema.STATISTICS must return a result set");
    };
    assert_eq!(
        result.rows,
        // `idx_label` sorts before `PRIMARY` because MySQL orders text without
        // regard to case, which is what the rendered `ORDER BY` asks for.
        vec![
            vec![
                Some(b"metadata".to_vec()),
                Some(b"records".to_vec()),
                Some(b"1".to_vec()),
                Some(b"idx_label".to_vec()),
                Some(b"1".to_vec()),
                Some(b"label".to_vec()),
                Some(b"YES".to_vec()),
                Some(b"BTREE".to_vec()),
                None,
            ],
            vec![
                Some(b"metadata".to_vec()),
                Some(b"records".to_vec()),
                Some(b"0".to_vec()),
                Some(b"PRIMARY".to_vec()),
                Some(b"1".to_vec()),
                Some(b"id".to_vec()),
                Some(Vec::new()),
                Some(b"BTREE".to_vec()),
                None,
            ],
            vec![
                Some(b"metadata".to_vec()),
                Some(b"records".to_vec()),
                Some(b"0".to_vec()),
                Some(b"uk_code".to_vec()),
                Some(b"1".to_vec()),
                Some(b"code".to_vec()),
                Some(Vec::new()),
                Some(b"BTREE".to_vec()),
                None,
            ],
        ]
    );
    // Every shape here is the one MySQL 8.4.11 reports, taken from the pinned
    // golden. A numeric column carries the binary collation and a text one
    // carries utf8mb4, and the column MySQL never fills has the null type.
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.schema.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.character_set,
                column.flags,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "information_schema",
                "STATISTICS",
                "schemata",
                "TABLE_SCHEMA",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "information_schema",
                "STATISTICS",
                "tables",
                "TABLE_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "information_schema",
                "STATISTICS",
                "",
                "NON_UNIQUE",
                MYSQL_TYPE_LONG,
                2,
                MYSQL_BINARY_COLLATION,
                MYSQL_NOT_NULL_FLAG,
            ),
            (
                "information_schema",
                "STATISTICS",
                "",
                "INDEX_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                0,
            ),
            (
                "information_schema",
                "STATISTICS",
                "index_column_usage",
                "SEQ_IN_INDEX",
                MYSQL_TYPE_LONG,
                10,
                MYSQL_BINARY_COLLATION,
                MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "information_schema",
                "STATISTICS",
                "",
                "COLUMN_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                0,
            ),
            (
                "information_schema",
                "STATISTICS",
                "",
                "NULLABLE",
                MYSQL_TYPE_VAR_STRING,
                12,
                45,
                MYSQL_NOT_NULL_FLAG,
            ),
            (
                "information_schema",
                "STATISTICS",
                "",
                "INDEX_TYPE",
                MYSQL_TYPE_VAR_STRING,
                44,
                45,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            ),
            (
                "information_schema",
                "STATISTICS",
                "",
                "PACKED",
                MYSQL_TYPE_NULL,
                0,
                MYSQL_BINARY_COLLATION,
                MYSQL_BINARY_FLAG,
            ),
        ]
    );

    // A comparison against a numeric column takes an integer, which is what
    // this table declares for it rather than the text every column of
    // `information_schema.TABLES` holds.
    let CommandExecutionResult::ResultSet(unique_only) = adapter
        .execute_query(
            "SELECT INDEX_NAME FROM information_schema.STATISTICS WHERE NON_UNIQUE = 0 ORDER BY INDEX_NAME",
        )
        .unwrap()
    else {
        panic!("information_schema.STATISTICS must return a result set");
    };
    assert_eq!(
        unique_only.rows,
        vec![
            vec![Some(b"PRIMARY".to_vec())],
            vec![Some(b"uk_code".to_vec())],
        ]
    );

    for query in [
        // A wildcard asks for MySQL's eighteen columns and this answers
        // seventeen of them.
        "SELECT * FROM information_schema.STATISTICS",
        // The one column MySQL has that this does not answer.
        "SELECT CARDINALITY FROM information_schema.STATISTICS",
        // A numeric column takes no text.
        "SELECT INDEX_NAME FROM information_schema.STATISTICS WHERE NON_UNIQUE = 'no'",
    ] {
        assert!(
            adapter.execute_query(query).is_err(),
            "information_schema.STATISTICS query this cannot answer must be refused: {query}"
        );
    }
}

#[cfg(unix)]
#[test]
fn information_schema_key_column_usage_reports_the_keys_a_migration_tool_reads() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("metadata").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([52; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("metadata").unwrap();
    for sql in [
        "CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, code VARCHAR(32) NOT NULL)",
        "CREATE UNIQUE INDEX uk_code ON parent (code)",
        "CREATE TABLE child (cid INT NOT NULL PRIMARY KEY, parent_id INT NOT NULL, label VARCHAR(64), CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) REFERENCES parent (id))",
        "CREATE INDEX idx_label ON child (label)",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let query = "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, ORDINAL_POSITION, POSITION_IN_UNIQUE_CONSTRAINT, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE ORDER BY TABLE_NAME, CONSTRAINT_NAME";
    let CommandExecutionResult::ResultSet(result) = adapter.execute_query(query).unwrap() else {
        panic!("information_schema.KEY_COLUMN_USAGE must return a result set");
    };
    assert_eq!(
        result.rows,
        vec![
            vec![
                Some(b"child".to_vec()),
                Some(b"fk_child_parent".to_vec()),
                Some(b"parent_id".to_vec()),
                Some(b"1".to_vec()),
                Some(b"1".to_vec()),
                Some(b"metadata".to_vec()),
                Some(b"parent".to_vec()),
                Some(b"id".to_vec()),
            ],
            vec![
                Some(b"child".to_vec()),
                Some(b"PRIMARY".to_vec()),
                Some(b"cid".to_vec()),
                Some(b"1".to_vec()),
                None,
                None,
                None,
                None,
            ],
            vec![
                Some(b"parent".to_vec()),
                Some(b"PRIMARY".to_vec()),
                Some(b"id".to_vec()),
                Some(b"1".to_vec()),
                None,
                None,
                None,
                None,
            ],
            vec![
                Some(b"parent".to_vec()),
                Some(b"uk_code".to_vec()),
                Some(b"code".to_vec()),
                Some(b"1".to_vec()),
                None,
                None,
                None,
                None,
            ],
        ]
    );
    // The plain index on `child.label` constrains nothing, so MySQL gives it
    // no row here even though it has one in `STATISTICS`.
    assert!(result
        .rows
        .iter()
        .all(|row| row[1] != Some(b"idx_label".to_vec())));
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (
                column.schema.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.character_set,
                column.flags,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "tables",
                "TABLE_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "",
                "CONSTRAINT_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                0,
            ),
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "",
                "COLUMN_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                0,
            ),
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "",
                "ORDINAL_POSITION",
                MYSQL_TYPE_LONG,
                10,
                MYSQL_BINARY_COLLATION,
                MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG,
            ),
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "",
                "POSITION_IN_UNIQUE_CONSTRAINT",
                MYSQL_TYPE_LONG,
                10,
                MYSQL_BINARY_COLLATION,
                MYSQL_UNSIGNED_FLAG,
            ),
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "",
                "REFERENCED_TABLE_SCHEMA",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                MYSQL_BINARY_FLAG,
            ),
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "",
                "REFERENCED_TABLE_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                MYSQL_BINARY_FLAG,
            ),
            (
                "information_schema",
                "KEY_COLUMN_USAGE",
                "",
                "REFERENCED_COLUMN_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                45,
                0,
            ),
        ]
    );

    // Reading only the foreign keys is what a migration tool does, and it is
    // a `WHERE` no recognizer of written shapes ever answered.
    let CommandExecutionResult::ResultSet(keys) = adapter
        .execute_query(
            "SELECT CONSTRAINT_NAME, REFERENCED_TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE REFERENCED_TABLE_NAME IS NOT NULL",
        )
        .unwrap()
    else {
        panic!("information_schema.KEY_COLUMN_USAGE must return a result set");
    };
    assert_eq!(
        keys.rows,
        vec![vec![
            Some(b"fk_child_parent".to_vec()),
            Some(b"parent".to_vec()),
        ]]
    );

    // A wildcard is refused over every one of these tables. This is the only
    // one that answers all of MySQL's columns, so the row would be the right
    // width — but a query that names its columns is answered either way, and
    // one rule for all of them is worth more than that.
    assert!(adapter
        .execute_query("SELECT * FROM information_schema.KEY_COLUMN_USAGE")
        .is_err());
}

#[cfg(unix)]
#[test]
fn information_schema_constraint_tables_name_each_key_and_its_rules() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("metadata").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([53; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("metadata").unwrap();
    for sql in [
        "CREATE TABLE parent (id INT NOT NULL PRIMARY KEY, code VARCHAR(32) NOT NULL)",
        "CREATE UNIQUE INDEX uk_code ON parent (code)",
        "CREATE TABLE child (cid INT NOT NULL PRIMARY KEY, parent_id INT, parent_code VARCHAR(32), label VARCHAR(64), CONSTRAINT fk_by_id FOREIGN KEY (parent_id) REFERENCES parent (id) ON DELETE CASCADE ON UPDATE SET NULL, CONSTRAINT fk_by_code FOREIGN KEY (parent_code) REFERENCES parent (code) ON DELETE RESTRICT)",
        "CREATE INDEX idx_label ON child (label)",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let CommandExecutionResult::ResultSet(constraints) = adapter
        .execute_query(
            "SELECT TABLE_NAME, CONSTRAINT_NAME, CONSTRAINT_TYPE, ENFORCED FROM information_schema.TABLE_CONSTRAINTS ORDER BY TABLE_NAME, CONSTRAINT_NAME",
        )
        .unwrap()
    else {
        panic!("information_schema.TABLE_CONSTRAINTS must return a result set");
    };
    assert_eq!(
        constraints.rows,
        vec![
            vec![
                Some(b"child".to_vec()),
                Some(b"fk_by_code".to_vec()),
                Some(b"FOREIGN KEY".to_vec()),
                Some(b"YES".to_vec()),
            ],
            vec![
                Some(b"child".to_vec()),
                Some(b"fk_by_id".to_vec()),
                Some(b"FOREIGN KEY".to_vec()),
                Some(b"YES".to_vec()),
            ],
            vec![
                Some(b"child".to_vec()),
                Some(b"PRIMARY".to_vec()),
                Some(b"PRIMARY KEY".to_vec()),
                Some(b"YES".to_vec()),
            ],
            vec![
                Some(b"parent".to_vec()),
                Some(b"PRIMARY".to_vec()),
                Some(b"PRIMARY KEY".to_vec()),
                Some(b"YES".to_vec()),
            ],
            vec![
                Some(b"parent".to_vec()),
                Some(b"uk_code".to_vec()),
                Some(b"UNIQUE".to_vec()),
                Some(b"YES".to_vec()),
            ],
        ]
    );
    assert_eq!(
        constraints
            .columns
            .iter()
            .map(|column| (
                column.schema.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "information_schema",
                "TABLE_CONSTRAINTS",
                "tables",
                "TABLE_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "information_schema",
                "TABLE_CONSTRAINTS",
                "",
                "CONSTRAINT_NAME",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
            ),
            (
                "information_schema",
                "TABLE_CONSTRAINTS",
                "",
                "CONSTRAINT_TYPE",
                MYSQL_TYPE_VAR_STRING,
                44,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            ),
            (
                "information_schema",
                "TABLE_CONSTRAINTS",
                "",
                "ENFORCED",
                MYSQL_TYPE_VAR_STRING,
                12,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            ),
        ]
    );

    let CommandExecutionResult::ResultSet(referential) = adapter
        .execute_query(
            "SELECT CONSTRAINT_NAME, UNIQUE_CONSTRAINT_NAME, MATCH_OPTION, UPDATE_RULE, DELETE_RULE, TABLE_NAME, REFERENCED_TABLE_NAME FROM information_schema.REFERENTIAL_CONSTRAINTS ORDER BY CONSTRAINT_NAME",
        )
        .unwrap()
    else {
        panic!("information_schema.REFERENTIAL_CONSTRAINTS must return a result set");
    };
    assert_eq!(
        referential.rows,
        vec![
            // `ON DELETE RESTRICT` reads back as written, and the `ON UPDATE`
            // it was not given reads back as NO ACTION.
            vec![
                Some(b"fk_by_code".to_vec()),
                Some(b"uk_code".to_vec()),
                Some(b"NONE".to_vec()),
                Some(b"NO ACTION".to_vec()),
                Some(b"RESTRICT".to_vec()),
                Some(b"child".to_vec()),
                Some(b"parent".to_vec()),
            ],
            vec![
                Some(b"fk_by_id".to_vec()),
                Some(b"PRIMARY".to_vec()),
                Some(b"NONE".to_vec()),
                Some(b"SET NULL".to_vec()),
                Some(b"CASCADE".to_vec()),
                Some(b"child".to_vec()),
                Some(b"parent".to_vec()),
            ],
        ]
    );
    // Measured: MySQL declares the rule columns as ENUMs, so they report the
    // string type where the names around them report a var_string.
    assert_eq!(
        referential
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.column_length
            ))
            .collect::<Vec<_>>(),
        vec![
            ("CONSTRAINT_NAME", MYSQL_TYPE_VAR_STRING, 256),
            ("UNIQUE_CONSTRAINT_NAME", MYSQL_TYPE_VAR_STRING, 256),
            ("MATCH_OPTION", MYSQL_TYPE_STRING, 28),
            ("UPDATE_RULE", MYSQL_TYPE_STRING, 44),
            ("DELETE_RULE", MYSQL_TYPE_STRING, 44),
            ("TABLE_NAME", MYSQL_TYPE_VAR_STRING, 256),
            ("REFERENCED_TABLE_NAME", MYSQL_TYPE_VAR_STRING, 256),
        ]
    );

    // Reading which keys cascade is a `WHERE` over a column no recognizer of
    // written shapes ever read.
    let CommandExecutionResult::ResultSet(cascading) = adapter
        .execute_query(
            "SELECT CONSTRAINT_NAME FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE DELETE_RULE = 'CASCADE'",
        )
        .unwrap()
    else {
        panic!("information_schema.REFERENTIAL_CONSTRAINTS must return a result set");
    };
    assert_eq!(cascading.rows, vec![vec![Some(b"fk_by_id".to_vec())]]);
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
            columns: information_schema_columns_columns(&[
                MySqlInformationSchemaColumnsColumn::ColumnName,
                MySqlInformationSchemaColumnsColumn::OrdinalPosition,
                MySqlInformationSchemaColumnsColumn::ColumnDefault,
                MySqlInformationSchemaColumnsColumn::IsNullable,
                MySqlInformationSchemaColumnsColumn::ColumnType,
                MySqlInformationSchemaColumnsColumn::ColumnKey,
                MySqlInformationSchemaColumnsColumn::Extra,
            ]),
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
            columns: information_schema_columns_columns(&[
                MySqlInformationSchemaColumnsColumn::ColumnName,
                MySqlInformationSchemaColumnsColumn::OrdinalPosition,
                MySqlInformationSchemaColumnsColumn::ColumnDefault,
                MySqlInformationSchemaColumnsColumn::IsNullable,
                MySqlInformationSchemaColumnsColumn::ColumnType,
                MySqlInformationSchemaColumnsColumn::ColumnKey,
                MySqlInformationSchemaColumnsColumn::Extra,
            ]),
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
            &[
                MySqlInformationSchemaColumnsColumn::ColumnName,
                MySqlInformationSchemaColumnsColumn::OrdinalPosition,
                MySqlInformationSchemaColumnsColumn::ColumnDefault,
                MySqlInformationSchemaColumnsColumn::IsNullable,
                MySqlInformationSchemaColumnsColumn::ColumnType,
                MySqlInformationSchemaColumnsColumn::ColumnKey,
                MySqlInformationSchemaColumnsColumn::Extra,
            ],
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
            &[
                MySqlInformationSchemaColumnsColumn::ColumnName,
                MySqlInformationSchemaColumnsColumn::OrdinalPosition,
                MySqlInformationSchemaColumnsColumn::ColumnDefault,
                MySqlInformationSchemaColumnsColumn::IsNullable,
                MySqlInformationSchemaColumnsColumn::ColumnType,
                MySqlInformationSchemaColumnsColumn::ColumnKey,
                MySqlInformationSchemaColumnsColumn::Extra,
            ],
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
            &[
                MySqlInformationSchemaColumnsColumn::ColumnName,
                MySqlInformationSchemaColumnsColumn::OrdinalPosition,
                MySqlInformationSchemaColumnsColumn::ColumnDefault,
                MySqlInformationSchemaColumnsColumn::IsNullable,
                MySqlInformationSchemaColumnsColumn::ColumnType,
                MySqlInformationSchemaColumnsColumn::ColumnKey,
                MySqlInformationSchemaColumnsColumn::Extra,
            ],
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
            &[
                MySqlInformationSchemaColumnsColumn::ColumnName,
                MySqlInformationSchemaColumnsColumn::OrdinalPosition,
                MySqlInformationSchemaColumnsColumn::ColumnDefault,
                MySqlInformationSchemaColumnsColumn::IsNullable,
                MySqlInformationSchemaColumnsColumn::ColumnType,
                MySqlInformationSchemaColumnsColumn::ColumnKey,
                MySqlInformationSchemaColumnsColumn::Extra,
            ],
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
        // A column MySQL has and this does not answer.
        "SELECT COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'records'",
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
            &[
                MySqlInformationSchemaTablesColumn::TableSchema,
                MySqlInformationSchemaTablesColumn::TableName,
                MySqlInformationSchemaTablesColumn::TableType,
            ],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
    assert_eq!(
        information_schema_tables_result_to_execution_result(
            &"x".repeat(MAX_TEXT_ROW_VALUE_LENGTH - 19),
            tables.clone(),
            &[
                MySqlInformationSchemaTablesColumn::TableSchema,
                MySqlInformationSchemaTablesColumn::TableName,
                MySqlInformationSchemaTablesColumn::TableType,
            ],
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
            &[
                MySqlInformationSchemaTablesColumn::TableSchema,
                MySqlInformationSchemaTablesColumn::TableName,
                MySqlInformationSchemaTablesColumn::TableType,
            ],
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
            columns: vec![show_tables_column("reports", None)],
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
            None,
            vec![String::new(); MAX_DISPATCH_RESULT_ROWS + 1],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
    assert_eq!(
        show_tables_result_to_execution_result(
            "reports",
            None,
            vec!["x".repeat(MAX_TEXT_ROW_VALUE_LENGTH + 1)],
            SERVER_STATUS_AUTOCOMMIT,
        ),
        Err(FrontendErrorKind::Internal)
    );
    assert_eq!(
        show_tables_result_to_execution_result(
            "reports",
            None,
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

#[cfg(unix)]
#[test]
fn text_and_blob_sizes_metadata_show_create_and_columns_match_mysql() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([55; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    adapter
        .execute_query(
            "CREATE TABLE t (\
             id INT NOT NULL PRIMARY KEY, \
             a TINYTEXT, b TEXT, c MEDIUMTEXT, d LONGTEXT, \
             e TINYBLOB, f BLOB, g MEDIUMBLOB, h LONGBLOB\
             )",
        )
        .unwrap();

    let CommandExecutionResult::ResultSet(create_table_result) =
        adapter.execute_query("SHOW CREATE TABLE t").unwrap()
    else {
        panic!("SHOW CREATE TABLE must return a result set");
    };
    let [row] = create_table_result.rows.as_slice() else {
        panic!("SHOW CREATE TABLE must return exactly one row");
    };
    assert_eq!(row[0], Some(b"t".to_vec()));
    assert_eq!(
        String::from_utf8(row[1].clone().unwrap()).unwrap(),
        concat!(
            "CREATE TABLE `t` (\n",
            "  `id` int NOT NULL,\n",
            "  `a` tinytext,\n",
            "  `b` text,\n",
            "  `c` mediumtext,\n",
            "  `d` longtext,\n",
            "  `e` tinyblob,\n",
            "  `f` blob,\n",
            "  `g` mediumblob,\n",
            "  `h` longblob,\n",
            "  PRIMARY KEY (`id`)\n",
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );

    let CommandExecutionResult::ResultSet(columns_result) =
        adapter.execute_query("SHOW COLUMNS FROM t").unwrap()
    else {
        panic!("SHOW COLUMNS must return a result set");
    };
    assert_eq!(
        columns_result
            .rows
            .iter()
            .map(|row| (
                row[0].as_deref().map(std::str::from_utf8).unwrap().unwrap(),
                row[1].as_deref().map(std::str::from_utf8).unwrap().unwrap(),
                row[2].as_deref().map(std::str::from_utf8).unwrap().unwrap(),
                row[3].as_deref().map(std::str::from_utf8).unwrap().unwrap(),
                row[4].as_deref().map(std::str::from_utf8).transpose().unwrap(),
                row[5].as_deref().map(std::str::from_utf8).unwrap().unwrap(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("id", "int", "NO", "PRI", None, ""),
            ("a", "tinytext", "YES", "", None, ""),
            ("b", "text", "YES", "", None, ""),
            ("c", "mediumtext", "YES", "", None, ""),
            ("d", "longtext", "YES", "", None, ""),
            ("e", "tinyblob", "YES", "", None, ""),
            ("f", "blob", "YES", "", None, ""),
            ("g", "mediumblob", "YES", "", None, ""),
            ("h", "longblob", "YES", "", None, ""),
        ]
    );

    adapter
        .execute_query("INSERT INTO t (id, a, b, c, d) VALUES (1, 'x', 'x', 'x', 'x')")
        .unwrap();

    let CommandExecutionResult::ResultSet(select_result) =
        adapter.execute_query("SELECT a, b, c, d, e, f, g, h FROM t").unwrap()
    else {
        panic!("SELECT must return a result set");
    };

    let expected = [
        ("a", MYSQL_TYPE_BLOB, u16::from(DEFAULT_UTF8MB4_COLLATION), 1_020u32, 0u8, MYSQL_BLOB_FLAG),
        ("b", MYSQL_TYPE_BLOB, u16::from(DEFAULT_UTF8MB4_COLLATION), 262_140u32, 0u8, MYSQL_BLOB_FLAG),
        ("c", MYSQL_TYPE_BLOB, u16::from(DEFAULT_UTF8MB4_COLLATION), 67_108_860u32, 0u8, MYSQL_BLOB_FLAG),
        ("d", MYSQL_TYPE_BLOB, u16::from(DEFAULT_UTF8MB4_COLLATION), u32::MAX, 0u8, MYSQL_BLOB_FLAG),
        ("e", MYSQL_TYPE_BLOB, MYSQL_BINARY_COLLATION, 255u32, 0u8, MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG),
        ("f", MYSQL_TYPE_BLOB, MYSQL_BINARY_COLLATION, 65_535u32, 0u8, MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG),
        ("g", MYSQL_TYPE_BLOB, MYSQL_BINARY_COLLATION, 16_777_215u32, 0u8, MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG),
        ("h", MYSQL_TYPE_BLOB, MYSQL_BINARY_COLLATION, u32::MAX, 0u8, MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG),
    ];

    assert_eq!(select_result.columns.len(), 8);
    let codec = PacketCodec::new(4096).unwrap();
    for (index, expected_col) in expected.iter().enumerate() {
        let col = &select_result.columns[index];
        assert_eq!(col.name, expected_col.0);
        assert_eq!(col.original_name, expected_col.0);
        assert_eq!(col.table, "t");
        assert_eq!(col.original_table, "t");
        assert_eq!(col.schema, "reports");
        assert_eq!(col.column_type, expected_col.1);
        assert_eq!(col.character_set, expected_col.2);
        assert_eq!(col.column_length, expected_col.3);
        assert_eq!(col.decimals, expected_col.4);
        assert_eq!(col.flags, expected_col.5);

        let frame = col.encode(codec, (index + 1) as u8).unwrap();
        let decoded = crate::ColumnDefinitionPacket::decode(codec, &frame).unwrap();
        assert_eq!(decoded.name, expected_col.0);
        assert_eq!(decoded.column_type, expected_col.1);
        assert_eq!(decoded.character_set, expected_col.2);
        assert_eq!(decoded.column_length, expected_col.3);
        assert_eq!(decoded.flags, expected_col.5);
    }

    assert_eq!(
        select_result.rows,
        vec![vec![
            Some(b"x".to_vec()),
            Some(b"x".to_vec()),
            Some(b"x".to_vec()),
            Some(b"x".to_vec()),
            None,
            None,
            None,
            None,
        ]]
    );
}

#[cfg(unix)]
#[test]
fn dml_order_by_and_limit_updates_and_deletes_expected_rows() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([56; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();

    adapter
        .execute_query("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(20))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t (id, n, name) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c'), (4, 40, 'd')")
        .unwrap();

    // DELETE with WHERE, ORDER BY, and LIMIT 2: deletes id 1 and 2
    let CommandExecutionResult::Ok(del1) = adapter
        .execute_query("DELETE FROM t WHERE n > 5 ORDER BY id LIMIT 2")
        .unwrap()
    else {
        panic!("DELETE must return Ok packet");
    };
    assert_eq!(del1.affected_rows, 2);

    let CommandExecutionResult::ResultSet(res1) = adapter
        .execute_query("SELECT id, n FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return ResultSet");
    };
    assert_eq!(
        res1.rows,
        vec![
            vec![Some(b"3".to_vec()), Some(b"30".to_vec())],
            vec![Some(b"4".to_vec()), Some(b"40".to_vec())],
        ]
    );

    // DELETE with ORDER BY DESC and LIMIT 1: deletes id 4
    let CommandExecutionResult::Ok(del2) = adapter
        .execute_query("DELETE FROM t ORDER BY id DESC LIMIT 1")
        .unwrap()
    else {
        panic!("DELETE must return Ok packet");
    };
    assert_eq!(del2.affected_rows, 1);

    let CommandExecutionResult::ResultSet(res2) = adapter
        .execute_query("SELECT id, n FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return ResultSet");
    };
    assert_eq!(
        res2.rows,
        vec![vec![Some(b"3".to_vec()), Some(b"30".to_vec())]]
    );

    // UPDATE with ORDER BY and LIMIT 1: updates id 3
    let CommandExecutionResult::Ok(upd1) = adapter
        .execute_query("UPDATE t SET n = 0 ORDER BY id LIMIT 1")
        .unwrap()
    else {
        panic!("UPDATE must return Ok packet");
    };
    assert_eq!(upd1.affected_rows, 1);

    let CommandExecutionResult::ResultSet(res3) = adapter
        .execute_query("SELECT id, n FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return ResultSet");
    };
    assert_eq!(
        res3.rows,
        vec![vec![Some(b"3".to_vec()), Some(b"0".to_vec())]]
    );

    // DELETE with ORDER BY without LIMIT: deletes remaining id 3
    let CommandExecutionResult::Ok(del3) = adapter
        .execute_query("DELETE FROM t ORDER BY id")
        .unwrap()
    else {
        panic!("DELETE must return Ok packet");
    };
    assert_eq!(del3.affected_rows, 1);

    let CommandExecutionResult::ResultSet(res4) = adapter
        .execute_query("SELECT id FROM t")
        .unwrap()
    else {
        panic!("SELECT must return ResultSet");
    };
    assert!(res4.rows.is_empty());

    // Non-PK ORDER BY column
    adapter
        .execute_query("CREATE TABLE t2 (id INT NOT NULL PRIMARY KEY, val INT)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO t2 (id, val) VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();

    let CommandExecutionResult::Ok(t2_del) = adapter
        .execute_query("DELETE FROM t2 WHERE val > 5 ORDER BY val DESC LIMIT 1")
        .unwrap()
    else {
        panic!("DELETE must return Ok packet");
    };
    assert_eq!(t2_del.affected_rows, 1);

    let CommandExecutionResult::ResultSet(t2_res) = adapter
        .execute_query("SELECT id, val FROM t2 ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return ResultSet");
    };
    assert_eq!(
        t2_res.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"10".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"20".to_vec())],
        ]
    );

    // Rejection tests:
    // Bare LIMIT without ORDER BY is rejected as unsupported
    assert_eq!(
        adapter.execute_query("DELETE FROM t LIMIT 1"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("UPDATE t SET n = 0 LIMIT 1"),
        Err(FrontendErrorKind::Unsupported)
    );
    // Ordinal in ORDER BY is rejected
    assert_eq!(
        adapter.execute_query("DELETE FROM t ORDER BY 1 LIMIT 1"),
        Err(FrontendErrorKind::Unsupported)
    );
    // Non-integer (text) column in ORDER BY is rejected by catalog validation
    assert_eq!(
        adapter.execute_query("DELETE FROM t ORDER BY name LIMIT 1"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("UPDATE t SET n = 0 ORDER BY name LIMIT 1"),
        Err(FrontendErrorKind::Unsupported)
    );

    // Prepared statement tests:
    let prep_del = adapter
        .execute_stmt_prepare("DELETE FROM t2 WHERE val > ? ORDER BY id LIMIT 1")
        .unwrap();
    assert_eq!(prep_del.parameters.len(), 1);

    let prep_upd = adapter
        .execute_stmt_prepare("UPDATE t2 SET val = ? WHERE id = ? ORDER BY id LIMIT 1")
        .unwrap();
    assert_eq!(prep_upd.parameters.len(), 2);

    // Prepared statement text column in ORDER BY is rejected
    assert_eq!(
        adapter.execute_stmt_prepare("DELETE FROM t ORDER BY name LIMIT 1"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// A `WHERE` comparison against a column held in a canonical form of its own.
///
/// Every row below is the row MySQL 8.4.11 answers for the same table and the
/// same statement, recorded in the pinned golden
/// `select-temporal-comparison.json`.
#[cfg(unix)]
#[test]
fn a_where_compares_a_temporal_or_real_column_the_way_mysql_compares_it() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([54; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE moments (id INT NOT NULL PRIMARY KEY, d DATE, dt DATETIME, ts TIMESTAMP NULL, t TIME, y YEAR, money DECIMAL(10,2), ratio DOUBLE)",
        "INSERT INTO moments (id, d, dt, ts, t, y, money, ratio) VALUES (1, '2024-01-01', '2024-01-01 00:00:00', '2024-01-01 00:00:00', '01:02:03', 2024, 1.50, 1.5)",
        "INSERT INTO moments (id, d, dt, ts, t, y, money, ratio) VALUES (2, '2024-06-15', '2024-06-15 12:30:45', '2024-06-15 12:30:45', '99:00:00', 1999, 200.25, 200.25)",
        "INSERT INTO moments (id, d, dt, ts, t, y, money, ratio) VALUES (3, '2023-12-31', '2023-12-31 23:59:59', '2023-12-31 23:59:59', '-01:00:00', 2155, 0.00, 0.0)",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM moments WHERE d = '2024-01-01' ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM moments WHERE d > '2024-01-01' ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM moments WHERE dt >= '2024-01-01 00:00:00' ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM moments WHERE ts = '2024-06-15 12:30:45' ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM moments WHERE t = '01:02:03' ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM moments WHERE y = 2024 ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM moments WHERE y >= 2024 ORDER BY id",
            vec!["1", "3"],
        ),
        (
            "SELECT id FROM moments WHERE money > 1 ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM moments WHERE money = 0 ORDER BY id",
            vec!["3"],
        ),
        (
            "SELECT id FROM moments WHERE ratio >= 2 ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM moments WHERE d IN ('2024-01-01', '2023-12-31') ORDER BY id",
            vec!["1", "3"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // Measured: MySQL reads this as the first of January and finds row 1.
        // The stored day is `2024-01-01`, so comparing against the text as
        // written would find nothing — it is refused rather than answered
        // differently.
        "SELECT id FROM moments WHERE d = '2024-1-1'",
        // Measured: MySQL reads a day compared to a moment as that day's
        // midnight, which comparing the stored text would not.
        "SELECT id FROM moments WHERE dt >= '2024-01-01'",
        // Measured: MySQL reads 24 as 2024 and finds row 1, where the stored
        // year is the number 2024.
        "SELECT id FROM moments WHERE y = 24",
        // A span runs past a day and carries a sign, so reading two of them in
        // order is not reading them in time order. Only sameness is answered.
        "SELECT id FROM moments WHERE t > '02:00:00'",
        // A parameter carries no type until it is bound, and a bound value is
        // not put into the form the column holds.
        "SELECT id FROM moments WHERE d = ?",
    ] {
        assert!(
            adapter.execute_query(sql).is_err(),
            "a comparison this cannot answer the way MySQL does must be refused: {sql}"
        );
    }
}

/// A `WHERE` comparison against an `ENUM` or a `SET` column.
///
/// Every row below is the row MySQL 8.4.11 answers for the same table and the
/// same statement, recorded in the pinned golden
/// `select-member-comparison.json`.
#[cfg(unix)]
#[test]
fn a_where_compares_a_member_column_the_way_mysql_compares_it() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([55; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE tickets (id INT NOT NULL PRIMARY KEY, state ENUM('pending','active','closed'), rights SET('read','write','exec'))",
        "INSERT INTO tickets (id, state, rights) VALUES (1, 'pending', 'read'), (2, 'active', 'read,write'), (3, 'closed', 'exec')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM tickets WHERE state = 'active' ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM tickets WHERE state <> 'active' ORDER BY id",
            vec!["1", "3"],
        ),
        (
            "SELECT id FROM tickets WHERE state IN ('pending', 'closed') ORDER BY id",
            vec!["1", "3"],
        ),
        (
            "SELECT id FROM tickets WHERE rights = 'read' ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM tickets WHERE rights = 'read,write' ORDER BY id",
            vec!["2"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // Measured: MySQL's collation ignores case, so this finds row 2 there.
        // The member is held under the spelling it was declared with, so
        // comparing what is stored against this text would find nothing.
        "SELECT id FROM tickets WHERE state = 'ACTIVE'",
        // Measured: MySQL reads a number as the position a member was declared
        // in and finds row 2, where the stored value is the word.
        "SELECT id FROM tickets WHERE state = 2",
        // A word no member carries.
        "SELECT id FROM tickets WHERE state = 'gone'",
        // Measured: MySQL reads an ENUM in the order its members were
        // declared, which is not the order their words read in.
        "SELECT id FROM tickets WHERE state > 'active'",
        // A SET holds its members in the order they were declared, so this is
        // not the way any subset is held.
        "SELECT id FROM tickets WHERE rights = 'write,read'",
    ] {
        assert!(
            adapter.execute_query(sql).is_err(),
            "a comparison this cannot answer the way MySQL does must be refused: {sql}"
        );
    }
}

/// A `WHERE` comparison against a number written with a fraction.
///
/// Every row below is the row MySQL 8.4.11 answers for the same table and the
/// same statement, recorded in the pinned golden
/// `select-decimal-literal-comparison.json`.
#[cfg(unix)]
#[test]
fn a_where_compares_a_number_written_with_a_fraction() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([56; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE prices (id INT NOT NULL PRIMARY KEY, n INT, money DECIMAL(10,2), ratio DOUBLE, label VARCHAR(20))",
        "INSERT INTO prices (id, n, money, ratio, label) VALUES (1, 1, 1.10, 1.5, '2'), (2, 2, 9.99, 2.25, '10'), (3, 3, 100.00, 1000000.0, 'x')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM prices WHERE n > 1.5 ORDER BY id",
            vec!["2", "3"],
        ),
        ("SELECT id FROM prices WHERE n = 1.5 ORDER BY id", vec![]),
        ("SELECT id FROM prices WHERE n = 2.0 ORDER BY id", vec!["2"]),
        (
            "SELECT id FROM prices WHERE money = 9.99 ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM prices WHERE money >= 1.10 ORDER BY id",
            vec!["1", "2", "3"],
        ),
        (
            "SELECT id FROM prices WHERE money < 9.99 ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM prices WHERE ratio > 2.0 ORDER BY id",
            vec!["2", "3"],
        ),
        (
            "SELECT id FROM prices WHERE ratio >= 1e6 ORDER BY id",
            vec!["3"],
        ),
        (
            "SELECT id FROM prices WHERE money > -1.5 ORDER BY id",
            vec!["1", "2", "3"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    // Measured: MySQL reads the text as a number, so it answers rows 1 and 2
    // and raises a truncation warning for the row that is not one. Comparing
    // text against a number is refused here, which is what a string against an
    // integer column has always been.
    assert_eq!(
        adapter.execute_query("SELECT id FROM prices WHERE label > 1.5"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// A `WHERE` comparison against the moment the statement runs.
///
/// Every row below is the row MySQL 8.4.11 answers for the same table and the
/// same statement, recorded in the pinned golden `select-now-comparison.json`.
#[cfg(unix)]
#[test]
fn a_where_compares_the_moment_the_statement_runs() {
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
        .execute_query(
            "CREATE TABLE events (id INT NOT NULL PRIMARY KEY, d DATE, dt DATETIME, n INT)",
        )
        .unwrap();
    // A call is not yet taken as a value to insert, so today is read out first
    // and written in. Reading it here also keeps the test the same on any day.
    let CommandExecutionResult::ResultSet(today) =
        adapter.execute_query("SELECT CURDATE(), NOW()").unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    let day = String::from_utf8(today.rows[0][0].clone().unwrap()).unwrap();
    let moment = String::from_utf8(today.rows[0][1].clone().unwrap()).unwrap();
    for sql in [
        format!("INSERT INTO events (id, d, dt, n) VALUES (1, '{day}', '{moment}', 1)"),
        "INSERT INTO events (id, d, dt, n) VALUES (2, '2000-01-01', '2000-01-01 00:00:00', 2)"
            .to_owned(),
    ] {
        adapter.execute_query(&sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM events WHERE d = CURDATE() ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM events WHERE d < CURDATE() ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM events WHERE d >= CURRENT_DATE ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM events WHERE dt <= NOW() ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM events WHERE dt > CURRENT_TIMESTAMP ORDER BY id",
            vec![],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // Measured: MySQL reads today compared to a moment as this morning's
        // midnight, which comparing the stored text would not — it finds no
        // row here only because no row holds midnight.
        "SELECT id FROM events WHERE dt = CURDATE()",
        // A reading of the moment meets the column whose form it answers in
        // and no other.
        "SELECT id FROM events WHERE d = NOW()",
        "SELECT id FROM events WHERE n = CURDATE()",
        // A call this does not read is still refused rather than rendered.
        "SELECT id FROM events WHERE d = MAKEDATE(2024, 1)",
    ] {
        assert!(
            adapter.execute_query(sql).is_err(),
            "a comparison this cannot answer the way MySQL does must be refused: {sql}"
        );
    }
}

/// Writing the moment the statement runs, and letting the column it lands in
/// put it into the form that column holds.
///
/// Every answer below is the one MySQL 8.4.11 gives for the same table and the
/// same statements, recorded in the pinned golden `insert-now-value.json`.
#[cfg(unix)]
#[test]
fn a_written_value_can_be_the_moment_the_statement_runs() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([58; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE marks (id INT NOT NULL PRIMARY KEY, d DATE, dt DATETIME, ts TIMESTAMP NULL, tm TIME, n BIGINT, label VARCHAR(30))",
        "INSERT INTO marks (id, d, dt) VALUES (1, CURDATE(), NOW())",
        "INSERT INTO marks (id, d, dt) VALUES (2, NOW(), CURDATE())",
        "INSERT INTO marks (id, ts, tm) VALUES (3, NOW(), CURTIME())",
        "INSERT INTO marks SET id = 4, label = NOW()",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    fn first_column(outcome: Result<CommandExecutionResult, FrontendErrorKind>) -> Vec<String> {
        let CommandExecutionResult::ResultSet(result) =
            outcome.unwrap_or_else(|error| panic!("{error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect()
    }

    // Measured: a moment written into a day keeps the day — MySQL raises 1292
    // for the time it drops and this drops it quietly, which is the one
    // difference — and a day written into a moment becomes that day's
    // midnight.
    assert_eq!(
        first_column(adapter.execute_query("SELECT id FROM marks WHERE d = CURDATE() ORDER BY id")),
        vec!["1", "2"]
    );
    // Today compared against a moment is refused, so the midnight is read by
    // writing today out and asking for that moment exactly.
    let day = first_column(adapter.execute_query("SELECT CURDATE()")).remove(0);
    assert_eq!(
        first_column(adapter.execute_query(&format!(
            "SELECT id FROM marks WHERE dt = '{day} 00:00:00' ORDER BY id"
        ))),
        vec!["2"]
    );
    assert_eq!(
        first_column(adapter.execute_query("SELECT id FROM marks WHERE ts <= NOW() ORDER BY id")),
        vec!["3"]
    );
    // Measured: a moment written into a word is the moment written out, which
    // is nineteen characters.
    assert_eq!(
        first_column(adapter.execute_query("SELECT CHAR_LENGTH(label) FROM marks WHERE id = 4")),
        vec!["19"]
    );

    adapter
        .execute_query("UPDATE marks SET dt = NOW() WHERE id = 2")
        .unwrap();
    assert_eq!(
        first_column(adapter.execute_query(&format!(
            "SELECT id FROM marks WHERE dt = '{day} 00:00:00' ORDER BY id"
        ))),
        Vec::<String>::new()
    );

    // A time of day written into a TIME column reads back as the time of day,
    // which is what `CURTIME()` answers.
    assert_eq!(
        first_column(
            adapter.execute_query("SELECT id FROM marks WHERE tm IS NOT NULL ORDER BY id")
        ),
        vec!["3"]
    );

    // Measured: MySQL runs the moment together into a number and stores
    // 20260908170430. The engine writes the moment as text, which a number
    // column refuses, so this is refused rather than stored as something else.
    assert_eq!(
        adapter.execute_query("INSERT INTO marks (id, n) VALUES (5, NOW())"),
        Err(FrontendErrorKind::IncorrectValue)
    );
    // A word too narrow for the moment is refused the way any oversized value
    // is, which is what MySQL answers 1406 for.
    adapter
        .execute_query("CREATE TABLE narrow (id INT NOT NULL PRIMARY KEY, s VARCHAR(5))")
        .unwrap();
    assert_eq!(
        adapter.execute_query("INSERT INTO narrow (id, s) VALUES (1, NOW())"),
        Err(FrontendErrorKind::DataTooLong)
    );
    // A call this does not read is still refused rather than rendered.
    assert!(adapter
        .execute_query("INSERT INTO marks (id, d) VALUES (6, MAKEDATE(2024, 1))")
        .is_err());
}

/// Counting a column up, and the arithmetic around it.
///
/// Every answer below is the one MySQL 8.4.11 gives for the same table and the
/// same statements, recorded in the pinned golden
/// `update-arithmetic-assignment.json`.
#[cfg(unix)]
#[test]
fn an_update_assigns_arithmetic_over_the_row_it_changes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([59; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE counters (id INT NOT NULL PRIMARY KEY, a INT, b INT, big BIGINT, money DECIMAL(10,2))",
        "INSERT INTO counters (id, a, b, big, money) VALUES (1, 1, 10, 9223372036854775807, 1.50)",
        "INSERT INTO counters (id, a, b, big, money) VALUES (2, NULL, 20, 1, 2.25)",
        "UPDATE counters SET a = a + 1 WHERE id = 1",
        "UPDATE counters SET a = a + 1 WHERE id = 2",
        "UPDATE counters SET b = b * 2 - 5 WHERE id = 1",
        "UPDATE counters SET b = a + b WHERE id = 1",
        "UPDATE counters SET money = money + 1.5 WHERE id = 1",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let CommandExecutionResult::ResultSet(read) = adapter
        .execute_query("SELECT id, a, b, money FROM counters ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        read.rows,
        vec![
            vec![
                Some(b"1".to_vec()),
                Some(b"2".to_vec()),
                Some(b"17".to_vec()),
                Some(b"3.00".to_vec()),
            ],
            // Measured: counting up from nothing leaves nothing.
            vec![
                Some(b"2".to_vec()),
                None,
                Some(b"20".to_vec()),
                Some(b"2.25".to_vec()),
            ],
        ]
    );

    // Measured: MySQL answers 1690 for a whole number counted past its range,
    // and the value stays where it was.
    assert!(adapter
        .execute_query("UPDATE counters SET big = big + 1 WHERE id = 1")
        .is_err());
    let CommandExecutionResult::ResultSet(unchanged) = adapter
        .execute_query("SELECT big FROM counters WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        unchanged.rows,
        vec![vec![Some(b"9223372036854775807".to_vec())]]
    );

    for sql in [
        // Measured: MySQL reads the column this statement has already
        // assigned, so `b` is left at 100 there and would be left at the old
        // `a` here. Refused rather than answered differently.
        "UPDATE counters SET a = 100, b = a WHERE id = 2",
        "UPDATE counters SET a = 100, b = counters.a WHERE id = 2",
        // Measured: `b / 2` over 101 answers 50.5 in MySQL and 50 in the
        // engine, so the two would write different numbers.
        "UPDATE counters SET b = b / 2 WHERE id = 2",
        // A call this does not read is still refused rather than rendered.
        "UPDATE counters SET a = ABS(a) WHERE id = 2",
    ] {
        assert!(
            adapter.execute_query(sql).is_err(),
            "an assignment this cannot answer the way MySQL does must be refused: {sql}"
        );
    }

    // The other order is answered, because nothing reads what was assigned.
    adapter
        .execute_query("UPDATE counters SET b = a, a = 100 WHERE id = 2")
        .unwrap();
}

/// A paged query binds its row count, which is what a client that prepares one
/// writes.
#[test]
fn a_prepared_limit_and_offset_bind_their_row_counts() {
    let mut adapter = adapter();
    adapter
        .execute_query("CREATE TABLE paged (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    adapter
        .execute_query("INSERT INTO paged (id) VALUES (1), (2), (3), (4)")
        .unwrap();

    let count = |adapter: &mut MySqlCommandAdapter, sql: &str, bound: &[i64]| {
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        let mut payload = vec![0, 1];
        for _ in bound {
            payload.extend_from_slice(&[MYSQL_TYPE_LONGLONG, 0]);
        }
        for value in bound {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        adapter
            .execute_stmt_execute(prepared.statement_id, &payload)
            .map(|result| {
                prepared_result_set(result)
                    .rows
                    .iter()
                    .map(|row| row[0].clone())
                    .collect::<Vec<_>>()
            })
    };

    assert_eq!(
        count(
            &mut adapter,
            "SELECT id FROM paged ORDER BY id LIMIT ?",
            &[2]
        )
        .unwrap(),
        vec![BinaryResultValue::Integer(1), BinaryResultValue::Integer(2)]
    );
    assert_eq!(
        count(
            &mut adapter,
            "SELECT id FROM paged ORDER BY id LIMIT ? OFFSET ?",
            &[2, 1]
        )
        .unwrap(),
        vec![BinaryResultValue::Integer(2), BinaryResultValue::Integer(3)]
    );
    // MySQL's other spelling writes the offset first, so the first parameter
    // bound is the offset and the second is the row count.
    assert_eq!(
        count(
            &mut adapter,
            "SELECT id FROM paged ORDER BY id LIMIT ?, ?",
            &[1, 2]
        )
        .unwrap(),
        vec![BinaryResultValue::Integer(2), BinaryResultValue::Integer(3)]
    );
    // A parameter in the WHERE and one in the LIMIT are bound in the order
    // they are written.
    assert_eq!(
        count(
            &mut adapter,
            "SELECT id FROM paged WHERE id > ? ORDER BY id LIMIT ?",
            &[1, 2]
        )
        .unwrap(),
        vec![BinaryResultValue::Integer(2), BinaryResultValue::Integer(3)]
    );

    // The engine reads a negative row count as no limit at all, where MySQL
    // refuses one, so a negative bound value is refused rather than answered
    // with every row.
    assert!(count(
        &mut adapter,
        "SELECT id FROM paged ORDER BY id LIMIT ?",
        &[-1]
    )
    .is_err());
}

/// Counting a joined table's rows, which is the shape a query for "how many
/// children does each parent have" takes.
///
/// A join has to qualify its columns, so `COUNT(p.id)` is the only way to
/// write it. Every row below is the row MySQL 8.4.11 answers for the same
/// tables and the same statement.
#[cfg(unix)]
#[test]
fn a_count_takes_the_qualified_column_a_join_has_to_write() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([60; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE authors (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))",
        "CREATE TABLE books (id INT NOT NULL PRIMARY KEY, author_id INT, title VARCHAR(20))",
        "INSERT INTO authors (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        "INSERT INTO books (id, author_id, title) VALUES (1, 1, 'x'), (2, 1, 'y'), (3, 2, 'z')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let CommandExecutionResult::ResultSet(counted) = adapter
        .execute_query(
            "SELECT a.id, COUNT(b.id) FROM authors a LEFT JOIN books b ON b.author_id = a.id GROUP BY a.id ORDER BY a.id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        counted.rows,
        vec![
            vec![Some(b"1".to_vec()), Some(b"2".to_vec())],
            vec![Some(b"2".to_vec()), Some(b"1".to_vec())],
            // The author with no books counts nothing, because the outer join
            // leaves the book's id NULL and a count skips a NULL.
            vec![Some(b"3".to_vec()), Some(b"0".to_vec())],
        ]
    );
    // MySQL names the column after the text that was written, qualifier and
    // all.
    assert_eq!(counted.columns[1].name, "COUNT(b.id)");
    // Measured on MySQL 8.4.11: a count is a non-null LONGLONG of length 21
    // whatever it counts, which is why a qualified column needs no type read
    // out of the table it belongs to.
    assert_eq!(counted.columns[1].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(counted.columns[1].column_length, 21);

    let CommandExecutionResult::ResultSet(distinct) = adapter
        .execute_query("SELECT COUNT(DISTINCT b.author_id) FROM books b")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(distinct.rows, vec![vec![Some(b"2".to_vec())]]);

    // The other aggregates still take a bare column: each answers its
    // argument's own type, which means reading the column the qualifier names
    // out of the table it belongs to.
    for sql in [
        "SELECT MIN(b.id) FROM books b",
        "SELECT SUM(b.id) FROM books b",
        "SELECT a.id, AVG(b.id) FROM authors a JOIN books b ON b.author_id = a.id GROUP BY a.id",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `SELECT ... FOR UPDATE` reads rows this session is about to change, and
/// takes a lock that really is held while it does.
///
/// The engine holds one write lock over the whole database rather than a lock
/// for each row, so the lock is stronger than the one MySQL takes: another
/// session is kept out of every table rather than out of these rows. It is a
/// lock all the same, which is what the statement asked for.
#[cfg(unix)]
#[test]
fn a_select_for_update_takes_a_lock_that_is_held() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("ledger").unwrap();
    let second_factory =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer);
    let mut one = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([61; 32]),
        ))
        .unwrap();
    one.authorize_connection().unwrap();
    one.execute_init_db("ledger").unwrap();
    for sql in [
        "CREATE TABLE accounts (id INT NOT NULL PRIMARY KEY, balance INT)",
        "INSERT INTO accounts (id, balance) VALUES (1, 100), (2, 200)",
    ] {
        one.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut two = second_factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([62; 32]),
        ))
        .unwrap();
    two.authorize_connection().unwrap();
    two.execute_init_db("ledger").unwrap();

    // Outside a transaction the lock would end with the statement that took
    // it, so none is taken and the other session is not kept out.
    one.execute_query("SELECT balance FROM accounts WHERE id = 1 FOR UPDATE")
        .unwrap();
    two.execute_query("UPDATE accounts SET balance = 300 WHERE id = 2")
        .unwrap();

    // A session waits for a lock another session holds, the way MySQL's does,
    // so the wait is cut short here rather than leaving the test to sit out
    // the fifty seconds MySQL starts with.
    two.execute_query("SET SESSION innodb_lock_wait_timeout = 1")
        .unwrap();

    // Inside one, the lock is held until the transaction ends.
    one.execute_query("START TRANSACTION").unwrap();
    let CommandExecutionResult::ResultSet(read) = one
        .execute_query("SELECT balance FROM accounts WHERE id = 1 FOR UPDATE")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(read.rows, vec![vec![Some(b"100".to_vec())]]);

    let waited = std::time::Instant::now();
    assert_eq!(
        two.execute_query("UPDATE accounts SET balance = 999 WHERE id = 1"),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    // It waited for the lock rather than answering the moment it found it
    // held, which is what MySQL does before it answers 1205.
    assert!(
        waited.elapsed() >= Duration::from_secs(1),
        "{:?}",
        waited.elapsed()
    );

    one.execute_query("UPDATE accounts SET balance = 150 WHERE id = 1")
        .unwrap();
    one.execute_query("COMMIT").unwrap();

    // Once the transaction ends the other session writes again, and the row
    // it reads is the one this session left.
    two.execute_query("UPDATE accounts SET balance = 400 WHERE id = 2")
        .unwrap();
    let CommandExecutionResult::ResultSet(after) = two
        .execute_query("SELECT balance FROM accounts WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(after.rows, vec![vec![Some(b"150".to_vec())]]);

    // `FOR SHARE` is MySQL's other spelling and takes the same lock, because
    // there is no weaker one to take.
    one.execute_query("START TRANSACTION").unwrap();
    one.execute_query("SELECT balance FROM accounts WHERE id = 1 FOR SHARE")
        .unwrap();
    assert_eq!(
        two.execute_query("UPDATE accounts SET balance = 998 WHERE id = 2"),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    one.execute_query("ROLLBACK").unwrap();

    // The options that say what to do when the lock is already held ask for
    // something one lock cannot answer, and so does naming which tables to
    // lock.
    for sql in [
        "SELECT balance FROM accounts FOR UPDATE NOWAIT",
        "SELECT balance FROM accounts FOR UPDATE SKIP LOCKED",
        "SELECT balance FROM accounts FOR UPDATE OF accounts",
    ] {
        assert!(one.execute_query(sql).is_err(), "{sql}");
    }
}

/// `LOCK TABLES` takes a lock that is held until `UNLOCK TABLES`, the way
/// MySQL's is.
///
/// The engine holds one write lock over the whole database rather than a lock
/// for each table, so this locks more than was asked for. It is a lock all the
/// same, which is the one thing the statement asks to be true.
#[cfg(unix)]
#[test]
fn lock_tables_holds_the_lock_until_it_is_unlocked() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("stock").unwrap();
    let second_factory =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer);
    let mut one = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([63; 32]),
        ))
        .unwrap();
    one.authorize_connection().unwrap();
    one.execute_init_db("stock").unwrap();
    for sql in [
        "CREATE TABLE items (id INT NOT NULL PRIMARY KEY, count INT)",
        "INSERT INTO items (id, count) VALUES (1, 10)",
    ] {
        one.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut two = second_factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([64; 32]),
        ))
        .unwrap();
    two.authorize_connection().unwrap();
    two.execute_init_db("stock").unwrap();
    two.execute_query("SET SESSION innodb_lock_wait_timeout = 1")
        .unwrap();

    // Nothing is held yet, so the other session writes.
    two.execute_query("UPDATE items SET count = 11 WHERE id = 1")
        .unwrap();

    one.execute_query("LOCK TABLES items WRITE").unwrap();
    assert_eq!(
        two.execute_query("UPDATE items SET count = 12 WHERE id = 1"),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    // The session holding the lock reads and writes through it.
    one.execute_query("UPDATE items SET count = 20 WHERE id = 1")
        .unwrap();

    // Ending the transaction would let go of the lock, so it is refused rather
    // than dropped quietly.
    for sql in ["START TRANSACTION", "COMMIT", "ROLLBACK"] {
        assert_eq!(
            one.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }

    one.execute_query("UNLOCK TABLES").unwrap();
    two.execute_query("UPDATE items SET count = 30 WHERE id = 1")
        .unwrap();
    let CommandExecutionResult::ResultSet(read) = two
        .execute_query("SELECT count FROM items WHERE id = 1")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(read.rows, vec![vec![Some(b"30".to_vec())]]);

    // A `READ` lock takes the same lock, because there is no weaker one, and
    // an `UNLOCK TABLES` holding nothing is answered the way MySQL answers it.
    one.execute_query("UNLOCK TABLES").unwrap();
    one.execute_query("LOCK TABLES items READ, items AS other READ")
        .unwrap();
    assert_eq!(
        two.execute_query("UPDATE items SET count = 40 WHERE id = 1"),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    // Locking again replaces what was held rather than stacking on it.
    one.execute_query("LOCK TABLES items WRITE").unwrap();
    one.execute_query("UNLOCK TABLES").unwrap();
    two.execute_query("UPDATE items SET count = 50 WHERE id = 1")
        .unwrap();

    // The forms that ask for something one write lock cannot answer.
    for sql in [
        "LOCK TABLES items READ LOCAL",
        "LOCK TABLES items LOW_PRIORITY WRITE",
        "LOCK INSTANCE FOR BACKUP",
        "LOCK TABLES items",
    ] {
        assert_eq!(
            one.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
}

/// A moment shifted by an interval, which is how a suite asks for the rows of
/// the last month.
///
/// Every shape and every row below is what MySQL 8.4.11 answers for the same
/// table and the same statements, recorded in the pinned golden
/// `select-shifted-moment.json`.
#[cfg(unix)]
#[test]
fn a_clock_reading_is_shifted_by_an_interval() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([65; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE marks (id INT NOT NULL PRIMARY KEY, d DATE, dt DATETIME)",
        "INSERT INTO marks (id, d, dt) VALUES (1, CURDATE(), NOW())",
        "INSERT INTO marks (id, d, dt) VALUES (2, DATE_SUB(CURDATE(), INTERVAL 10 DAY), DATE_SUB(NOW(), INTERVAL 10 DAY))",
        "INSERT INTO marks (id, d, dt) VALUES (3, '2000-01-01', '2000-01-01 00:00:00')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    // Measured: shifting a moment answers a DATETIME of 19 whatever the
    // interval named, shifting a day by whole days, months or years answers a
    // DATE of 10, and one carrying a time answers a DATETIME. Every one of
    // them is nullable where the reading itself is not.
    let CommandExecutionResult::ResultSet(shapes) = adapter
        .execute_query(
            "SELECT DATE_SUB(NOW(), INTERVAL 30 DAY), DATE_SUB(CURDATE(), INTERVAL 1 DAY), DATE_ADD(CURDATE(), INTERVAL 1 MONTH), DATE_ADD(CURDATE(), INTERVAL 1 HOUR)",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        shapes
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG),
        ]
    );

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM marks WHERE dt > DATE_SUB(NOW(), INTERVAL 30 DAY) ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM marks WHERE dt > DATE_SUB(NOW(), INTERVAL 7 DAY) ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM marks WHERE d >= DATE_SUB(CURDATE(), INTERVAL 30 DAY) ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM marks WHERE d < DATE_ADD(CURDATE(), INTERVAL 1 MONTH) ORDER BY id",
            vec!["1", "2", "3"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // A shift of a day answers a day, which a moment does not meet, and a
        // shift of a moment answers a moment, which a day does not.
        "SELECT id FROM marks WHERE dt = DATE_SUB(CURDATE(), INTERVAL 1 DAY)",
        "SELECT id FROM marks WHERE d = DATE_SUB(NOW(), INTERVAL 1 DAY)",
        // A time of day holds a span rather than a moment, so shifting one by
        // a month names nothing.
        "SELECT DATE_SUB(CURTIME(), INTERVAL 1 HOUR)",
        // A shift of anything but a clock reading still needs the column it
        // shifts.
        "SELECT DATE_SUB(MAKEDATE(2024, 1), INTERVAL 1 DAY)",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Writing a column out, and reading a whole number, a day or a moment out of
/// one.
///
/// Every shape and every value below is what MySQL 8.4.11 answers for the same
/// table and the same statement, recorded in the pinned golden
/// `select-cast.json`.
#[cfg(unix)]
#[test]
fn a_cast_answers_what_mysql_answers_for_the_targets_it_takes() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([66; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE readings (id INT NOT NULL PRIMARY KEY, n INT, big BIGINT, ratio DOUBLE, money DECIMAL(10,2), label VARCHAR(20), dt DATETIME, d DATE)",
        "INSERT INTO readings (id, n, big, ratio, money, label, dt, d) VALUES (1, 42, 9007199254740993, 1.5, 12.34, 'seven', '2024-06-15 12:30:45', '2024-06-15')",
        "INSERT INTO readings (id, n, big, ratio, money, label, dt, d) VALUES (2, -3, -1, -1.5, -0.05, 'eight', '2000-01-01 00:00:00', '2000-01-01')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let CommandExecutionResult::ResultSet(written) = adapter
        .execute_query(
            "SELECT CAST(n AS CHAR), CAST(big AS CHAR), CAST(dt AS CHAR), CAST(d AS CHAR) FROM readings ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        written.rows,
        vec![
            vec![
                Some(b"42".to_vec()),
                Some(b"9007199254740993".to_vec()),
                Some(b"2024-06-15 12:30:45".to_vec()),
                Some(b"2024-06-15".to_vec()),
            ],
            vec![
                Some(b"-3".to_vec()),
                Some(b"-1".to_vec()),
                Some(b"2000-01-01 00:00:00".to_vec()),
                Some(b"2000-01-01".to_vec()),
            ],
        ]
    );
    // Measured: the width is the column's own display width counted in the
    // four bytes utf8mb4 reserves for a character.
    assert_eq!(
        written
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_VAR_STRING, 44, 0),
            (MYSQL_TYPE_VAR_STRING, 80, 0),
            (MYSQL_TYPE_VAR_STRING, 76, 0),
            (MYSQL_TYPE_VAR_STRING, 40, 0),
        ]
    );

    // Measured: reading a whole number out rounds away from zero, where the
    // engine's own cast would cut the fraction off.
    let CommandExecutionResult::ResultSet(rounded) = adapter
        .execute_query(
            "SELECT CAST(ratio AS SIGNED), CAST(money AS SIGNED), CAST(n AS SIGNED) FROM readings ORDER BY id",
        )
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        rounded.rows,
        vec![
            vec![
                Some(b"2".to_vec()),
                Some(b"12".to_vec()),
                Some(b"42".to_vec()),
            ],
            vec![
                Some(b"-2".to_vec()),
                Some(b"0".to_vec()),
                Some(b"-3".to_vec()),
            ],
        ]
    );
    assert_eq!(
        rounded
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        // The numeric flag rides along with the type, the way it does for every
        // other number this answers. The golden cannot show it: the driver the
        // observations are recorded through does not report it.
        vec![(MYSQL_TYPE_LONGLONG, 21, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG); 3]
    );

    let CommandExecutionResult::ResultSet(moments) = adapter
        .execute_query("SELECT CAST(dt AS DATE), CAST(d AS DATETIME) FROM readings ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        moments.rows,
        vec![
            vec![
                Some(b"2024-06-15".to_vec()),
                Some(b"2024-06-15 00:00:00".to_vec()),
            ],
            vec![
                Some(b"2000-01-01".to_vec()),
                Some(b"2000-01-01 00:00:00".to_vec()),
            ],
        ]
    );
    assert_eq!(
        moments
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        vec![
            (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG),
            (MYSQL_TYPE_DATETIME, 19, MYSQL_BINARY_FLAG),
        ]
    );

    // `CONVERT` is the same thing spelled the other way.
    let CommandExecutionResult::ResultSet(converted) = adapter
        .execute_query("SELECT CONVERT(n, CHAR), CONVERT(ratio, SIGNED) FROM readings ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        converted.rows,
        vec![
            vec![Some(b"42".to_vec()), Some(b"2".to_vec())],
            vec![Some(b"-3".to_vec()), Some(b"-2".to_vec())],
        ]
    );
    assert_eq!(
        converted
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length))
            .collect::<Vec<_>>(),
        vec![(MYSQL_TYPE_VAR_STRING, 44), (MYSQL_TYPE_LONGLONG, 21)]
    );

    for sql in [
        // `CONVERT(col USING <charset>)` names a character set rather than a
        // type, and this server speaks one.
        "SELECT CONVERT(label USING utf8mb4) FROM readings",
        "SELECT CONVERT(n, UNSIGNED) FROM readings",
        // Measured: MySQL wraps a negative into an unsigned 64-bit number —
        // `CAST(-3 AS UNSIGNED)` answers 18446744073709551613 — and the engine
        // holds an integer as an i64.
        "SELECT CAST(n AS UNSIGNED) FROM readings",
        // Measured: a DECIMAL keeps its declared scale, so `1.50` is written
        // out as `1.50` there and would be `1.5` here.
        "SELECT CAST(money AS CHAR) FROM readings",
        // A DOUBLE prints by a rule of its own.
        "SELECT CAST(ratio AS CHAR) FROM readings",
        // Measured: cutting a word short warns, and the warning is not raised
        // here.
        "SELECT CAST(label AS CHAR(4)) FROM readings",
        "SELECT CAST(label AS SIGNED) FROM readings",
        "SELECT CAST(label AS DATE) FROM readings",
        // A DECIMAL carries a scale the engine does not keep.
        "SELECT CAST(n AS DECIMAL) FROM readings",
        "SELECT CAST(n AS DECIMAL(10,2)) FROM readings",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// A call standing where a column stands on the left of a comparison.
///
/// Every row below is the row MySQL 8.4.11 answers for the same table and the
/// same statement, recorded in the pinned golden `select-call-comparison.json`.
#[cfg(unix)]
#[test]
fn a_call_stands_on_the_left_of_a_comparison() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([67; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE people (id INT NOT NULL PRIMARY KEY, name VARCHAR(20), n INT, dt DATETIME, d DATE)",
        "INSERT INTO people (id, name, n, dt, d) VALUES (1, 'Ada', -3, '2024-06-15 12:30:45', '2024-06-15')",
        "INSERT INTO people (id, name, n, dt, d) VALUES (2, 'bob', 7, '2024-06-15 00:00:00', '2000-01-01')",
        "INSERT INTO people (id, name, n, dt, d) VALUES (3, 'CARL', 0, '2000-01-01 23:59:59', '2024-06-15')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM people WHERE LOWER(name) = 'ada' ORDER BY id",
            vec!["1"],
        ),
        // Measured: MySQL's collation ignores case after the call has answered,
        // so this finds the row holding `Ada` too.
        (
            "SELECT id FROM people WHERE LOWER(name) = 'ADA' ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM people WHERE UPPER(name) = 'BOB' ORDER BY id",
            vec!["2"],
        ),
        (
            "SELECT id FROM people WHERE LOWER(name) > 'b' ORDER BY id",
            vec!["2", "3"],
        ),
        (
            "SELECT id FROM people WHERE CHAR_LENGTH(name) > 3 ORDER BY id",
            vec!["3"],
        ),
        (
            "SELECT id FROM people WHERE YEAR(d) = 2024 ORDER BY id",
            vec!["1", "3"],
        ),
        (
            "SELECT id FROM people WHERE CAST(dt AS DATE) = '2024-06-15' ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM people WHERE CAST(n AS CHAR) = '7' ORDER BY id",
            vec!["2"],
        ),
        // The call reads the same on either side of the operator.
        (
            "SELECT id FROM people WHERE 'ada' = LOWER(name) ORDER BY id",
            vec!["1"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // A word is not a number, and a number is not a word.
        "SELECT id FROM people WHERE CHAR_LENGTH(name) = 'three'",
        "SELECT id FROM people WHERE LOWER(name) = 3",
        // A day is held to the form one is stored in, the way a DATE column is.
        "SELECT id FROM people WHERE CAST(dt AS DATE) = '2024-6-15'",
        // A parameter carries no type until it binds, and nothing puts it into
        // the form a day is held in.
        "SELECT id FROM people WHERE CAST(dt AS DATE) = ?",
        // A call this does not read is still refused rather than rendered.
        "SELECT id FROM people WHERE MAKEDATE(2024, 1) = '2024-01-01'",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `DATE(col)` reads the day out of a moment, which is the other spelling of
/// `CAST(col AS DATE)`.
///
/// Every shape and every row below is what MySQL 8.4.11 answers for the same
/// table and the same statement, recorded in the pinned golden
/// `select-date-of.json`.
#[cfg(unix)]
#[test]
fn the_day_is_read_out_of_a_moment_under_either_spelling() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([68; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE marks (id INT NOT NULL PRIMARY KEY, dt DATETIME, d DATE, ts TIMESTAMP NULL, n INT, name VARCHAR(20))",
        "INSERT INTO marks (id, dt, d, ts) VALUES (1, '2024-06-15 12:30:45', '2024-06-15', '2024-06-15 12:30:45')",
        "INSERT INTO marks (id, dt, d, ts) VALUES (2, '2024-06-15 00:00:00', '2000-01-01', '2000-01-01 00:00:00')",
        "INSERT INTO marks (id, dt) VALUES (3, '2000-01-01 23:59:59')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let CommandExecutionResult::ResultSet(days) = adapter
        .execute_query("SELECT DATE(dt), DATE(d), DATE(ts) FROM marks ORDER BY id")
        .unwrap()
    else {
        panic!("SELECT must return a result set");
    };
    assert_eq!(
        days.rows,
        vec![
            vec![
                Some(b"2024-06-15".to_vec()),
                Some(b"2024-06-15".to_vec()),
                Some(b"2024-06-15".to_vec()),
            ],
            vec![
                Some(b"2024-06-15".to_vec()),
                Some(b"2000-01-01".to_vec()),
                Some(b"2000-01-01".to_vec()),
            ],
            vec![Some(b"2000-01-01".to_vec()), None, None],
        ]
    );
    // Measured: the same shape `CAST(col AS DATE)` answers, which is what makes
    // the two one thing.
    assert_eq!(
        days.columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        vec![(MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG); 3]
    );

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM marks WHERE DATE(dt) = '2024-06-15' ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM marks WHERE DATE(dt) < '2024-01-01' ORDER BY id",
            vec!["3"],
        ),
        (
            "SELECT id FROM marks WHERE DATE(d) = '2000-01-01' ORDER BY id",
            vec!["2"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // A day is held to the form one is stored in, the way a DATE column is.
        "SELECT id FROM marks WHERE DATE(dt) = '2024-6-15'",
        // The day of something that holds none.
        "SELECT DATE(n) FROM marks",
        "SELECT DATE(name) FROM marks",
        // `TIME(col)` is the other half of this and is not read: a TIME holds a
        // span running past a day, which the engine's reader answers NULL for.
        "SELECT TIME(dt) FROM marks",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// A `LIKE` whose pattern is bound, which is how a client that prepares a
/// search writes one.
#[test]
fn a_like_binds_its_pattern() {
    let mut adapter = adapter();
    adapter
        .execute_query("CREATE TABLE names (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))")
        .unwrap();
    adapter
        .execute_query("INSERT INTO names (id, name) VALUES (1, 'Ada'), (2, 'bob'), (3, 'Adam')")
        .unwrap();

    let mut matched = |sql: &str, pattern: &[u8]| {
        let prepared = adapter.execute_stmt_prepare(sql).unwrap();
        let mut payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0, pattern.len() as u8];
        payload.extend_from_slice(pattern);
        adapter
            .execute_stmt_execute(prepared.statement_id, &payload)
            .map(|result| {
                prepared_result_set(result)
                    .rows
                    .iter()
                    .map(|row| row[0].clone())
                    .collect::<Vec<_>>()
            })
    };

    // The engine already matches a pattern without regard to ASCII case, which
    // is what MySQL's default collation does, so a bound pattern needs no
    // collation of its own.
    assert_eq!(
        matched("SELECT id FROM names WHERE name LIKE ? ORDER BY id", b"ad%").unwrap(),
        vec![BinaryResultValue::Integer(1), BinaryResultValue::Integer(3)]
    );
    assert_eq!(
        matched("SELECT id FROM names WHERE name LIKE ? ORDER BY id", b"%b%").unwrap(),
        vec![BinaryResultValue::Integer(2)]
    );
    assert_eq!(
        matched(
            "SELECT id FROM names WHERE name NOT LIKE ? ORDER BY id",
            b"ad%"
        )
        .unwrap(),
        vec![BinaryResultValue::Integer(2)]
    );

    // MySQL reads a backslash in a pattern as an escape and the engine reads it
    // as itself, so a bound pattern carrying one is refused the way a written
    // one is.
    assert!(matched(
        "SELECT id FROM names WHERE name LIKE ? ORDER BY id",
        b"a\\%"
    )
    .is_err());
}

/// `(a, b) IN ((1, 'x'), (2, 'y'))`, which is how a lookup by a key of more
/// than one column is written.
///
/// Every row below is the row MySQL 8.4.11 answers for the same table and the
/// same statement, recorded in the pinned golden `select-row-in.json`.
#[cfg(unix)]
#[test]
fn a_row_of_columns_is_looked_up_in_a_list_of_rows() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([69; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE pairs (id INT NOT NULL PRIMARY KEY, a INT, b VARCHAR(10))",
        "INSERT INTO pairs (id, a, b) VALUES (1, 1, 'x'), (2, 2, 'y'), (3, 1, 'y'), (4, NULL, 'x')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM pairs WHERE (a, b) IN ((1, 'x')) ORDER BY id",
            vec!["1"],
        ),
        (
            "SELECT id FROM pairs WHERE (a, b) IN ((1, 'x'), (2, 'y')) ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM pairs WHERE (a, b) IN ((9, 'z')) ORDER BY id",
            vec![],
        ),
        // Measured: MySQL reads the word under the column's collation, which
        // ignores case.
        (
            "SELECT id FROM pairs WHERE (a, b) IN ((1, 'X')) ORDER BY id",
            vec!["1"],
        ),
        // Measured: the row holding nothing in one column is left out of the
        // NOT IN as well as the IN, which is what three-valued logic answers.
        (
            "SELECT id FROM pairs WHERE (a, b) NOT IN ((1, 'x')) ORDER BY id",
            vec!["2", "3"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // A member that is not a row, or one of a different width, asks a
        // question the columns cannot answer.
        "SELECT id FROM pairs WHERE (a, b) IN (1)",
        "SELECT id FROM pairs WHERE (a, b) IN ((1, 'x', 2))",
        // Each column is held to its own type, the way it is in a comparison
        // written out.
        "SELECT id FROM pairs WHERE (a, b) IN (('x', 1))",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `ORDER BY n IS NULL, n`, which is how a statement asks for the rows holding
/// nothing to come last.
///
/// Every order below is the order MySQL 8.4.11 answers for the same table and
/// the same statement, recorded in the pinned golden
/// `select-order-by-nulls.json`.
#[cfg(unix)]
#[test]
fn ordering_by_whether_a_column_holds_nothing_sends_those_rows_last() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([70; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE listed (id INT NOT NULL PRIMARY KEY, n INT, name VARCHAR(10))",
        "INSERT INTO listed (id, n, name) VALUES (1, 2, 'b'), (2, NULL, NULL), (3, 1, 'a'), (4, NULL, 'c')",
    ] {
        adapter.execute_query(sql).unwrap_or_else(|error| {
            panic!("{sql}: {error:?}");
        });
    }

    let mut ids = |sql: &str| {
        let CommandExecutionResult::ResultSet(result) = adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        else {
            panic!("SELECT must return a result set");
        };
        result
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>()
    };

    for (sql, expected) in [
        (
            "SELECT id FROM listed ORDER BY n IS NULL, n",
            vec!["3", "1", "2", "4"],
        ),
        // Left off, the rows holding nothing come first, which is where both
        // put them.
        ("SELECT id FROM listed ORDER BY n", vec!["2", "4", "3", "1"]),
        (
            "SELECT id FROM listed ORDER BY n IS NULL, n DESC",
            vec!["1", "3", "2", "4"],
        ),
        (
            "SELECT id FROM listed ORDER BY n IS NOT NULL, n",
            vec!["2", "4", "3", "1"],
        ),
        (
            "SELECT id FROM listed ORDER BY name IS NULL, name",
            vec!["3", "1", "4", "2"],
        ),
        (
            "SELECT id FROM listed ORDER BY n IS NULL DESC, id",
            vec!["2", "4", "1", "3"],
        ),
    ] {
        assert_eq!(ids(sql), expected, "{sql}");
    }

    for sql in [
        // The test is over a column, not over anything that reduces to one.
        "SELECT id FROM listed ORDER BY (n + 1) IS NULL",
        "SELECT id FROM listed ORDER BY LOWER(name) IS NULL",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
