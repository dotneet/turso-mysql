// Copyright 2026 the Turso authors. All rights reserved. MIT license.

use std::sync::Arc;

use turso_core::storage::database::DatabaseFile;
use turso_core::{Database, MemoryIO, OpenFlags, OpenOptions, Value, IO};
use turso_mysql::schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext};
use turso_mysql::{
    MySqlAffectedRowsMode, MySqlConnection, MySqlDialect, MySqlPreparedExecutionResult,
};

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
