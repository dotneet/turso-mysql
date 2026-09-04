// Copyright 2026 the Turso authors. All rights reserved. MIT license.

use std::sync::Arc;

use turso_core::{storage::database::DatabaseFile, Database, MemoryIO, OpenFlags, OpenOptions, IO};
use turso_mysql::{
    schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext},
    MySqlConnection, MySqlDialect, MySqlPreparedValue,
};

#[test]
fn ordered_limits_preserve_nulls_binary_order_and_prepared_metadata() {
    let connection = connection();
    connection
        .execute("CREATE TABLE records (id INT, label TEXT)")
        .unwrap();
    connection
        .execute("INSERT INTO records (id, label) VALUES (3, 'b'), (1, 'A'), (2, 'a'), (4, NULL)")
        .unwrap();
    for suffix in ["LIMIT 2 OFFSET 1", "LIMIT 1, 2"] {
        let sql = format!("SELECT id AS ranked FROM records ORDER BY label ASC, id DESC {suffix}");
        let rows = connection
            .prepare_select(&sql)
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], turso_core::Value::from_i64(1));
        assert_eq!(rows[1][0], turso_core::Value::from_i64(2));
    }
    let metadata = connection
        .prepare_checked_statement(
            "SELECT id AS ranked, ? AS marker FROM records ORDER BY ranked DESC LIMIT 2",
        )
        .unwrap();
    assert_eq!(metadata.parameter_count, 1);
    assert_eq!(metadata.result_columns[0].name, "ranked");
    assert_eq!(
        connection
            .prepared_statement_result_column_type_metadata(metadata.statement_id)
            .unwrap()[0]
            .declared_type_name(),
        Some("INT")
    );
    for marker in [7, 8] {
        let rows = connection
            .execute_prepared_select(
                metadata.statement_id,
                &[MySqlPreparedValue::Integer(marker)],
                None,
            )
            .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![
                    MySqlPreparedValue::Integer(4),
                    MySqlPreparedValue::Integer(marker)
                ],
                vec![
                    MySqlPreparedValue::Integer(3),
                    MySqlPreparedValue::Integer(marker)
                ],
            ]
        );
    }
    for suffix in ["LIMIT 0", "LIMIT 1 OFFSET 9223372036854775807"] {
        assert!(connection
            .prepare_select(&format!("SELECT id FROM records ORDER BY id {suffix}"))
            .unwrap()
            .run_collect_rows()
            .unwrap()
            .is_empty());
    }
    let all_rows = connection
        .prepare_select("SELECT id FROM records ORDER BY label, id LIMIT 9223372036854775807")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(all_rows.len(), 4);
    assert_eq!(all_rows[0][0], turso_core::Value::from_i64(4));
    let shadowed = connection
        .prepare_select("SELECT id AS label FROM records ORDER BY label")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(
        shadowed
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        (1..=4).map(turso_core::Value::from_i64).collect::<Vec<_>>()
    );
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM records ORDER BY label DESC, id LIMIT 1")
            .unwrap()
            .run_collect_rows()
            .unwrap()[0][0],
        turso_core::Value::from_i64(3)
    );
    assert!(connection
        .prepare_select("SELECT name FROM sqlite_schema ORDER BY name LIMIT 1")
        .is_err());
    assert!(connection
        .prepare_checked_statement("SELECT name FROM sqlite_schema ORDER BY name LIMIT 1")
        .is_err());
    connection.close().unwrap();
}

fn connection() -> MySqlConnection {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let file = io
        .open_file("select-clauses.db", OpenFlags::Create, true)
        .unwrap();
    let database = Database::open(
        io,
        "select-clauses.db",
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
