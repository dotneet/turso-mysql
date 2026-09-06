// Copyright 2026 the Turso authors. All rights reserved. MIT license.

use std::sync::Arc;

use turso_core::storage::database::DatabaseFile;
use turso_core::{Database, MemoryIO, OpenFlags, OpenOptions, Value, IO};
use turso_mysql::schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext};
use turso_mysql::{
    MySqlAffectedRowsMode, MySqlConnection, MySqlDialect, MySqlPreparedExecutionResult,
    MySqlPreparedValue, MySqlQueryError,
};
use turso_mysql_parser::MySqlTableName;

#[test]
fn empty_insert_applies_typed_defaults_and_reports_one_row() {
    let connection = connection();
    connection.execute("CREATE TABLE records (small SMALLINT NOT NULL DEFAULT -12, wide BIGINT DEFAULT 9223372036854775807, enabled INT DEFAULT TRUE, missing INT NULL, explicit_null TEXT DEFAULT NULL)").unwrap();
    let sql = "INSERT INTO records () VALUES ()";
    let result = connection.execute_checked_write(sql, None).unwrap();
    assert_eq!(result.affected_rows, 1);
    assert_eq!(result.last_insert_id, 0);
    let prepared = connection.prepare_checked_statement(sql).unwrap();
    assert_eq!(prepared.parameter_count, 0);
    let MySqlPreparedExecutionResult::Write(result) = connection
        .execute_prepared_statement(
            prepared.statement_id,
            &[],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap()
    else {
        panic!("expected INSERT result")
    };
    assert_eq!(result.affected_rows, 1);
    assert_eq!(result.last_insert_id, 0);
    let rows = connection
        .prepare_select("SELECT small, wide, enabled, missing, explicit_null FROM records")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::from_i64(-12),
                Value::from_i64(i64::MAX),
                Value::from_i64(1),
                Value::Null,
                Value::Null
            ];
            2
        ]
    );
    connection.close().unwrap();
}

#[test]
fn empty_insert_rejects_missing_required_value_without_a_partial_row() {
    for definition in ["required INT NOT NULL", "required TEXT NOT NULL"] {
        let connection = connection();
        connection
            .execute(&format!(
                "CREATE TABLE required_values ({definition}, optional INT DEFAULT 7)"
            ))
            .unwrap();
        let error = connection
            .execute_checked_write("INSERT INTO required_values () VALUES ()", None)
            .unwrap_err();
        assert!(
            matches!(error, turso_mysql::MySqlQueryError::MissingRequiredDefault(ref column) if column == "required"),
            "{error}"
        );
        let prepared = connection
            .prepare_checked_statement("INSERT INTO required_values () VALUES ()")
            .unwrap();
        assert!(matches!(
            connection.execute_prepared_statement(
                prepared.statement_id,
                &[],
                Some(std::time::Duration::ZERO),
                MySqlAffectedRowsMode::Changed,
            ),
            Err(turso_mysql::MySqlPreparedStatementError::Engine(
                turso_core::LimboError::Interrupt
            ))
        ));
        assert!(matches!(
            connection.execute_prepared_statement(
                prepared.statement_id, &[], None, MySqlAffectedRowsMode::Changed,
            ),
            Err(turso_mysql::MySqlPreparedStatementError::MissingRequiredDefault(ref column)) if column == "required"
        ));
        assert!(connection
            .prepare_select("SELECT required, optional FROM required_values")
            .unwrap()
            .run_collect_rows()
            .unwrap()
            .is_empty());
        connection.reset_connection().unwrap();
        assert!(matches!(
            connection.execute_prepared_statement(
                prepared.statement_id,
                &[],
                None,
                MySqlAffectedRowsMode::Changed,
            ),
            Err(turso_mysql::MySqlPreparedStatementError::UnknownStatement { .. })
        ));
        connection.close().unwrap();
    }
}

#[test]
fn insert_reports_the_first_required_column_it_never_lists() {
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL, req_a INT NOT NULL, req_b INT NOT NULL, opt INT, with_def INT NOT NULL DEFAULT 7)")
        .unwrap();

    let result = connection
        .execute_checked_write("INSERT INTO t (id, req_a, req_b) VALUES (1, 1, 1)", None)
        .unwrap();
    assert_eq!(result.affected_rows, 1);

    let error = connection
        .execute_checked_write("INSERT INTO t (id, req_a) VALUES (2, 1)", None)
        .unwrap_err();
    assert!(
        matches!(error, MySqlQueryError::MissingRequiredDefault(ref column) if column == "req_b"),
        "{error}"
    );

    // MySQL names the first required column in table definition order, not in
    // the order the statement lists columns.
    let error = connection
        .execute_checked_write("INSERT INTO t (opt) VALUES (9)", None)
        .unwrap_err();
    assert!(
        matches!(error, MySqlQueryError::MissingRequiredDefault(ref column) if column == "id"),
        "{error}"
    );

    // with_def is NOT NULL but has a default, so leaving it out is fine.
    let result = connection
        .execute_checked_write("INSERT INTO t (id, req_a, req_b) VALUES (3, 1, 1)", None)
        .unwrap();
    assert_eq!(result.affected_rows, 1);

    assert_eq!(
        connection
            .prepare_select("SELECT id FROM t ORDER BY id")
            .unwrap()
            .run_collect_rows()
            .unwrap(),
        vec![vec![Value::from_i64(1)], vec![Value::from_i64(3)]]
    );
    connection.close().unwrap();
}

#[test]
fn an_explicit_null_keeps_its_not_null_error_instead_of_the_missing_default_one() {
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL, req_a INT NOT NULL, req_b INT NOT NULL)")
        .unwrap();

    // MySQL stores the values it was given before checking that every required
    // column was filled, so an explicit NULL reports 1048 and suppresses the
    // 1364 check even when another required column is missing.
    for sql in [
        "INSERT INTO t (id, req_a) VALUES (1, NULL)",
        "INSERT INTO t (id, req_b) VALUES (2, NULL)",
        "INSERT INTO t (id, req_a, req_b) VALUES (3, NULL, 1)",
        // The checked INSERT grammar keeps parentheses and a unary plus, so a
        // NULL can reach a column wrapped in either.
        "INSERT INTO t (id, req_a) VALUES (4, (NULL))",
        "INSERT INTO t (id, req_a) VALUES (5, ((NULL)))",
        "INSERT INTO t (id, req_a) VALUES (6, (+NULL))",
    ] {
        let error = connection.execute_checked_write(sql, None).unwrap_err();
        assert!(
            matches!(
                error,
                MySqlQueryError::Engine(turso_core::LimboError::NotNullConstraint { .. })
            ),
            "{sql}: {error}"
        );
    }

    assert!(connection
        .prepare_select("SELECT id FROM t")
        .unwrap()
        .run_collect_rows()
        .unwrap()
        .is_empty());
    connection.close().unwrap();
}

#[test]
fn a_null_in_a_not_null_column_with_a_default_also_keeps_its_not_null_error() {
    // 1048 fires on any NOT NULL column, so a default does not exempt a column
    // from suppressing the 1364 check.
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL, req INT NOT NULL, def INT NOT NULL DEFAULT 7)")
        .unwrap();
    let error = connection
        .execute_checked_write("INSERT INTO t (id, def) VALUES (1, NULL)", None)
        .unwrap_err();
    assert!(
        matches!(
            error,
            MySqlQueryError::Engine(turso_core::LimboError::NotNullConstraint { .. })
        ),
        "{error}"
    );
    connection.close().unwrap();
}

#[test]
fn only_the_first_rows_nulls_suppress_the_missing_default_error() {
    // MySQL stops at the first row that fails. Every row shares the column
    // list, so a required column the statement never lists already fails on
    // row one and MySQL never reaches a later row's NULL.
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL, req_a INT NOT NULL, req_b INT NOT NULL)")
        .unwrap();

    let error = connection
        .execute_checked_write("INSERT INTO t (id, req_a) VALUES (1, 1), (2, NULL)", None)
        .unwrap_err();
    assert!(
        matches!(error, MySqlQueryError::MissingRequiredDefault(ref column) if column == "req_b"),
        "{error}"
    );

    let error = connection
        .execute_checked_write("INSERT INTO t (id, req_a) VALUES (3, NULL), (4, 1)", None)
        .unwrap_err();
    assert!(
        matches!(
            error,
            MySqlQueryError::Engine(turso_core::LimboError::NotNullConstraint { .. })
        ),
        "{error}"
    );

    // Every required column is listed, so nothing reports 1364 and the engine
    // rejects the later row's NULL.
    let error = connection
        .execute_checked_write(
            "INSERT INTO t (id, req_a, req_b) VALUES (5, 1, 1), (6, NULL, 1)",
            None,
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            MySqlQueryError::Engine(turso_core::LimboError::NotNullConstraint { .. })
        ),
        "{error}"
    );

    assert!(connection
        .prepare_select("SELECT id FROM t")
        .unwrap()
        .run_collect_rows()
        .unwrap()
        .is_empty());
    connection.close().unwrap();
}

#[test]
fn insert_into_a_table_the_column_metadata_cannot_describe_still_works() {
    // list_columns rejects these tables, so the required-column check must not
    // go through it: an ordinary INSERT into them has to keep working. A named
    // index used to be one of them and no longer is, since it says nothing
    // about how the columns were declared.
    for (schema, insert) in [
        (
            "CREATE TABLE t (id INT NOT NULL, label TEXT, UNIQUE (id, label))",
            "INSERT INTO t (id, label) VALUES (1, 'a')",
        ),
        (
            "CREATE TABLE t (id INT NOT NULL, label TEXT DEFAULT 1.25)",
            "INSERT INTO t (id, label) VALUES (1, 'a')",
        ),
    ] {
        let connection = connection();
        for statement in schema.split(';') {
            connection.execute(statement.trim()).unwrap();
        }
        assert!(
            connection
                .list_columns(&MySqlTableName::parse("t").unwrap())
                .is_err(),
            "{schema}: this table is meant to be one list_columns rejects"
        );
        let result = connection.execute_checked_write(insert, None).unwrap();
        assert_eq!(result.affected_rows, 1, "{insert}");
        connection.close().unwrap();
    }
}

#[test]
fn required_columns_ignore_case_quoting_and_column_order() {
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL, req INT NOT NULL)")
        .unwrap();
    for sql in [
        "INSERT INTO t (ID, REQ) VALUES (1, 1)",
        "INSERT INTO t (`id`, `req`) VALUES (2, 1)",
        "INSERT INTO t (req, id) VALUES (1, 3)",
        "INSERT INTO t (id, req) VALUES (4, 1), (5, 1)",
    ] {
        connection.execute_checked_write(sql, None).unwrap();
    }
    let error = connection
        .execute_checked_write("INSERT INTO t (ID) VALUES (6)", None)
        .unwrap_err();
    assert!(
        matches!(error, MySqlQueryError::MissingRequiredDefault(ref column) if column == "req"),
        "{error}"
    );
    connection.close().unwrap();
}

#[test]
fn a_prepared_insert_decides_on_the_values_it_was_bound() {
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL, req_a INT NOT NULL, req_b INT NOT NULL)")
        .unwrap();

    let missing = connection
        .prepare_checked_statement("INSERT INTO t (id, req_a) VALUES (?, ?)")
        .unwrap();
    assert!(matches!(
        connection.execute_prepared_statement(
            missing.statement_id,
            &[MySqlPreparedValue::Integer(1), MySqlPreparedValue::Integer(1)],
            None,
            MySqlAffectedRowsMode::Changed,
        ),
        Err(turso_mysql::MySqlPreparedStatementError::MissingRequiredDefault(ref column))
            if column == "req_b"
    ));

    let complete = connection
        .prepare_checked_statement("INSERT INTO t (id, req_a, req_b) VALUES (?, ?, ?)")
        .unwrap();
    // A bound NULL is an explicit NULL, so this reports the NOT NULL failure
    // rather than the missing-default one.
    assert!(matches!(
        connection.execute_prepared_statement(
            complete.statement_id,
            &[
                MySqlPreparedValue::Integer(2),
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Integer(1),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        ),
        Err(turso_mysql::MySqlPreparedStatementError::Engine(
            turso_core::LimboError::NotNullConstraint { .. }
        ))
    ));

    // The statement survives both failures and runs once its values are right.
    let MySqlPreparedExecutionResult::Write(result) = connection
        .execute_prepared_statement(
            complete.statement_id,
            &[
                MySqlPreparedValue::Integer(3),
                MySqlPreparedValue::Integer(1),
                MySqlPreparedValue::Integer(1),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap()
    else {
        panic!("expected INSERT result")
    };
    assert_eq!(result.affected_rows, 1);
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM t")
            .unwrap()
            .run_collect_rows()
            .unwrap(),
        vec![vec![Value::from_i64(3)]]
    );
    connection.close().unwrap();
}

#[test]
fn a_primary_key_without_auto_increment_is_a_required_column() {
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, label TEXT)")
        .unwrap();
    let error = connection
        .execute_checked_write("INSERT INTO t (label) VALUES ('a')", None)
        .unwrap_err();
    assert!(
        matches!(error, MySqlQueryError::MissingRequiredDefault(ref column) if column == "id"),
        "{error}"
    );
    connection
        .execute_checked_write("INSERT INTO t (id, label) VALUES (1, 'a')", None)
        .unwrap();
    connection.close().unwrap();
}

#[test]
fn a_failed_insert_leaves_no_row_and_keeps_the_connection_usable() {
    let connection = connection();
    connection
        .execute("CREATE TABLE t (id INT NOT NULL, req INT NOT NULL)")
        .unwrap();
    for sql in [
        "INSERT INTO t (id) VALUES (1)",
        "INSERT INTO t (id, req) VALUES (2, NULL)",
        "INSERT INTO t (id) VALUES (3), (4)",
    ] {
        assert!(
            connection.execute_checked_write(sql, None).is_err(),
            "{sql}"
        );
    }
    let result = connection
        .execute_checked_write("INSERT INTO t (id, req) VALUES (5, 1)", None)
        .unwrap();
    assert_eq!(result.affected_rows, 1);
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM t")
            .unwrap()
            .run_collect_rows()
            .unwrap(),
        vec![vec![Value::from_i64(5)]]
    );
    connection.close().unwrap();
}

fn connection() -> MySqlConnection {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "insert-defaults.db";
    let file = io.open_file(path, OpenFlags::Create, true).unwrap();
    let database = Database::open(
        io,
        path,
        OpenOptions::new(Arc::new(MySqlDialect))
            .storage(Arc::new(DatabaseFile::new(file)))
            .flags(OpenFlags::Create),
    )
    .unwrap();
    MySqlConnection::new(
        database.connect().unwrap(),
        SchemaSqlSessionContext {
            sql_mode: SchemaSqlMode {
                ansi_quotes: false,
                no_backslash_escapes: false,
            },
            character_set_client: CharacterSet::Binary,
            collation_connection: Collation::Binary,
            default_character_set: CharacterSet::Binary,
            default_collation: Collation::Binary,
        },
    )
    .unwrap()
}
