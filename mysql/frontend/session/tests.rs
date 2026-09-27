//! Tests for the MySQL session.
//!
//! Kept in their own file because the session is the widest surface in the
//! frontend, and its tests outgrew reading alongside it.

use super::*;
use crate::{
    schema_sql::{decode_schema_sql, CharacterSet, Collation, SchemaSqlKind, SchemaSqlMode},
    MySqlDialect,
};
use turso_core::{
    io::FileSyncType,
    storage::auto_increment::{AllocatorDatabaseIdentity, AllocatorOpenMode},
    storage::database::DatabaseFile,
    AssignmentError, Database, DatabaseOpts, MemoryIO, OpenFlags, OpenOptions, PlatformIO,
    SchemaCatalogValidationContext, Value, IO,
};
use turso_parser::parser::Parser;

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
fn fractional_temporal_values_and_declarations_survive_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-fractional-temporal.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute(
            "CREATE TABLE moments (id INT PRIMARY KEY, t TIME(3), d DATETIME(6), s TIMESTAMP(2))",
        )?;
        connection.execute("INSERT INTO moments (id, t, d, s) VALUES (1, '-12:34:56.1235', '2024-12-31 23:59:59.1234567', '2024-01-01 12:00:00.125')")?;
        let columns = connection
            .list_columns(&MySqlTableName::parse("moments").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(columns[1].temporal_precision(), Some(3));
        assert_eq!(columns[2].temporal_precision(), Some(6));
        assert_eq!(columns[3].temporal_precision(), Some(2));
        assert_eq!(
            connection
                .prepare_select("SELECT t, d, s FROM moments")
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .run_collect_rows()?,
            vec![vec![
                Value::build_text("-12:34:56.124"),
                Value::build_text("2024-12-31 23:59:59.123457"),
                Value::build_text("2024-01-01 12:00:00.13"),
            ]]
        );
        connection.close()?;
    }
    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .prepare_select("SELECT t, d, s FROM moments")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![
            Value::build_text("-12:34:56.124"),
            Value::build_text("2024-12-31 23:59:59.123457"),
            Value::build_text("2024-01-01 12:00:00.13"),
        ]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn decimal_values_survive_prepared_insert_index_and_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-exact-decimal.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute(
            "CREATE TABLE exact_amounts (id INT NOT NULL PRIMARY KEY, v DECIMAL(65,30))",
        )?;
        assert_eq!(
            connection
                .inner()
                .current_schema()
                .get_btree_table("exact_amounts")
                .unwrap()
                .columns()[1]
                .ty_str,
            "mysql_decimal"
        );
        connection.execute("CREATE INDEX amount_index ON exact_amounts (v)")?;
        connection.execute("INSERT INTO exact_amounts (id, v) VALUES (1, 0.100000000000000000000000000001), (2, -1.235)")?;
        let prepared = connection
            .prepare_checked_statement("INSERT INTO exact_amounts (id, v) VALUES (?, ?)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        connection
            .execute_prepared_statement(
                prepared.statement_id,
                &[
                    MySqlPreparedValue::Integer(3),
                    MySqlPreparedValue::Text("0.100000000000000000000000000002".to_string()),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM exact_amounts ORDER BY v")
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(2)],
                vec![Value::from_i64(1)],
                vec![Value::from_i64(3)]
            ]
        );
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .prepare_select(
                "SELECT v FROM exact_amounts WHERE v = 0.100000000000000000000000000001"
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.100000000000000000000000000001")]]
    );
    let prepared_select = connection
        .prepare_checked_statement("SELECT id FROM exact_amounts WHERE v = ?")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        connection
            .execute_prepared_select(
                prepared_select.statement_id,
                &[MySqlPreparedValue::Text(
                    "0.100000000000000000000000000002".to_string(),
                )],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![MySqlPreparedValue::Integer(3)]]
    );
    let prepared_less = connection
        .prepare_checked_statement("SELECT id FROM exact_amounts WHERE v < ? ORDER BY id")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        connection
            .execute_prepared_select(
                prepared_less.statement_id,
                &[MySqlPreparedValue::Text(
                    "0.100000000000000000000000000002".to_string(),
                )],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![
            vec![MySqlPreparedValue::Integer(1)],
            vec![MySqlPreparedValue::Integer(2)],
        ]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT SUM(v), AVG(v) FROM exact_amounts WHERE id IN (1, 3)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![
            Value::build_text("0.200000000000000000000000000003"),
            Value::build_text("0.100000000000000000000000000002"),
        ]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT v * 0.5 FROM exact_amounts WHERE id = 1")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.050000000000000000000000000001")]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT SUM(v) + 0.001 FROM exact_amounts WHERE id IN (1, 3)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.201000000000000000000000000003")]]
    );
    let wide_integer = "1".repeat(65);
    connection.execute("CREATE TABLE whole_decimals (v DECIMAL(65,0))")?;
    connection.execute(&format!(
        "INSERT INTO whole_decimals (v) VALUES ({wide_integer})"
    ))?;
    assert_eq!(
        connection
            .prepare_select(&format!(
                "SELECT v FROM whole_decimals WHERE v = {wide_integer}"
            ))
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text(wide_integer)]]
    );
    connection.execute("CREATE TABLE divided_amounts (v DECIMAL(5,2))")?;
    connection.execute("INSERT INTO divided_amounts (v) VALUES (1.24)")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v / 0.5 FROM divided_amounts")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("2.480000")]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT SUM(v) / 2 FROM divided_amounts")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.620000")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn decimal_updates_keep_operand_precision_until_assignment() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-decimal-update.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE d (v DECIMAL(5,2))")?;
    connection.execute("CREATE INDEX d_v ON d(v)")?;
    connection.execute("INSERT INTO d (v) VALUES (1.23)")?;
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM d WHERE v = 1.234")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(0)]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM d WHERE v < 1.234")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM d WHERE v >= 1.234")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(0)]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM d WHERE v IN (1.23, 1.234)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM d WHERE v IN (1.234)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(0)]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM d WHERE v = 1.234 OR v = 1.23")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    connection.execute("UPDATE d SET v = v - 0.005")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM d")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("1.23")]]
    );
    connection.execute("UPDATE d SET v = v / 2")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM d")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.62")]]
    );

    connection.execute("CREATE TABLE wide (v DECIMAL(65,30))")?;
    connection.execute("INSERT INTO wide (v) VALUES (0.100000000000000000000000000001)")?;
    connection.execute("UPDATE wide SET v = v / 2")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM wide")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.050000000000000000000000000001")]]
    );
    let prepared = connection
        .prepare_checked_statement("UPDATE wide SET v = v + ?")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection
        .execute_prepared_statement(
            prepared.statement_id,
            &[MySqlPreparedValue::Text(
                "0.000000000000000000000000000001".to_string(),
            )],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM wide")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.050000000000000000000000000002")]]
    );
    assert!(connection
        .execute_prepared_statement(
            prepared.statement_id,
            &[MySqlPreparedValue::Text("1e100000000".to_string())],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .is_err());

    connection.execute("CREATE TABLE signed_wide (v DECIMAL(65,0))")?;
    connection.execute("INSERT INTO signed_wide (v) VALUES (-9223372036854775809)")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM signed_wide")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("-9223372036854775809")]]
    );
    connection.execute("CREATE TABLE upsert_decimal (id INT PRIMARY KEY, v DECIMAL(65,30))")?;
    connection.execute(
        "INSERT INTO upsert_decimal (id, v) VALUES (1, 0.100000000000000000000000000001)",
    )?;
    connection.execute("INSERT INTO upsert_decimal (id, v) VALUES (1, 0.000000000000000000000000000001) ON DUPLICATE KEY UPDATE v = v + VALUES(v)")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM upsert_decimal")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.100000000000000000000000000002")]]
    );
    connection.execute("INSERT INTO upsert_decimal (id, v) VALUES (1, 0.000000000000000000000000000001) ON DUPLICATE KEY UPDATE v = (v + VALUES(v)) + 0.000000000000000000000000000001")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM upsert_decimal")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.100000000000000000000000000004")]]
    );
    connection.execute(
        "INSERT INTO upsert_decimal (id, v) VALUES (1, 0) ON DUPLICATE KEY UPDATE v = 0.1 + 0.2",
    )?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM upsert_decimal")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.300000000000000000000000000000")]]
    );
    connection.execute("UPDATE upsert_decimal SET v = 0.100000000000000000000000000001 + 0.000000000000000000000000000001")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM upsert_decimal")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("0.100000000000000000000000000002")]]
    );
    connection.execute("CREATE TABLE mixed (v DECIMAL(20,2), r DOUBLE, i BIGINT)")?;
    connection.execute("INSERT INTO mixed (v, r, i) VALUES (1.25, 2.0, 9007199254740993)")?;
    assert!(connection
        .prepare_select("SELECT v / r FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT CAST(v AS SIGNED) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT CONVERT(v, SIGNED) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT SQRT(v) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT POW(v, 2) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT LENGTH(v) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT SUBSTRING(v, 1, 1) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT TRIM(v) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT EXTRACT(YEAR FROM v) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT SUM(v) OVER (ORDER BY i) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT AVG(v) OVER (ORDER BY i) FROM mixed")
        .is_err());
    assert!(connection
        .prepare_select("SELECT LAG(v) OVER (ORDER BY i) FROM mixed")
        .is_err());
    connection.execute("UPDATE mixed SET v = i / 2")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM mixed")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("4503599627370496.50")]]
    );
    assert!(connection
        .execute("UPDATE mixed SET v = CAST(v AS SIGNED)")
        .is_err());
    assert!(connection
        .execute("UPDATE mixed SET v = CONVERT(v, SIGNED)")
        .is_err());
    assert!(connection.execute("UPDATE mixed SET i = v + 0").is_err());
    assert!(connection.execute("UPDATE mixed SET i = v").is_err());
    assert!(connection
        .execute("UPDATE mixed SET i = (v + 0) + 0")
        .is_err());
    assert!(connection.execute("UPDATE mixed SET i = (+v) + 0").is_err());
    assert!(connection.execute("UPDATE mixed SET i = v / 2").is_err());
    assert!(connection.execute("UPDATE mixed SET i = -v").is_err());
    assert!(connection.execute("UPDATE mixed SET i = ABS(v)").is_err());
    assert!(connection
        .execute("UPDATE mixed SET i = TRUNCATE(v, 1)")
        .is_err());
    assert!(connection
        .execute("UPDATE mixed SET i = CAST(v AS SIGNED)")
        .is_err());
    assert!(connection
        .execute("UPDATE mixed SET i = CASE WHEN 1 = 1 THEN v ELSE 0 END")
        .is_err());
    connection.execute("CREATE TABLE upsert_mixed (id INT PRIMARY KEY, v DECIMAL(5,2), i INT)")?;
    connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1)")?;
    assert!(connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = v + 0").is_err());
    assert!(connection
        .execute(
            "INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = v"
        )
        .is_err());
    assert!(connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = VALUES(v)").is_err());
    assert!(connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = (v + 0) + 0").is_err());
    assert!(connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = (+v) + 0").is_err());
    assert!(connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = -v").is_err());
    assert!(connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = ABS(v)").is_err());
    assert!(connection.execute("INSERT INTO upsert_mixed (id, v, i) VALUES (1, 1.23, 1) ON DUPLICATE KEY UPDATE i = TRUNCATE(v, 1)").is_err());
    connection.execute("CREATE TABLE left_decimal (v DECIMAL(5,2))")?;
    connection.execute("CREATE TABLE right_decimal (v DECIMAL(5,1))")?;
    connection.execute("INSERT INTO left_decimal (v) VALUES (1.20)")?;
    connection.execute("INSERT INTO right_decimal (v) VALUES (1.2)")?;
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM left_decimal WHERE v IN (1.2)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    assert_eq!(
        connection
            .prepare_select(
                "SELECT COUNT(*) FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    assert!(connection
        .prepare_select("SELECT a.v FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v WHERE a.v IN (1.2)")
        .is_err());
    assert!(connection
        .prepare_select(
            "SELECT a.v / 2 FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
        )
        .is_err());
    assert!(connection
        .prepare_select("SELECT a.v + 0.123456789012345678901234567890 FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v")
        .is_err());
    assert!(connection
        .prepare_select(
            "SELECT SUM(a.v) FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
        )
        .is_err());
    assert!(connection
        .prepare_select(
            "SELECT AVG(a.v) FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
        )
        .is_err());
    assert!(connection
        .prepare_select(
            "SELECT TRUNCATE(a.v, 1) FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
        )
        .is_err());
    assert!(connection
        .prepare_select(
            "SELECT ABS(a.v) FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
        )
        .is_err());
    assert!(connection
        .prepare_select(
            "SELECT GREATEST(a.v, b.v) FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
        )
        .is_err());
    assert!(connection
        .prepare_select(
            "SELECT LEAST(a.v, b.v) FROM left_decimal AS a JOIN right_decimal AS b ON a.v = b.v"
        )
        .is_err());
    assert!(connection
        .prepare_select("SELECT x / 2 FROM (SELECT v AS x FROM left_decimal) AS q")
        .is_err());
    assert!(connection
        .prepare_select("WITH q AS (SELECT v AS x FROM left_decimal) SELECT x / 2 FROM q")
        .is_err());
    connection.execute("CREATE TABLE scalar_join (id INT)")?;
    connection.execute("INSERT INTO scalar_join (id) VALUES (1)")?;
    // The engine compares a DECIMAL with a whole number as a double, where
    // MySQL compares the two exactly, so the join itself is refused.
    for sql in [
        "SELECT SQRT(v) FROM left_decimal AS a JOIN scalar_join AS b ON a.v = b.id",
        "SELECT CAST(v AS SIGNED) FROM left_decimal AS a JOIN scalar_join AS b ON a.v = b.id",
        "SELECT LENGTH(v) FROM left_decimal AS a JOIN scalar_join AS b ON a.v = b.id",
        "SELECT b.id FROM left_decimal AS a JOIN scalar_join AS b ON a.v = b.id ORDER BY ABS(v)",
        "SELECT COUNT(*) FROM left_decimal AS a JOIN scalar_join AS b ON a.v = b.id",
        "SELECT b.id FROM left_decimal AS a JOIN scalar_join AS b ON a.v = b.id ORDER BY b.id",
    ] {
        assert!(connection.prepare_select(sql).is_err(), "{sql}");
    }
    assert!(connection
        .prepare_select("SELECT v FROM left_decimal UNION SELECT v FROM right_decimal")
        .is_err());
    connection.execute("CREATE TABLE delete_decimal (v DECIMAL(5,2))")?;
    connection.execute("INSERT INTO delete_decimal (v) VALUES (1.20)")?;
    connection.execute("DELETE FROM delete_decimal WHERE v IN (1.2)")?;
    assert_eq!(
        connection
            .prepare_select("SELECT COUNT(*) FROM delete_decimal")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(0)]]
    );
    connection.execute("CREATE TABLE insert_select_integer (i BIGINT)")?;
    assert!(connection
        .execute("INSERT INTO insert_select_integer (i) SELECT v FROM left_decimal")
        .is_err());
    connection.execute("CREATE TABLE insert_select_decimal (v DECIMAL(65,30))")?;
    assert!(connection
        .execute("INSERT INTO insert_select_decimal (v) SELECT v / 2 FROM left_decimal")
        .is_err());
    connection.execute("CREATE TABLE insert_select_same_decimal (v DECIMAL(5,2))")?;
    connection.execute("INSERT INTO insert_select_same_decimal (v) SELECT v FROM left_decimal")?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM insert_select_same_decimal")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("1.20")]]
    );
    connection.execute("CREATE TABLE insert_select_other_scale (v DECIMAL(5,1))")?;
    assert!(connection
        .execute("INSERT INTO insert_select_other_scale (v) SELECT v FROM left_decimal")
        .is_err());
    Ok(())
}

#[test]
fn prepared_decimal_update_refuses_changed_table_definition() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-decimal-reprepare.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE d(v DECIMAL(5,2))")?;
    let prepared = connection
        .prepare_checked_statement("UPDATE d SET v = v / 3")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    let drop =
        turso_mysql_parser::parse_optional_drop_table("DROP TABLE d", SessionSqlMode::default())
            .expect("DROP TABLE parses")
            .expect("DROP TABLE is recognized");
    connection
        .drop_table(&drop)
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection.execute("CREATE TABLE d(v DECIMAL(65,30))")?;
    connection.execute("INSERT INTO d(v) VALUES (1.000000000000000000000000000001)")?;
    assert!(connection
        .execute_prepared_statement(
            prepared.statement_id,
            &[],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .is_err());
    Ok(())
}

#[test]
fn prepared_decimal_select_keeps_typed_sql_after_schema_change() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-decimal-select-reprepare.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE d (v DECIMAL(65,30))")?;
    connection.execute("INSERT INTO d (v) VALUES (0.100000000000000000000000000001)")?;
    let prepared = connection
        .prepare_checked_statement("SELECT v / 2 FROM d")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    let expected = vec![vec![MySqlPreparedValue::Text(
        "0.050000000000000000000000000001".to_string(),
    )]];
    connection.execute("CREATE VIEW d_view AS SELECT v FROM d")?;
    assert!(connection
        .prepare_checked_statement("SELECT v / 2 FROM d_view")
        .is_err());
    assert_eq!(
        connection
            .execute_prepared_select(prepared.statement_id, &[], None)
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        expected
    );
    connection.execute("CREATE TABLE unrelated (id INT)")?;
    assert_eq!(
        connection
            .execute_prepared_select(prepared.statement_id, &[], None)
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        expected
    );
    let drop =
        turso_mysql_parser::parse_optional_drop_table("DROP TABLE d", SessionSqlMode::default())
            .expect("DROP TABLE parses")
            .expect("DROP TABLE is recognized");
    connection
        .drop_table(&drop)
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection.execute("CREATE TABLE d (v DECIMAL(5,2))")?;
    assert!(connection
        .execute_prepared_select(prepared.statement_id, &[], None)
        .is_err());
    Ok(())
}

#[test]
fn prepared_select_refuses_new_decimal_type_after_reprepare() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-select-new-decimal-reprepare.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE changing (v INT)")?;
    connection.execute("INSERT INTO changing (v) VALUES (1)")?;
    let statements = [
        "SELECT ABS(v) FROM changing",
        "SELECT LENGTH(v) FROM changing",
        "SELECT v FROM changing WHERE v = 1",
    ]
    .into_iter()
    .map(|sql| {
        connection
            .prepare_checked_statement(sql)
            .map(|prepared| prepared.statement_id)
    })
    .collect::<std::result::Result<Vec<_>, _>>()
    .map_err(|error| LimboError::InternalError(error.to_string()))?;
    for statement_id in &statements {
        assert_eq!(
            connection
                .execute_prepared_select(*statement_id, &[], None)
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .len(),
            1
        );
    }
    let drop = turso_mysql_parser::parse_optional_drop_table(
        "DROP TABLE changing",
        SessionSqlMode::default(),
    )
    .expect("DROP TABLE parses")
    .expect("DROP TABLE is recognized");
    connection
        .drop_table(&drop)
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection.execute("CREATE TABLE changing (v DECIMAL(5,2))")?;
    connection.execute("INSERT INTO changing (v) VALUES (1.20)")?;
    for statement_id in statements {
        assert!(connection
            .execute_prepared_select(statement_id, &[], None)
            .is_err());
    }
    Ok(())
}

#[test]
fn unreferenced_decimal_columns_do_not_block_checked_selects_or_copies() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-unused-decimal.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE a (id INT, v DECIMAL(5,2))")?;
    connection.execute("CREATE TABLE b (k INT)")?;
    connection.execute("INSERT INTO a (id, v) VALUES (2, 1.20)")?;
    connection.execute("INSERT INTO b (k) VALUES (2)")?;

    let mut result = connection
        .prepare_select("SELECT ABS(id) FROM a CROSS JOIN b ORDER BY ABS(id)")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(result.run_collect_rows()?, vec![vec![Value::from_i64(2)]]);
    let mut result = connection
        .prepare_select("SELECT 1 FROM a UNION SELECT 1 FROM b")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(result.run_collect_rows()?, vec![vec![Value::from_i64(1)]]);
    connection.execute("INSERT INTO b (k) SELECT 1 FROM a")?;
    let mut result = connection
        .prepare_select("SELECT COUNT(*) FROM b")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(result.run_collect_rows()?, vec![vec![Value::from_i64(2)]]);

    assert!(connection
        .prepare_select("SELECT ABS(v) FROM a CROSS JOIN b")
        .is_err());
    assert!(connection
        .prepare_select("SELECT 1 FROM a UNION SELECT v FROM a")
        .is_err());
    assert!(connection
        .execute("INSERT INTO b (k) SELECT v FROM a")
        .is_err());
    Ok(())
}

#[test]
fn prepared_insert_select_refuses_changed_source_definition() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-insert-select-reprepare.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE source_numbers (v INT)")?;
    connection.execute("CREATE TABLE copied_numbers (v INT)")?;
    connection.execute("INSERT INTO source_numbers (v) VALUES (1)")?;
    let prepared = connection
        .prepare_checked_statement("INSERT INTO copied_numbers (v) SELECT v FROM source_numbers")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection
        .execute_prepared_statement(
            prepared.statement_id,
            &[],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    let drop = turso_mysql_parser::parse_optional_drop_table(
        "DROP TABLE source_numbers",
        SessionSqlMode::default(),
    )
    .expect("DROP TABLE parses")
    .expect("DROP TABLE is recognized");
    connection
        .drop_table(&drop)
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection.execute("CREATE TABLE source_numbers (v DECIMAL(5,2))")?;
    connection.execute("INSERT INTO source_numbers (v) VALUES (1.20)")?;
    assert!(connection
        .execute_prepared_statement(
            prepared.statement_id,
            &[],
            None,
            MySqlAffectedRowsMode::Changed
        )
        .is_err());
    Ok(())
}

#[test]
fn unsigned_decimal_negative_reports_out_of_range_for_text_and_prepared_writes() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-unsigned-decimal-range.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE amounts (id INT, v DECIMAL(10,2) UNSIGNED)")?;
    assert!(matches!(
        connection.execute_checked_write("INSERT INTO amounts (id, v) VALUES (1, -1.5)", None),
        Err(MySqlQueryError::Engine(LimboError::Assignment(error)))
            if matches!(*error, AssignmentError::OutOfRange { column: 2, .. })
    ));
    let prepared = connection
        .prepare_checked_statement("INSERT INTO amounts (id, v) VALUES (?, ?)")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert!(matches!(
        connection.execute_prepared_statement(
            prepared.statement_id,
            &[
                MySqlPreparedValue::Integer(2),
                MySqlPreparedValue::Text("-0.001".to_string()),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        ),
        Err(MySqlPreparedStatementError::Engine(LimboError::Assignment(error)))
            if matches!(*error, AssignmentError::OutOfRange { column: 2, .. })
    ));
    Ok(())
}

#[test]
fn bigint_unsigned_keeps_all_digits_across_index_and_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-uint64-full-range.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE u (id INT PRIMARY KEY, v BIGINT UNSIGNED)")?;
        connection.execute("INSERT INTO u (id, v) VALUES (1, 0), (2, 9223372036854775807), (3, 9223372036854775808), (4, 18446744073709551615)")?;
        let prepared = connection
            .prepare_checked_statement("INSERT INTO u (id, v) VALUES (?, ?)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        connection
            .execute_prepared_statement(
                prepared.statement_id,
                &[
                    MySqlPreparedValue::Integer(5),
                    MySqlPreparedValue::UnsignedInteger(18446744073709551614),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        connection.execute("CREATE INDEX u_v ON u(v)")?;
        connection.close()?;
    }
    let db = open_database(io, path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM u ORDER BY v")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        [
            "0",
            "9223372036854775807",
            "9223372036854775808",
            "18446744073709551614",
            "18446744073709551615"
        ]
        .into_iter()
        .map(|value| vec![Value::build_text(value)])
        .collect::<Vec<_>>()
    );
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM u WHERE v = 9223372036854775808")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("9223372036854775808")]]
    );
    let prepared = connection
        .prepare_checked_statement("SELECT id FROM u WHERE v = ?")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        connection
            .execute_prepared_select(
                prepared.statement_id,
                &[MySqlPreparedValue::UnsignedInteger(18446744073709551614)],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![MySqlPreparedValue::Integer(5)]]
    );
    let prepared = connection
        .prepare_checked_statement("SELECT id FROM u WHERE v < ? ORDER BY id")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        connection
            .execute_prepared_select(
                prepared.statement_id,
                &[MySqlPreparedValue::UnsignedInteger(18446744073709551614)],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![
            vec![MySqlPreparedValue::Integer(1)],
            vec![MySqlPreparedValue::Integer(2)],
            vec![MySqlPreparedValue::Integer(3)],
        ]
    );
    let null_safe = connection
        .prepare_checked_statement("SELECT id FROM u WHERE v <=> ?")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        connection
            .execute_prepared_select(
                null_safe.statement_id,
                &[MySqlPreparedValue::UnsignedInteger(18446744073709551615)],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![MySqlPreparedValue::Integer(4)]]
    );
    assert_eq!(
        connection
            .prepare_select(
                "SELECT v FROM u WHERE v IN (9223372036854775808, 18446744073709551615) ORDER BY v"
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![
            vec![Value::build_text("9223372036854775808")],
            vec![Value::build_text("18446744073709551615")],
        ]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT v + 1 FROM u WHERE id = 5")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("18446744073709551615")]]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT SUM(v), AVG(v) FROM u WHERE id IN (2, 3)")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![
            Value::build_text("18446744073709551615"),
            Value::build_text("9223372036854775807.5000"),
        ]]
    );
    assert!(connection
        .prepare_select("SELECT v + 1 FROM u WHERE id = 4")
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .run_collect_rows()
        .is_err());
    Ok(())
}

#[test]
fn bigint_unsigned_negative_and_overflow_report_out_of_range() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-uint64-range.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE u (v BIGINT UNSIGNED)")?;
    for written in ["-1", "18446744073709551616"] {
        let sql = format!("INSERT INTO u (v) VALUES ({written})");
        assert!(matches!(
            connection.execute_checked_write(&sql, None),
            Err(MySqlQueryError::Engine(LimboError::Assignment(error)))
                if matches!(*error, AssignmentError::OutOfRange { column: 1, ref type_name, .. } if type_name == "BIGINT UNSIGNED")
        ));
    }
    Ok(())
}

#[test]
fn bigint_unsigned_auto_increment_crosses_signed_boundary() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-uint64-auto.db", [0x74; 16])?;
    connection.execute_schema_ddl("CREATE TABLE u (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, n INT) AUTO_INCREMENT=9223372036854775808").map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection.execute("INSERT INTO u (n) VALUES (1), (2)")?;
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM u ORDER BY id")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_text("9223372036854775808")],
            vec![Value::from_text("9223372036854775809")],
        ]
    );
    assert_eq!(connection.last_insert_id(), 9223372036854775808);
    connection.close()?;
    Ok(())
}

#[test]
fn untyped_prepared_projection_refuses_wide_unsigned_integer() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-uint64-untyped-arithmetic.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE u (id INT)")?;
    connection.execute("INSERT INTO u (id) VALUES (1)")?;
    assert!(connection
        .prepare_checked_statement("SELECT ? + 1 FROM u")
        .is_err());
    let prepared = connection
        .prepare_checked_statement("SELECT ? FROM u")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert!(matches!(
        connection.execute_prepared_select(
            prepared.statement_id,
            &[MySqlPreparedValue::UnsignedInteger(9223372036854775808)],
            None,
        ),
        Err(MySqlPreparedStatementError::Engine(
            LimboError::IntegerOverflow
        ))
    ));
    Ok(())
}

#[test]
fn wide_decimal_text_parameter_is_still_accepted() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-wide-decimal-text.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE d (v DECIMAL(65,0))")?;
    let prepared = connection
        .prepare_checked_statement("INSERT INTO d (v) VALUES (?)")
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection
        .execute_prepared_statement(
            prepared.statement_id,
            &[MySqlPreparedValue::Text("18446744073709551616".to_string())],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        connection
            .prepare_select("SELECT v FROM d")
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?,
        vec![vec![Value::build_text("18446744073709551616")]]
    );
    Ok(())
}

#[test]
fn recognizes_legacy_decimal_columns() {
    assert!(legacy_decimal_type("DECIMAL"));
    assert!(legacy_decimal_type("unsigned decimal"));
    assert!(!legacy_decimal_type("mysql_decimal"));
    assert!(!legacy_decimal_type("mysql_decimal_unsigned"));
}

fn open_database(io: Arc<dyn IO>, path: &str, flags: OpenFlags) -> Result<Arc<Database>> {
    let file = io.open_file(path, flags, true)?;
    Database::open(
        io,
        path,
        OpenOptions::new(Arc::new(MySqlDialect))
            .storage(Arc::new(DatabaseFile::new(file)))
            .flags(flags)
            .db_opts(DatabaseOpts::new().with_vacuum(true).with_views(true)),
    )
}

fn open_database_with_identity(
    io: Arc<dyn IO>,
    path: &str,
    flags: OpenFlags,
    database_identity: [u8; 16],
) -> Result<Arc<Database>> {
    let file = io.open_file(path, flags, true)?;
    Database::open(
        io,
        path,
        OpenOptions::new(Arc::new(MySqlDialect))
            .storage(Arc::new(DatabaseFile::new(file)))
            .flags(flags)
            .schema_catalog_validation_context(SchemaCatalogValidationContext::new(
                database_identity,
            ))
            .db_opts(DatabaseOpts::new().with_vacuum(true).with_views(true)),
    )
}

fn open_allocator_connection(
    path: &str,
    database_identity: [u8; 16],
) -> Result<(MySqlConnection, DurableRangeAllocator, Arc<dyn IO>)> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database =
        open_database_with_identity(Arc::clone(&io), path, OpenFlags::Create, database_identity)?;
    let allocator = DurableRangeAllocator::open(
        io.as_ref(),
        &format!("{path}.auto-increment"),
        AllocatorDatabaseIdentity::new(database_identity)?,
        AllocatorOpenMode::Create,
        FileSyncType::Fsync,
    )?;
    let mut initialization = allocator.initialize()?;
    io.block(|| initialization.step())?;
    let connection = MySqlConnection::new_with_auto_increment_and_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        allocator.clone(),
        Arc::clone(&io),
        MySqlPreparedStatementAuthority::default(),
    )?;
    Ok((connection, allocator, io))
}

#[test]
fn set_rows_order_by_member_bits_instead_of_text() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-set-order.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE member_rows (id INT, rights SET('read','write','exec'))")?;
    connection.execute(
        "INSERT INTO member_rows (id, rights) VALUES \
         (1, 'read,write'), (2, 'read'), (3, 'exec'), \
         (4, 'write'), (5, ''), (6, NULL)",
    )?;

    for (sql, expected) in [
        (
            "SELECT id FROM member_rows ORDER BY rights",
            vec![6, 5, 2, 4, 1, 3],
        ),
        (
            "SELECT id FROM member_rows ORDER BY rights DESC",
            vec![3, 1, 4, 2, 5, 6],
        ),
        (
            "SELECT id, rights FROM member_rows ORDER BY 2",
            vec![6, 5, 2, 4, 1, 3],
        ),
    ] {
        let rows = connection.prepare_select(sql)?.run_collect_rows()?;
        let ids = rows
            .into_iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            expected
                .into_iter()
                .map(Value::from_i64)
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }
    Ok(())
}

#[test]
fn float_rows_are_rounded_before_reads_comparisons_and_sums() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-float-storage.db", [0xf1; 16])?;
    connection.execute("CREATE TABLE numbers (id INT, value FLOAT)")?;
    connection.execute("INSERT INTO numbers (id, value) VALUES (1, 0.1), (2, 0.1), (3, 0.1)")?;

    let stored = Value::from_f64(f64::from(0.1_f32));
    assert_eq!(
        connection
            .prepare_select("SELECT value FROM numbers WHERE id = 1")?
            .run_collect_rows()?,
        vec![vec![stored]]
    );
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT id FROM numbers WHERE value > 0.100000001 ORDER BY id")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(1)],
            vec![Value::from_i64(2)],
            vec![Value::from_i64(3)],
        ]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT SUM(value) FROM numbers")?
            .run_collect_rows()?,
        vec![vec![Value::from_f64(f64::from(0.1_f32) * 3.0)]]
    );
    connection.close()?;
    Ok(())
}

fn auto_increment_key(connection: &MySqlConnection, table: &str) -> Result<AutoIncrementKey> {
    let rows = connection
        .inner()
        .prepare(format!(
            "SELECT sql FROM sqlite_schema WHERE name = '{table}'"
        ))?
        .run_collect_rows()?;
    let stored = rows
        .first()
        .and_then(|row| row.first())
        .ok_or_else(|| LimboError::InternalError("AUTO_INCREMENT table is missing".to_string()))?
        .to_string();
    let decoded = decode_schema_sql(SchemaSqlKind::Table, stored.trim_matches('\''))
        .map_err(|error| LimboError::Corrupt(error.to_string()))?
        .ok_or_else(|| LimboError::Corrupt("AUTO_INCREMENT table has no envelope".to_string()))?;
    AutoIncrementKey::new(
        decoded
            .v2_metadata()
            .ok_or_else(|| {
                LimboError::Corrupt("AUTO_INCREMENT table has no v2 metadata".to_string())
            })?
            .allocator_id
            .into_bytes(),
    )
}

#[test]
fn auto_increment_execute_reserves_and_injects_one_range_per_values_batch() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-auto-increment-execute.db", [0x51; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;

    connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;
    connection.execute("INSERT INTO users (name) VALUES ('Grace'), ('Linus')")?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (9, 'ordinary')")?;

    assert_eq!(
        connection
            .prepare_select("SELECT id, name FROM users")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(1), Value::from_text("Ada")],
            vec![Value::from_i64(2), Value::from_text("Grace")],
            vec![Value::from_i64(3), Value::from_text("Linus")],
        ]
    );
    assert_eq!(
        connection
            .prepare_select("SELECT id, body FROM notes")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(9), Value::from_text("ordinary")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn auto_increment_prepare_never_reserves_and_unsupported_marked_insert_fails_closed() -> Result<()>
{
    let (connection, allocator, io) =
        open_allocator_connection("mysql-session-auto-increment-prepare.db", [0x52; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    connection.prepare("INSERT INTO users (name) VALUES ('Ada')")?;
    assert!(connection
        .execute("INSERT INTO users (name) VALUES (upper('Ada'))")
        .is_err());

    let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
    assert_eq!(io.block(|| reservation.step())?.first(), 1);
    connection.execute("INSERT INTO users (name) VALUES ('Grace')")?;
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM users")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(2)]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn auto_increment_insert_with_a_target_trigger_fails_before_reservation() -> Result<()> {
    let (connection, allocator, io) =
        open_allocator_connection("mysql-session-auto-increment-trigger.db", [0x55; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    connection.execute("CREATE TABLE audit (name TEXT)")?;
    connection.execute(
        "CREATE TRIGGER copy_user AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
    )?;

    assert!(matches!(
        connection.execute("INSERT INTO users (name) VALUES ('Ada')"),
        Err(LimboError::ParseError(_))
    ));
    let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
    assert_eq!(io.block(|| reservation.step())?.first(), 1);
    connection.close()?;
    Ok(())
}

#[test]
fn an_identifier_that_does_not_resolve_is_an_error_not_a_string() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database = open_database_with_identity(
        io,
        "mysql-session-unresolved-identifier.db",
        OpenFlags::Create,
        [0x64; 16],
    )?;
    let connection = MySqlConnection::new(database.connect()?, binary_context())?;
    connection.execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, note TEXT)")?;
    connection.execute("INSERT INTO t (id, note) VALUES (1, 'a')")?;

    // With SQLite's DQS misfeature left on, each of these answered with the
    // identifier's own text as a value. `SELECT id, nosuchcolumn FROM t`
    // put a fabricated column beside a real one in the same row.
    for sql in [
        "SELECT nosuchcolumn",
        "SELECT nosuchcolumn FROM t",
        "SELECT id, nosuchcolumn FROM t",
        // What MySQL 8.4.11's own client probes a connection with. Measured
        // there: `select $$` is 1064 and `SELECT $` is 1054. Either way it
        // is an error, and the client stays in step with the server.
        "SELECT $$",
        "SELECT $",
    ] {
        assert!(
            matches!(
                connection.prepare(sql),
                Err(LimboError::NoSuchColumn { .. })
            ),
            "{sql}"
        );
    }

    // A double-quoted string is still a string outside ANSI_QUOTES, which
    // is what MySQL does, and real columns still resolve.
    let mut quoted = connection.prepare("SELECT \"literal\"")?;
    assert_eq!(quoted.run_collect_rows()?[0][0].to_string(), "literal");
    let mut real = connection.prepare("SELECT id, note FROM t")?;
    let rows = real.run_collect_rows()?;
    assert_eq!(rows[0][0].to_string(), "1");
    assert_eq!(rows[0][1].to_string(), "a");

    connection.close()?;
    Ok(())
}

#[test]
fn auto_increment_legacy_constructor_has_no_allocator_capability() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database = open_database_with_identity(
        io,
        "mysql-session-auto-increment-no-capability.db",
        OpenFlags::Create,
        [0x53; 16],
    )?;
    let connection = MySqlConnection::new(database.connect()?, binary_context())?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    assert!(matches!(
        connection.execute("INSERT INTO users (name) VALUES ('Ada')"),
        Err(LimboError::ParseError(_))
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn auto_increment_reservation_is_not_rolled_back_with_the_row() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-auto-increment-rollback.db", [0x54; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    connection.inner().execute("BEGIN")?;
    connection.execute("INSERT INTO users (name) VALUES ('rolled back')")?;
    connection.inner().execute("ROLLBACK")?;
    connection.execute("INSERT INTO users (name) VALUES ('kept')")?;
    assert_eq!(
        connection
            .prepare_select("SELECT id, name FROM users")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(2), Value::from_text("kept")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn last_insert_id_tracks_only_successful_generated_inserts() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-last-insert-id.db", [0x56; 16])?;
    let clone = connection.clone();
    assert_eq!(connection.last_insert_id(), 0);
    let mut prepared = connection.prepare_select("SELECT LAST_INSERT_ID()")?;

    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;
    assert_eq!(connection.last_insert_id(), 1);
    assert_eq!(clone.last_insert_id(), 1);
    assert_eq!(prepared.run_collect_rows()?, vec![vec![Value::from_i64(1)]]);
    prepared.reset()?;

    connection.execute("INSERT INTO users (name) VALUES ('Grace'), ('Linus')")?;
    assert_eq!(connection.last_insert_id(), 2);
    assert_eq!(prepared.run_collect_rows()?, vec![vec![Value::from_i64(2)]]);
    prepared.reset()?;

    assert!(connection
        .execute("INSERT INTO users (name) VALUES (upper('failed'))")
        .is_err());
    assert_eq!(connection.last_insert_id(), 2);

    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (9, 'ordinary')")?;
    assert_eq!(connection.last_insert_id(), 2);

    connection.inner().execute("BEGIN")?;
    connection.execute("INSERT INTO users (name) VALUES ('rolled back')")?;
    assert_eq!(connection.last_insert_id(), 4);
    connection.inner().execute("ROLLBACK")?;
    assert_eq!(connection.last_insert_id(), 4);
    assert_eq!(prepared.run_collect_rows()?, vec![vec![Value::from_i64(4)]]);
    prepared.reset()?;
    connection.set_last_insert_id(u64::MAX);
    assert_eq!(
        prepared.run_collect_rows()?,
        vec![vec![Value::from_text(u64::MAX.to_string())]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn checked_write_reports_insert_delete_and_generated_results() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-write.db", [0x57; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;

    let inserted = connection
        .execute_checked_write("INSERT INTO notes (id, body) VALUES (1, 'kept')", None)
        .unwrap();
    assert_eq!(inserted.affected_rows, 1);
    assert_eq!(inserted.last_insert_id, 0);

    let deleted = connection
        .execute_checked_write("DELETE FROM notes WHERE id IS NOT NULL", None)
        .unwrap();
    assert_eq!(deleted.affected_rows, 1);
    assert_eq!(deleted.last_insert_id, 0);
    let deleted_again = connection
        .execute_checked_write("DELETE FROM notes WHERE id IS NOT NULL", None)
        .unwrap();
    assert_eq!(deleted_again.affected_rows, 0);

    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    let generated = connection
        .execute_checked_write("INSERT INTO users (name) VALUES ('Ada'), ('Grace')", None)
        .unwrap();
    assert_eq!(generated.affected_rows, 2);
    assert_eq!(generated.last_insert_id, 1);
    assert_eq!(connection.last_insert_id(), 1);

    assert!(matches!(
        connection.execute_checked_write("DELETE FROM missing", None),
        Err(MySqlQueryError::Engine(_))
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn checked_update_reports_zero_changed_rows_for_no_op_values() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-update-no-op.db", [0x59; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept'), (2, 'kept')")?;

    let changed = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = 'kept' WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(changed.affected_rows, 0);

    let matched = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = 'kept' WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(matched.affected_rows, 2);
    connection.close()?;
    Ok(())
}

/// `AND CHAIN` ends a transaction and begins another at once. Measured on
/// MySQL 8.4.11 with a table holding nothing: `START TRANSACTION; INSERT 5;
/// ROLLBACK AND CHAIN; INSERT 6; ROLLBACK` leaves the table empty, because the
/// second insert was inside the chained transaction. `COMMIT AND CHAIN` with
/// autocommit on leaves the session in a transaction too, with nothing to end.
#[test]
fn chaining_transaction_commands_end_one_and_begin_another() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-chained-transaction.db", [0x67; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;

    connection
        .execute_transaction_command("START TRANSACTION")
        .unwrap();
    connection.execute("INSERT INTO notes (id, body) VALUES (5, 'discarded')")?;
    connection
        .execute_transaction_command("ROLLBACK AND CHAIN")
        .unwrap();
    // The chain left a transaction open, so this write is inside it.
    assert!(!connection.is_auto_commit());
    connection.execute("INSERT INTO notes (id, body) VALUES (6, 'also discarded')")?;
    connection.execute_transaction_command("ROLLBACK").unwrap();
    assert!(connection
        .prepare_select("SELECT id FROM notes")?
        .run_collect_rows()?
        .is_empty());

    // With autocommit on there is nothing to end, and the chain still leaves
    // the session in a transaction.
    assert!(connection.is_auto_commit());
    connection
        .execute_transaction_command("COMMIT AND CHAIN")
        .unwrap();
    assert!(!connection.is_auto_commit());
    connection.execute("INSERT INTO notes (id, body) VALUES (7, 'kept')")?;
    connection.execute_transaction_command("COMMIT").unwrap();
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM notes")?
            .run_collect_rows()?
            .len(),
        1
    );
    connection.close()?;
    Ok(())
}

#[test]
fn explicit_transaction_commands_commit_rollback_and_no_op_when_idle() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-explicit-transaction.db", [0x61; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;

    connection.execute_transaction_command("COMMIT").unwrap();
    connection.execute_transaction_command("ROLLBACK").unwrap();
    assert!(connection.is_auto_commit());

    connection.execute_transaction_command("BEGIN").unwrap();
    assert!(!connection.is_auto_commit());
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 'discarded')")?;
    connection.execute_transaction_command("ROLLBACK").unwrap();
    assert!(connection.is_auto_commit());
    assert!(connection
        .prepare_select("SELECT id FROM notes")?
        .run_collect_rows()?
        .is_empty());

    connection
        .execute_transaction_command("START TRANSACTION")
        .unwrap();
    connection.execute("INSERT INTO notes (id, body) VALUES (2, 'kept')")?;
    connection.execute_transaction_command("COMMIT").unwrap();
    assert_eq!(
        connection
            .prepare_select("SELECT body FROM notes")?
            .run_collect_rows()?,
        vec![vec![Value::from_text("kept")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn autocommit_off_opens_on_write_and_survives_transaction_end() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-autocommit-off.db", [0x62; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;

    connection
        .execute_autocommit_setting("SET SESSION autocommit = 0")
        .unwrap();
    assert!(!connection.session_autocommit());
    assert!(connection.is_auto_commit());

    connection
        .execute_checked_write("INSERT INTO notes (id, body) VALUES (1, 'discarded')", None)
        .unwrap();
    assert!(!connection.is_auto_commit());
    connection.execute_transaction_command("ROLLBACK").unwrap();
    assert!(!connection.session_autocommit());
    assert!(connection.is_auto_commit());

    connection
        .execute_checked_write("INSERT INTO notes (id, body) VALUES (2, 'kept')", None)
        .unwrap();
    connection
        .execute_autocommit_setting("SET autocommit = 1")
        .unwrap();
    assert!(connection.session_autocommit());
    assert!(connection.is_auto_commit());
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM notes")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(2)]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn schema_ddl_commits_prior_work_even_when_the_ddl_fails() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-ddl-implicit-commit.db", [0x65; 16])?;
    connection.execute("CREATE TABLE notes (id INT)")?;
    connection
        .execute_autocommit_setting("SET autocommit = 0")
        .unwrap();
    connection
        .execute_checked_write("INSERT INTO notes (id) VALUES (1)", None)
        .unwrap();

    assert!(connection
        .execute_schema_ddl("CREATE TABLE notes (id INT)")
        .is_err());
    assert!(connection.is_auto_commit());
    assert!(!connection.session_autocommit());

    connection.execute_transaction_command("ROLLBACK").unwrap();
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM notes")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    connection.execute_transaction_command("ROLLBACK").unwrap();
    connection.close()?;
    Ok(())
}

#[test]
fn schema_ddl_commits_prior_work_and_returns_idle_after_success() -> Result<()> {
    for (suffix, setup, ddl, schema_name, schema_kind) in [
        (
            "table",
            None,
            "CREATE TABLE created_table (id INT)",
            "created_table",
            "table",
        ),
        (
            "index",
            Some("CREATE TABLE indexed_notes (body TEXT)"),
            "CREATE INDEX idx_notes_body ON indexed_notes (body)",
            "idx_notes_body",
            "index",
        ),
        (
            "view",
            Some("CREATE TABLE viewed_notes (body TEXT)"),
            "CREATE VIEW notes_view AS SELECT body FROM viewed_notes",
            "notes_view",
            "view",
        ),
        (
            "trigger",
            Some("CREATE TABLE triggered_notes (body TEXT)"),
            "CREATE TRIGGER copy_note AFTER INSERT ON triggered_notes FOR EACH ROW BEGIN INSERT INTO committed_notes (id) VALUES (NEW.rowid); END",
            "copy_note",
            "trigger",
        ),
        (
            "alter",
            Some("CREATE TABLE altered_notes (id INT)"),
            "ALTER TABLE altered_notes ADD COLUMN body TEXT",
            "altered_notes",
            "table",
        ),
    ] {
        let path = format!("mysql-session-ddl-implicit-commit-{suffix}.db");
        let (connection, _allocator, _io) = open_allocator_connection(&path, [0x66; 16])?;
        connection.execute("CREATE TABLE committed_notes (id INT)")?;
        if let Some(setup) = setup {
            connection.execute(setup)?;
        }
        connection
            .execute_autocommit_setting("SET autocommit = 0")
            .unwrap();
        connection
            .execute_checked_write("INSERT INTO committed_notes (id) VALUES (1)", None)
            .unwrap();

        connection.execute_schema_ddl(ddl).unwrap();
        assert!(
            connection.is_auto_commit(),
            "DDL left a transaction active: {ddl}"
        );
        assert!(!connection.session_autocommit());
        let stored_name = if schema_kind == "index" {
            connection
                .inner()
                .current_schema()
                .get_indices("indexed_notes")
                .find(|index| mysql_index_name(index) == schema_name)
                .expect("created index is loaded")
                .name
                .clone()
        } else {
            schema_name.to_owned()
        };
        assert_eq!(
            connection
                .inner()
                .prepare(format!(
                    "SELECT type FROM sqlite_schema WHERE name = '{stored_name}'"
                ))?
                .run_collect_rows()?,
            vec![vec![Value::from_text(schema_kind)]],
            "DDL did not create its schema object: {ddl}"
        );

        connection.execute_transaction_command("ROLLBACK").unwrap();
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM committed_notes")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]],
            "DDL did not commit prior work: {ddl}"
        );
        connection.execute_transaction_command("ROLLBACK").unwrap();
        connection.close()?;
    }
    Ok(())
}

#[test]
fn autocommit_off_opens_on_table_select_but_not_constant_select() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-autocommit-select.db", [0x63; 16])?;
    connection.execute("CREATE TABLE notes (id INT)")?;
    connection
        .execute_autocommit_setting("SET autocommit = 0")
        .unwrap();

    connection.prepare_select("SELECT 1")?.run_collect_rows()?;
    assert!(connection.is_auto_commit());

    connection
        .prepare_select("SELECT id FROM notes")?
        .run_collect_rows()?;
    assert!(!connection.is_auto_commit());

    connection.execute_transaction_command("ROLLBACK").unwrap();
    connection.close()?;
    Ok(())
}

#[test]
fn autocommit_off_table_select_starts_before_engine_prepare_error() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-autocommit-select-error.db", [0x64; 16])?;
    connection
        .execute_autocommit_setting("SET autocommit = 0")
        .unwrap();

    assert!(matches!(
        connection.prepare_select("SELECT id FROM missing_table"),
        Err(MySqlQueryError::Engine(_))
    ));
    assert!(!connection.is_auto_commit());

    connection.execute_transaction_command("ROLLBACK").unwrap();
    connection.close()?;
    Ok(())
}

#[test]
fn checked_replace_counts_each_deleted_row_and_each_inserted_row() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-replace-affected-rows.db", [0x83; 16])?;
    connection.execute("CREATE TABLE notes (id INT PRIMARY KEY, body INT UNIQUE)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 10), (2, 20)")?;

    let new_row = connection
        .execute_checked_write("REPLACE INTO notes (id, body) VALUES (3, 30)", None)
        .unwrap();
    assert_eq!(new_row.affected_rows, 1);

    let one_conflict = connection
        .execute_checked_write("REPLACE INTO notes (id, body) VALUES (1, 11)", None)
        .unwrap();
    assert_eq!(one_conflict.affected_rows, 2);

    let two_conflicts = connection
        .execute_checked_write("REPLACE INTO notes (id, body) VALUES (1, 20)", None)
        .unwrap();
    assert_eq!(two_conflicts.affected_rows, 3);

    let mixed = connection
        .execute_checked_write(
            "REPLACE INTO notes (id, body) VALUES (1, 21), (4, 40)",
            None,
        )
        .unwrap();
    assert_eq!(mixed.affected_rows, 3);
    connection.close()?;
    Ok(())
}

#[test]
fn checked_upsert_counts_insert_change_and_no_op() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-upsert-affected-rows.db", [0x84; 16])?;
    connection.execute("CREATE TABLE notes (id INT PRIMARY KEY, body INT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 10)")?;

    let inserted = connection
        .execute_checked_write(
            "INSERT INTO notes (id, body) VALUES (2, 20) ON DUPLICATE KEY UPDATE body = 99",
            None,
        )
        .unwrap();
    assert_eq!(inserted.affected_rows, 1);

    let changed = connection
        .execute_checked_write(
            "INSERT INTO notes (id, body) VALUES (1, 11) ON DUPLICATE KEY UPDATE body = 99",
            None,
        )
        .unwrap();
    assert_eq!(changed.affected_rows, 2);

    let no_op = connection
        .execute_checked_write(
            "INSERT INTO notes (id, body) VALUES (1, 11) ON DUPLICATE KEY UPDATE body = 99",
            None,
        )
        .unwrap();
    assert_eq!(no_op.affected_rows, 0);

    let found_no_op = connection
        .execute_checked_write_with_affected_rows_mode(
            "INSERT INTO notes (id, body) VALUES (1, 11) ON DUPLICATE KEY UPDATE body = 99",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(found_no_op.affected_rows, 1);

    let found_inserted = connection
        .execute_checked_write_with_affected_rows_mode(
            "INSERT INTO notes (id, body) VALUES (4, 40) ON DUPLICATE KEY UPDATE body = 100",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(found_inserted.affected_rows, 1);

    let found_changed = connection
        .execute_checked_write_with_affected_rows_mode(
            "INSERT INTO notes (id, body) VALUES (2, 20) ON DUPLICATE KEY UPDATE body = 100",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(found_changed.affected_rows, 2);

    let mixed = connection
        .execute_checked_write(
            "INSERT INTO notes (id, body) VALUES (3, 30), (1, 11) ON DUPLICATE KEY UPDATE body = 100",
            None,
        )
        .unwrap();
    assert_eq!(mixed.affected_rows, 3);

    let found_mixed = connection
        .execute_checked_write_with_affected_rows_mode(
            "INSERT INTO notes (id, body) VALUES (5, 50), (1, 11) ON DUPLICATE KEY UPDATE body = 100",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(found_mixed.affected_rows, 2);
    connection.close()?;
    Ok(())
}

#[test]
fn checked_update_reports_changed_rows_for_actual_values() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-update-actual.db", [0x5a; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 'before'), (2, 'before')")?;

    let changed = connection
        .execute_checked_write("UPDATE notes SET body = 'after' WHERE TRUE", None)
        .unwrap();
    assert_eq!(changed.affected_rows, 2);

    let matched = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = 'again' WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(matched.affected_rows, 2);
    connection.close()?;
    Ok(())
}

#[test]
fn checked_update_allows_auto_increment_tables_but_uninjected_inserts_stay_rejected() -> Result<()>
{
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-update-auto-increment.db", [0x60; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;

    let updated = connection
        .execute_checked_write("UPDATE users SET name = 'Grace' WHERE TRUE", None)
        .unwrap();
    assert_eq!(updated.affected_rows, 1);
    assert_eq!(
        connection
            .prepare_select("SELECT id, name FROM users")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1), Value::from_text("Grace")]]
    );

    let key_updated = connection
        .execute_checked_write("UPDATE users SET id = 7 WHERE TRUE", None)
        .unwrap();
    assert_eq!(key_updated.affected_rows, 1);
    assert!(matches!(
        connection.execute_checked_write(
            "UPDATE users SET name = 'unsafe', id = (id) WHERE TRUE",
            None,
        ),
        Err(MySqlQueryError::Unsupported(_))
    ));
    let unchanged = connection
        .execute_checked_write("UPDATE users SET id = ID WHERE TRUE", None)
        .unwrap();
    assert_eq!(unchanged.affected_rows, 0);

    let next = connection
        .execute_checked_write("INSERT INTO users (name) VALUES ('Linus')", None)
        .unwrap();
    assert_eq!(next.last_insert_id, 8);

    assert!(matches!(
        connection
            .prepare("INSERT INTO users (id, name) VALUES (10, 'unmanaged')")?
            .run_ignore_rows(),
        Err(LimboError::ParseError(message)) if message == "MySQL AUTO_INCREMENT inserts are not enabled"
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn rolled_back_auto_increment_key_update_burns_the_advanced_value() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-checked-update-auto-increment-rollback.db",
        [0x67; 16],
    )?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;

    connection.execute_transaction_command("BEGIN").unwrap();
    connection
        .execute_checked_write("UPDATE users SET id = 20 WHERE TRUE", None)
        .unwrap();
    connection.execute_transaction_command("ROLLBACK").unwrap();

    assert_eq!(
        connection
            .prepare_select("SELECT id FROM users")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    let generated = connection
        .execute_checked_write("INSERT INTO users (name) VALUES ('Grace')", None)
        .unwrap();
    assert_eq!(generated.last_insert_id, 21);
    connection.close()?;
    Ok(())
}

#[test]
fn failed_auto_increment_key_update_burns_the_advanced_value() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-checked-update-auto-increment-failure.db",
        [0x68; 16],
    )?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    connection.execute("INSERT INTO users (name) VALUES ('Ada'), ('Grace')")?;

    assert!(connection
        .execute_checked_write("UPDATE users SET id = 30 WHERE TRUE", None)
        .is_err());
    let generated = connection
        .execute_checked_write("INSERT INTO users (name) VALUES ('Linus')", None)
        .unwrap();
    assert_eq!(generated.last_insert_id, 31);
    connection.close()?;
    Ok(())
}

#[test]
fn checked_update_distinguishes_mixed_changed_and_matched_rows() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-update-mixed.db", [0x5b; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute(
        "INSERT INTO notes (id, body) VALUES (1, 'kept'), (2, 'replace'), (3, 'replace')",
    )?;

    let changed = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = 'kept' WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(changed.affected_rows, 2);

    connection.execute("UPDATE notes SET body = 'replace' WHERE TRUE")?;
    connection.execute("UPDATE notes SET body = 'kept' WHERE TRUE")?;
    let matched = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = 'kept' WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(matched.affected_rows, 3);
    connection.close()?;
    Ok(())
}

#[test]
fn checked_update_counts_null_assignments_by_stored_value_change() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-update-null.db", [0x5c; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, NULL), (2, 'present')")?;

    let changed = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = NULL WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(changed.affected_rows, 1);

    let matched = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = NULL WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Matched,
        )
        .unwrap();
    assert_eq!(matched.affected_rows, 2);
    connection.close()?;
    Ok(())
}

#[test]
fn failed_checked_update_does_not_return_an_affected_row_count() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-update-failed.db", [0x5d; 16])?;
    connection.execute("CREATE TABLE notes (id INTEGER UNIQUE, label TEXT, body TEXT)")?;
    connection.execute(
        "INSERT INTO notes (id, label, body) VALUES (1, 'first', 'kept'), (2, 'second', 'kept')",
    )?;

    assert!(matches!(
        connection.execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET id = 1 WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Changed,
        ),
        Err(MySqlQueryError::Engine(_))
    ));
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT label FROM notes ORDER BY id")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_text("first")],
            vec![Value::from_text("second")],
        ]
    );
    let no_op = connection
        .execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = 'kept' WHERE TRUE",
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(no_op.affected_rows, 0);
    connection.close()?;
    Ok(())
}

#[test]
fn checked_update_deadline_interrupts_before_mutating_rows() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-checked-update-timeout.db", [0x5e; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept')")?;

    assert!(matches!(
        connection.execute_checked_write_with_affected_rows_mode(
            "UPDATE notes SET body = 'late' WHERE TRUE",
            Some(Duration::ZERO),
            MySqlAffectedRowsMode::Changed,
        ),
        Err(MySqlQueryError::Engine(LimboError::Interrupt))
    ));
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT body FROM notes")?
            .run_collect_rows()?,
        vec![vec![Value::from_text("kept")]]
    );
    connection.close()?;
    Ok(())
}

/// The row of defaults takes a number on a counted table, the way every other
/// row does. Measured on MySQL 8.4.11: `INSERT INTO t () VALUES ()` writes one
/// row taking the next number, every other column taking its own default.
#[test]
fn empty_default_insert_numbers_the_row_it_writes() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-empty-default-auto.db", [0xa2; 16])?;
    connection.execute(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, value INT DEFAULT 7)",
    )?;
    let written = connection
        .execute_checked_write("INSERT INTO users () VALUES ()", None)
        .unwrap();
    assert_eq!((written.affected_rows, written.last_insert_id), (1, 1));
    connection
        .execute_checked_write("INSERT INTO users (value) VALUES (8)", None)
        .unwrap();
    assert_eq!(
        connection
            .prepare_select("SELECT id, value FROM users")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(1), Value::from_i64(7)],
            vec![Value::from_i64(2), Value::from_i64(8)],
        ]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn checked_write_zero_timeout_changes_nothing() -> Result<()> {
    let (connection, allocator, io) =
        open_allocator_connection("mysql-session-checked-write-timeout.db", [0x58; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;

    assert!(matches!(
        connection.execute_checked_write(
            "INSERT INTO notes (id, body) VALUES (1, 'late')",
            Some(Duration::ZERO),
        ),
        Err(MySqlQueryError::Engine(LimboError::Interrupt))
    ));
    assert!(connection
        .inner()
        .prepare("SELECT id FROM notes")?
        .run_collect_rows()?
        .is_empty());

    assert!(matches!(
        connection.execute_checked_write(
            "INSERT INTO users (name) VALUES ('late')",
            Some(Duration::ZERO),
        ),
        Err(MySqlQueryError::Engine(LimboError::Interrupt))
    ));
    let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
    let range = io.block(|| reservation.step())?;
    assert_eq!(range.first(), 1);
    connection.close()?;
    Ok(())
}

#[test]
fn empty_mysql_database_persists_the_format_v2_policy_marker() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-empty-format-v2.db";
    let marker =
        DatabaseFileOwner::mysql_application_id(DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES)
            as i64;

    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = db.connect()?;
        assert_eq!(
            connection
                .prepare("PRAGMA application_id")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(marker)]]
        );
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = db.connect()?;
    assert_eq!(
        connection
            .prepare("PRAGMA application_id")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(marker)]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn auto_increment_ddl_persists_trusted_identities_and_reopens_fail_closed() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-auto-increment-v2.db";
    let database_identity = [0x31; 16];
    let db = open_database_with_identity(io.clone(), path, OpenFlags::Create, database_identity)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute(
        "CREATE TABLE `users` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY, `name` TEXT)",
    )?;

    let rows = connection
        .inner()
        .prepare("SELECT sql FROM sqlite_schema WHERE name = 'users'")?
        .run_collect_rows()?;
    let stored = rows[0][0].to_string();
    let decoded = decode_schema_sql(SchemaSqlKind::Table, stored.trim_matches('\''))
        .map_err(|error| LimboError::Corrupt(error.to_string()))?
        .expect("AUTO_INCREMENT DDL must use a v2 schema envelope");
    let metadata = decoded
        .v2_metadata()
        .expect("AUTO_INCREMENT DDL must persist both identities");
    assert_eq!(metadata.database_id.into_bytes(), database_identity);
    assert_ne!(metadata.allocator_id.into_bytes(), [0; 16]);

    let insert_error = connection
        .inner()
        .execute("INSERT INTO users(name) VALUES ('Ada')")
        .unwrap_err();
    assert!(matches!(insert_error, LimboError::ParseError(_)));
    // A counted table's schema can be altered: the marker that says it is
    // counted rides through the rewrite, and the counted column is written
    // back the way MySQL declared it.
    connection
        .prepare("ALTER TABLE users ADD COLUMN email TEXT")
        .expect("a counted table takes an ADD COLUMN");
    connection.close()?;
    drop(connection);
    drop(db);

    let wrong_identity = open_database_with_identity(io.clone(), path, OpenFlags::None, [0x32; 16]);
    let Err(wrong_identity) = wrong_identity else {
        panic!("a v2 schema must reject a different durable database identity");
    };
    assert!(matches!(wrong_identity, LimboError::Corrupt(_)));

    let db = open_database_with_identity(io, path, OpenFlags::None, database_identity)?;
    let connection = db.connect()?;
    assert_eq!(
        connection
            .prepare("SELECT count(*) FROM sqlite_schema WHERE name = 'users'")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn auto_increment_ddl_requires_a_durable_database_identity() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-auto-increment-no-id.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    let error = connection
        .prepare("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")
        .unwrap_err();
    assert!(matches!(error, LimboError::ParseError(_)));
    assert!(connection
        .inner()
        .prepare("SELECT 1 FROM sqlite_schema WHERE name = 'users'")?
        .run_collect_rows()?
        .is_empty());
    connection.close()?;
    Ok(())
}

#[test]
fn auto_increment_ddl_rejects_non_main_catalog_targets() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database_with_identity(
        io,
        "mysql-session-auto-increment-main-only.db",
        OpenFlags::Create,
        [0x41; 16],
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;

    for sql in [
        "CREATE TABLE app.users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        "CREATE TEMPORARY TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
    ] {
        assert!(
            connection.prepare(sql).is_err(),
            "expected AUTO_INCREMENT target to be rejected: {sql}"
        );
    }
    assert!(connection
        .inner()
        .prepare("SELECT 1 FROM sqlite_schema WHERE name = 'users'")?
        .run_collect_rows()?
        .is_empty());
    connection.close()?;
    Ok(())
}

#[test]
fn create_table_persists_marker_and_reopens_with_mysql_dialect() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-create-reopen.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`id` INTEGER NOT NULL UNIQUE, `name` TEXT)")?;

        let rows = connection
            .inner()
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 'users'")?
            .run_collect_rows()?;
        assert_eq!(rows.len(), 1);
        assert!(rows[0][0]
            .to_string()
            .trim_matches('\'')
            .starts_with("/*@turso:mysql-schema:v3:"));
        connection.inner().close()?;
    }

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = db.connect()?;
        connection.execute("INSERT INTO users VALUES (1, 'Ada')")?;
        connection.execute("VACUUM")?;
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = db.connect()?;
    assert_eq!(
        connection
            .prepare("SELECT name FROM users WHERE id = 1")?
            .run_collect_rows()?,
        vec![vec![Value::build_text("Ada")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn fresh_text_collations_keep_equality_like_and_unique_keys_after_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-uca9-and-pad-space.db";
    for flags in [OpenFlags::Create, OpenFlags::None] {
        let db = open_database(io.clone(), path, flags)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        if flags == OpenFlags::Create {
            connection.execute(
                "CREATE TABLE names (id INT, name VARCHAR(16) UNIQUE, exact VARCHAR(16) COLLATE utf8mb4_bin UNIQUE)",
            )?;
            connection.execute("INSERT INTO names (id, name, exact) VALUES (1, 'café', 'a')")?;
            connection.execute("INSERT INTO names (id, name, exact) VALUES (2, 'other', 'B')")?;
        }
        for sql in [
            "SELECT id FROM names WHERE name = 'cafe'",
            "SELECT id FROM names WHERE name LIKE 'cafe'",
            "SELECT id FROM names WHERE exact = 'a  '",
            "SELECT id FROM names WHERE exact = 'a  ' COLLATE utf8mb4_bin",
            "SELECT id FROM names WHERE exact IN ('a  ')",
        ] {
            assert_eq!(
                connection
                    .prepare_select(sql)
                    .map_err(|error| LimboError::InternalError(error.to_string()))?
                    .run_collect_rows()?,
                vec![vec![Value::from_i64(1)]],
                "{sql}"
            );
        }
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM names ORDER BY exact")
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(2)], vec![Value::from_i64(1)]]
        );
        assert!(connection
            .prepare_select("SELECT id FROM names WHERE exact LIKE 'a%'")
            .is_err());
        assert!(connection
            .execute("INSERT INTO names (id, name, exact) VALUES (3, 'CAFE', 'c')")
            .is_err());
        assert!(connection
            .execute("INSERT INTO names (id, name, exact) VALUES (4, 'new', 'a  ')")
            .is_err());
        connection.close()?;
    }
    Ok(())
}

#[test]
fn scalar_text_calls_use_mysql_case_rules_or_refuse_unsupported_collations() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-scalar-text-calls.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection
        .execute("CREATE TABLE words (v VARCHAR(16), exact VARCHAR(16) COLLATE utf8mb4_bin)")?;
    connection.execute("INSERT INTO words (v, exact) VALUES ('ABC', 'ABC')")?;

    for (sql, expected) in [
        ("SELECT LOWER(v) FROM words", Value::build_text("abc")),
        ("SELECT UPPER(v) FROM words", Value::build_text("ABC")),
        ("SELECT INSTR(v, 'a') FROM words", Value::from_i64(1)),
        ("SELECT LOCATE('b', v) FROM words", Value::from_i64(2)),
        ("SELECT LOCATE('b', v, 3) FROM words", Value::from_i64(0)),
        ("SELECT FIELD(v, 'abc') FROM words", Value::from_i64(1)),
        (
            "SELECT GREATEST(v, 'abc') FROM words",
            Value::build_text("abc"),
        ),
        (
            "SELECT LEAST(v, 'abc') FROM words",
            Value::build_text("ABC"),
        ),
        ("SELECT NULLIF(v, 'abc') FROM words", Value::Null),
    ] {
        assert_eq!(
            connection
                .prepare_select(sql)
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .run_collect_rows()?,
            vec![vec![expected]],
            "{sql}"
        );
    }
    for sql in [
        "SELECT INSTR(exact, 'a') FROM words",
        "SELECT LOCATE('a', exact) FROM words",
        "SELECT FIELD(exact, 'abc') FROM words",
        "SELECT GREATEST(exact, 'abc') FROM words",
        "SELECT LEAST(exact, 'abc') FROM words",
        "SELECT NULLIF(exact, 'abc') FROM words",
    ] {
        assert!(connection.prepare_select(sql).is_err(), "{sql}");
    }

    connection.execute("INSERT INTO words (v, exact) VALUES ('Éclair', 'Éclair')")?;
    for sql in [
        "SELECT LOWER(v) FROM words WHERE exact = 'Éclair'",
        "SELECT UPPER(v) FROM words WHERE exact = 'Éclair'",
        "SELECT INSTR(v, 'é') FROM words WHERE exact = 'Éclair'",
        "SELECT LOCATE('é', v) FROM words WHERE exact = 'Éclair'",
    ] {
        assert!(
            connection
                .prepare_select(sql)
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .run_collect_rows()
                .is_err(),
            "{sql}"
        );
    }
    connection.close()?;
    Ok(())
}

#[test]
fn ordinary_integer_primary_keys_keep_mysql_metadata_without_a_rowid_alias() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-ordinary-primary-key-reopen.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        for (table_name, type_name) in [("int_keys", "INT"), ("integer_keys", "INTEGER")] {
            let ddl = format!(
                "CREATE TABLE `{table_name}` (`id` {type_name} PRIMARY KEY, `name` TEXT) ENGINE = InnoDB"
            );
            connection.execute(&ddl)?;

            let columns = connection
                .list_columns(&MySqlTableName::parse(table_name).unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?;
            assert_eq!(columns[0].type_name(), "INT");
            assert!(!columns[0].nullable());
            assert_eq!(columns[0].key(), MySqlColumnKey::Primary);
            assert!(columns[0].extra.is_empty());

            let table = connection
                .inner
                .current_schema()
                .get_table(table_name)
                .ok_or_else(|| LimboError::InternalError(format!("missing table {table_name}")))?;
            let btree = table.btree().ok_or_else(|| {
                LimboError::InternalError(format!("missing btree for {table_name}"))
            })?;
            assert!(btree.get_rowid_alias_column().is_none());
            assert!(btree.unique_sets.iter().any(|set| set.is_primary_key));

            let rows = connection
                .inner()
                .prepare(format!(
                    "SELECT sql FROM sqlite_schema WHERE name = '{table_name}'"
                ))?
                .run_collect_rows()?;
            let [row] = rows.as_slice() else {
                return Err(LimboError::InternalError(format!(
                    "expected one sqlite_schema row for {table_name}"
                )));
            };
            let stored = row[0].to_string();
            let decoded = decode_schema_sql(SchemaSqlKind::Table, stored.trim_matches('\''))
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .ok_or_else(|| {
                    LimboError::InternalError(format!("missing MySQL marker for {table_name}"))
                })?;
            assert!(decoded
                .normalized_ddl
                .contains(&format!("`id` {type_name}")));
            assert!(decoded.normalized_ddl.ends_with("ENGINE = InnoDB"));

            let insert = format!("INSERT INTO `{table_name}` (`id`, `name`) VALUES (1, 'first')");
            connection.execute(&insert)?;
            let duplicate =
                format!("INSERT INTO `{table_name}` (`id`, `name`) VALUES (1, 'duplicate')");
            assert!(connection.execute(&duplicate).is_err());
        }
        connection.inner().close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    for table_name in ["int_keys", "integer_keys"] {
        let columns = connection
            .list_columns(&MySqlTableName::parse(table_name).unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(columns[0].type_name(), "INT");
        assert_eq!(columns[0].key(), MySqlColumnKey::Primary);
    }
    connection.inner().execute("VACUUM")?;
    for table_name in ["int_keys", "integer_keys"] {
        let insert = format!("INSERT INTO `{table_name}` (`id`, `name`) VALUES (2, 'second')");
        connection.execute(&insert)?;
    }
    connection.inner().close()?;
    Ok(())
}

#[test]
fn alter_table_preserves_marker_context_through_reopen_and_vacuum() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-alter-reopen.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`id` INTEGER, `old_name` TEXT)")?;
        let expected_context = stored_schema_context(&connection, "users")?;

        connection.execute("ALTER TABLE `users` ADD COLUMN `email` TEXT")?;
        assert_eq!(
            stored_schema_context(&connection, "users")?,
            expected_context
        );
        connection.execute("ALTER TABLE `users` RENAME COLUMN `old_name` TO `name`")?;
        assert_eq!(
            stored_schema_context(&connection, "users")?,
            expected_context
        );
        connection.execute("ALTER TABLE `users` DROP COLUMN `email`")?;
        assert_eq!(
            stored_schema_context(&connection, "users")?,
            expected_context
        );
        connection.execute("ALTER TABLE `users` RENAME TO `accounts`")?;
        assert_eq!(
            stored_schema_context(&connection, "accounts")?,
            expected_context
        );
        connection.inner().close()?;
    }

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = db.connect()?;
        connection.execute("INSERT INTO accounts VALUES (1, 'Ada')")?;
        connection.execute("VACUUM")?;
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = db.connect()?;
    assert_eq!(
        connection
            .prepare("SELECT name FROM accounts WHERE id = 1")?
            .run_collect_rows()?,
        vec![vec![Value::build_text("Ada")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn create_then_alter_in_transaction_preserves_marker_context_through_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-transaction-reopen.db";
    let expected_context;
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.inner().execute("BEGIN")?;
        connection.execute("CREATE TABLE `users` (`id` INTEGER)")?;
        expected_context = stored_schema_context(&connection, "users")?;
        connection.execute("ALTER TABLE `users` ADD COLUMN `name` TEXT")?;
        assert_eq!(
            stored_schema_context(&connection, "users")?,
            expected_context
        );
        connection.inner().execute("COMMIT")?;
        connection.inner().close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        stored_schema_context(&connection, "users")?,
        expected_context
    );
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT name FROM users WHERE id = 1")?
            .run_collect_rows()?,
        Vec::<Vec<Value>>::new()
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn vacuum_into_preserves_mysql_marker_and_reopens_with_mysql_dialect() -> Result<()> {
    let temp_dir = tempfile::tempdir().map_err(|error| {
        LimboError::InternalError(format!("failed to create vacuum output directory: {error}"))
    })?;
    let output_path = temp_dir.path().join("mysql-vacuum-into-output.db");
    let output_path = output_path.to_str().ok_or_else(|| {
        LimboError::InternalError("vacuum output path is not valid UTF-8".to_string())
    })?;
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let expected_context;
    {
        let db = open_database(io, "mysql-session-vacuum-into-source.db", OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`id` INTEGER, `name` TEXT)")?;
        expected_context = stored_schema_context(&connection, "users")?;
        connection
            .inner()
            .execute(format!("VACUUM INTO '{output_path}'"))?;
        connection.inner().close()?;
    }

    let output_io: Arc<dyn IO> = Arc::new(PlatformIO::new()?);
    let db = open_database(output_io, output_path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        stored_schema_context(&connection, "users")?,
        expected_context
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn create_index_preserves_its_marker_through_schema_rewrites_and_vacuum() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-index-reopen.db";
    let expected_context = binary_context().for_kind(SchemaSqlKind::Index);
    let stored_index_name = {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`id` INTEGER, `name` TEXT)")?;
        connection
            .execute("CREATE INDEX `idx_users_name` ON `users` (`name`)")
            .map_err(|error| {
                LimboError::InternalError(format!("create marked index failed: {error}"))
            })?;
        let stored_index_name = connection
            .inner()
            .current_schema()
            .get_indices("users")
            .find(|index| mysql_index_name(index) == "idx_users_name")
            .expect("created index is loaded")
            .name
            .clone();
        assert_eq!(
            stored_schema_context_for_kind(&connection, &stored_index_name, SchemaSqlKind::Index,)?,
            expected_context
        );

        connection
            .execute("ALTER TABLE `users` RENAME COLUMN `name` TO `display_name`")
            .map_err(|error| {
                LimboError::InternalError(format!("rename marked index column failed: {error}"))
            })?;
        assert_eq!(
            stored_schema_context_for_kind(&connection, &stored_index_name, SchemaSqlKind::Index,)?,
            expected_context
        );
        connection
            .execute("ALTER TABLE `users` RENAME TO `accounts`")
            .map_err(|error| {
                LimboError::InternalError(format!("rename marked index table failed: {error}"))
            })?;
        assert_eq!(
            stored_schema_context_for_kind(&connection, &stored_index_name, SchemaSqlKind::Index,)?,
            expected_context
        );
        connection.inner().close()?;
        stored_index_name
    };

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = db.connect()?;
        connection.execute("INSERT INTO accounts VALUES (1, 'Ada')")?;
        let plan = connection
            .prepare("EXPLAIN QUERY PLAN SELECT id FROM accounts WHERE display_name = 'Ada'")?
            .run_collect_rows()?;
        assert!(
            plan.iter()
                .flat_map(|row| row.iter())
                .any(|value| value.to_string().contains(&stored_index_name)),
            "expected index lookup plan, got {plan:?}"
        );
        connection.execute("VACUUM")?;
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        stored_schema_context_for_kind(&connection, &stored_index_name, SchemaSqlKind::Index)?,
        expected_context
    );
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT id FROM accounts WHERE display_name = 'Ada'")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn secondary_index_on_primary_key_columns_is_visible_and_droppable() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-secondary-on-primary.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT NOT NULL PRIMARY KEY)")?;
    connection.execute("CREATE INDEX by_id ON records (id)")?;
    let table = MySqlTableName::parse("records").unwrap();
    assert_eq!(
        connection
            .list_indexes(&table)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["PRIMARY", "by_id"]
    );

    let drop = turso_mysql_parser::parse_optional_alter_table_indexes(
        "ALTER TABLE records DROP INDEX by_id",
        connection.parser_mode(),
    )
    .unwrap()
    .unwrap();
    connection.execute_alter_table_indexes(&drop).unwrap();
    assert_eq!(
        connection
            .list_indexes(&table)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["PRIMARY"]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn rewriting_a_referenced_table_cannot_change_its_foreign_key_target() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-referenced-rewrite.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE parent (id INT NOT NULL PRIMARY KEY)")?;
    connection.execute("INSERT INTO parent (id) VALUES (1)")?;
    connection.execute(
        "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT, \
         CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parent (id))",
    )?;

    assert!(matches!(
        connection.execute_schema_ddl("ALTER TABLE parent ADD COLUMN first_column INT FIRST"),
        Err(MySqlQueryError::Unsupported(_))
    ));
    let schema = connection.inner().current_schema();
    let child = schema.get_btree_table("child").unwrap();
    assert_eq!(child.foreign_keys[0].parent_table, "parent");
    assert!(schema
        .get_btree_table("parent")
        .unwrap()
        .columns()
        .iter()
        .all(|column| column.name.as_deref() != Some("first_column")));
    drop(schema);
    connection.execute("INSERT INTO child (id, parent_id) VALUES (1, 1)")?;
    connection.close()?;
    Ok(())
}

#[test]
fn the_same_logical_index_name_on_two_tables_survives_drop_and_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-table-local-index-names.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE left_side (id INT, value INT)")?;
        connection.execute("CREATE TABLE right_side (id INT, value INT)")?;
        connection.execute("CREATE INDEX by_value ON left_side (value)")?;
        connection.execute("CREATE INDEX by_value ON right_side (value)")?;
        assert!(matches!(
            connection.execute_schema_ddl("CREATE INDEX by_value ON left_side (id)"),
            Err(MySqlQueryError::DuplicateIndex)
        ));
        let schema = connection.inner().current_schema();
        let left = schema.get_indices("left_side").next().expect("left index");
        let right = schema
            .get_indices("right_side")
            .next()
            .expect("right index");
        assert_ne!(left.name, right.name);
        assert_eq!(mysql_index_name(left), "by_value");
        assert_eq!(mysql_index_name(right), "by_value");
        assert_eq!(
            connection
                .list_indexes(&MySqlTableName::parse("right_side").unwrap())
                .unwrap()
                .iter()
                .map(MySqlIndexEntry::key_name)
                .collect::<Vec<_>>(),
            vec!["by_value"]
        );

        let drop = turso_mysql_parser::parse_optional_alter_table_indexes(
            "ALTER TABLE left_side DROP INDEX by_value",
            connection.parser_mode(),
        )
        .unwrap()
        .unwrap();
        connection.execute_alter_table_indexes(&drop).unwrap();
        assert!(connection
            .inner()
            .current_schema()
            .get_indices("left_side")
            .next()
            .is_none());
        assert_eq!(
            connection
                .inner()
                .current_schema()
                .get_indices("right_side")
                .next()
                .map(|index| mysql_index_name(index)),
            Some("by_value".to_owned())
        );
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .list_indexes(&MySqlTableName::parse("right_side").unwrap())
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["by_value"]
    );
    assert!(connection
        .inner()
        .current_schema()
        .get_indices("left_side")
        .next()
        .is_none());
    connection.close()?;
    Ok(())
}

#[test]
fn foreign_keys_create_and_reuse_child_indexes() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-foreign-key-indexes.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE parent (id INT PRIMARY KEY)")?;

    let child = turso_mysql_parser::parse_optional_create_table_with_keys(
        "CREATE TABLE child (a INT, b INT, CONSTRAINT fk_a FOREIGN KEY (a) REFERENCES parent(id))",
        connection.parser_mode(),
    )
    .unwrap()
    .expect("FK-only CREATE TABLE uses the atomic index path");
    connection.execute_create_table_with_keys(&child)?;
    let child_name = MySqlTableName::parse("child").unwrap();
    assert_eq!(
        connection
            .list_indexes(&child_name)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["fk_a"]
    );
    assert!(connection
        .show_create_table(&child_name)
        .unwrap()
        .create_statement
        .contains("KEY `fk_a` (`a`)"));

    let drop = turso_mysql_parser::parse_optional_alter_table_indexes(
        "ALTER TABLE child DROP INDEX fk_a",
        connection.parser_mode(),
    )
    .unwrap()
    .unwrap();
    assert!(matches!(
        connection.execute_alter_table_indexes(&drop),
        Err(MySqlAlterTableIndexError::RequiredByForeignKey)
    ));
    connection.execute_schema_ddl("CREATE INDEX replacement ON child (a, b)")?;
    assert_eq!(
        connection
            .list_indexes(&child_name)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["replacement", "replacement"]
    );
    connection.execute_schema_ddl("ALTER TABLE child DROP FOREIGN KEY fk_a")?;
    assert_eq!(
        connection
            .list_indexes(&child_name)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["replacement", "replacement"]
    );

    let covered = turso_mysql_parser::parse_optional_create_table_with_keys(
        "CREATE TABLE covered (a INT, b INT, KEY ix_ab (a, b), CONSTRAINT fk_covered FOREIGN KEY (a) REFERENCES parent(id))",
        connection.parser_mode(),
    )
    .unwrap()
    .unwrap();
    connection.execute_create_table_with_keys(&covered)?;
    assert_eq!(
        connection
            .list_indexes(&MySqlTableName::parse("covered").unwrap())
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["ix_ab", "ix_ab"]
    );

    connection.execute("CREATE TABLE altered (a INT)")?;
    connection.execute_schema_ddl(
        "ALTER TABLE altered ADD CONSTRAINT fk_altered FOREIGN KEY (a) REFERENCES parent(id)",
    )?;
    assert_eq!(
        connection
            .list_indexes(&MySqlTableName::parse("altered").unwrap())
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["fk_altered"]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn mixed_index_and_column_alter_is_atomic_and_survives_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-mixed-index-column-alter.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE mixed (a INT, b INT)")?;
        connection.execute_schema_ddl("ALTER TABLE mixed ADD COLUMN c INT, ADD INDEX ix_a (a)")?;
        assert_eq!(
            connection
                .list_indexes(&MySqlTableName::parse("mixed").unwrap())
                .unwrap()
                .iter()
                .map(MySqlIndexEntry::key_name)
                .collect::<Vec<_>>(),
            vec!["ix_a"]
        );
        connection.execute_schema_ddl("ALTER TABLE mixed DROP INDEX ix_a, ADD COLUMN d INT")?;
        assert!(connection
            .list_indexes(&MySqlTableName::parse("mixed").unwrap())
            .unwrap()
            .is_empty());
        assert!(connection
            .execute_schema_ddl("ALTER TABLE mixed ADD INDEX ix_b (b), ADD COLUMN a INT")
            .is_err());
        assert!(connection
            .inner()
            .current_schema()
            .get_indices("mixed")
            .next()
            .is_none());
        connection.close()?;
    }
    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("mixed").unwrap())
            .unwrap()
            .iter()
            .map(MySqlColumnMetadata::name)
            .collect::<Vec<_>>(),
        vec!["a", "b", "c", "d"]
    );
    assert!(connection
        .list_indexes(&MySqlTableName::parse("mixed").unwrap())
        .unwrap()
        .is_empty());
    connection.close()?;
    Ok(())
}

#[test]
fn moving_a_child_column_preserves_its_foreign_key_and_index() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-child-column-position-reopen.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE parent (id INT NOT NULL PRIMARY KEY)")?;
        connection.execute("INSERT INTO parent (id) VALUES (1)")?;
        let child_ddl = turso_mysql_parser::parse_optional_create_table_with_keys(
            "CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT, \
             KEY ix_child_parent (parent_id), \
             CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) REFERENCES parent (id))",
            connection.parser_mode(),
        )
        .unwrap()
        .unwrap();
        connection.execute_create_table_with_keys(&child_ddl)?;
        connection.execute("INSERT INTO child (id, parent_id) VALUES (1, 1)")?;
        connection.execute_schema_ddl("ALTER TABLE child ADD COLUMN first_col INT FIRST")?;
        connection.close()?;
    }
    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    let child = MySqlTableName::parse("child").unwrap();
    assert_eq!(
        connection.list_columns(&child).unwrap()[0].name(),
        "first_col"
    );
    assert_eq!(
        connection
            .list_indexes(&child)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["PRIMARY", "ix_child_parent"]
    );
    assert!(connection
        .show_create_table(&child)
        .unwrap()
        .create_statement
        .contains(
            "CONSTRAINT `fk_child_parent` FOREIGN KEY (`parent_id`) REFERENCES `parent` (`id`)"
        ));
    connection.execute("INSERT INTO child (id, parent_id) VALUES (2, 1)")?;
    assert!(connection
        .execute("INSERT INTO child (id, parent_id) VALUES (3, 999)")
        .is_err());
    connection.close()?;
    Ok(())
}

#[test]
fn mixed_alter_replaces_a_foreign_key_index_atomically() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-mixed-fk-index-replace.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE parent (id INT NOT NULL PRIMARY KEY)")?;
    let child_ddl = turso_mysql_parser::parse_optional_create_table_with_keys(
        "CREATE TABLE child (pid INT, KEY old_pid (pid), \
         CONSTRAINT fk_parent FOREIGN KEY (pid) REFERENCES parent (id))",
        connection.parser_mode(),
    )
    .unwrap()
    .unwrap();
    connection.execute_create_table_with_keys(&child_ddl)?;
    connection.execute_schema_ddl(
        "ALTER TABLE child DROP INDEX old_pid, ADD COLUMN note INT, ADD INDEX new_pid (pid)",
    )?;
    let child = MySqlTableName::parse("child").unwrap();
    assert_eq!(
        connection
            .list_indexes(&child)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["new_pid"]
    );
    assert!(connection
        .execute_schema_ddl("ALTER TABLE child DROP INDEX new_pid, ADD COLUMN bad INT")
        .is_err());
    assert_eq!(
        connection
            .list_indexes(&child)
            .unwrap()
            .iter()
            .map(MySqlIndexEntry::key_name)
            .collect::<Vec<_>>(),
        vec!["new_pid"]
    );
    assert!(connection
        .inner()
        .current_schema()
        .get_btree_table("child")
        .unwrap()
        .columns()
        .iter()
        .all(|column| column.name.as_deref() != Some("bad")));
    connection.close()?;
    Ok(())
}

#[test]
fn legacy_foreign_key_without_child_index_is_rejected_on_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-legacy-foreign-key-index.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE parent (id INT NOT NULL PRIMARY KEY)")?;
        connection
            .prepare("CREATE TABLE child (id INT NOT NULL PRIMARY KEY, parent_id INT, CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parent(id))")?
            .run_ignore_rows()?;
        assert!(!connection
            .inner()
            .current_schema()
            .get_indices("child")
            .any(|index| index_covers_columns(index, &["parent_id".to_owned()])));
        connection.close()?;
    }
    let db = open_database(io, path, OpenFlags::None)?;
    assert!(matches!(
        MySqlConnection::new(db.connect()?, binary_context()),
        Err(LimboError::InvalidArgument(message)) if message.contains("foreign key") && message.contains("child index")
    ));
    Ok(())
}

#[test]
fn json_columns_cannot_be_indexed_after_table_creation() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-json-indexes.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE documents (id INT, doc JSON)")?;
    assert!(matches!(
        connection.execute_schema_ddl("CREATE INDEX doc_key ON documents(doc)"),
        Err(MySqlQueryError::JsonIndex)
    ));
    let added = turso_mysql_parser::parse_optional_alter_table_indexes(
        "ALTER TABLE documents ADD INDEX doc_key(doc)",
        connection.parser_mode(),
    )
    .unwrap()
    .unwrap();
    assert!(matches!(
        connection.execute_alter_table_indexes(&added),
        Err(MySqlAlterTableIndexError::JsonIndex)
    ));
    assert!(connection
        .inner()
        .current_schema()
        .get_indices("documents")
        .next()
        .is_none());
    connection.close()?;
    Ok(())
}

#[test]
fn json_string_comparisons_use_json_string_values_after_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-json-string-comparisons.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE documents (id INT NOT NULL PRIMARY KEY, doc JSON)")?;
        connection.execute(
            "INSERT INTO documents (id, doc) VALUES (1, '\"word\"'), (2, '\"Word\"'), (3, '1'), (4, '{\"a\":1}'), (5, NULL)",
        )?;
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    for (predicate, expected) in [
        ("doc = 'word'", vec![1]),
        ("doc <> 'word'", vec![2, 3, 4]),
        ("doc <=> 'word'", vec![1]),
        ("doc > 'word'", vec![4]),
        ("doc IN ('word', 'Word')", vec![1, 2]),
        ("doc NOT IN ('word', 'Word')", vec![3, 4]),
        ("doc = NULL", vec![]),
        ("doc <=> NULL", vec![5]),
    ] {
        let rows = connection
            .prepare_select(&format!(
                "SELECT id FROM documents WHERE {predicate} ORDER BY id"
            ))
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?;
        assert_eq!(
            rows,
            expected
                .into_iter()
                .map(|id| vec![Value::from_i64(id)])
                .collect::<Vec<_>>(),
            "{predicate}"
        );
    }
    assert!(matches!(
        connection.prepare_select("SELECT id FROM documents WHERE doc = ?"),
        Err(MySqlQueryError::Unsupported(_))
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn json_integer_comparisons_keep_large_integers_exact() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-json-integer-comparisons.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE documents (id INT NOT NULL PRIMARY KEY, doc JSON)")?;
    connection.execute(
        "INSERT INTO documents (id, doc) VALUES (1, '1'), (2, '1.0'), (3, 'true'), (4, '\"1\"'), (5, NULL), (6, '9007199254740992'), (7, '9007199254740993'), (8, '9007199254740992.0'), (9, 'null')",
    )?;
    for (predicate, expected) in [
        ("doc = 1", vec![1, 2]),
        ("doc <> 1", vec![3, 4, 6, 7, 8, 9]),
        ("doc <=> 1", vec![1, 2]),
        ("doc IN (1, '1')", vec![1, 2, 4]),
        ("doc NOT IN (1, '1')", vec![3, 6, 7, 8, 9]),
        ("doc = 9007199254740992", vec![6, 8]),
        ("doc = 9007199254740993", vec![7]),
        ("doc > 1", vec![3, 4, 6, 7, 8]),
        ("doc < 1", vec![9]),
        ("doc < 9007199254740993", vec![1, 2, 6, 8, 9]),
    ] {
        let rows = connection
            .prepare_select(&format!(
                "SELECT id FROM documents WHERE {predicate} ORDER BY id"
            ))
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?;
        assert_eq!(
            rows,
            expected
                .into_iter()
                .map(|id| vec![Value::from_i64(id)])
                .collect::<Vec<_>>(),
            "{predicate}"
        );
    }
    for predicate in ["doc = 1.5", "doc = ?"] {
        assert!(matches!(
            connection.prepare_select(&format!("SELECT id FROM documents WHERE {predicate}")),
            Err(MySqlQueryError::Unsupported(_))
        ));
    }
    assert!(connection
        .prepare_select("SELECT id FROM documents ORDER BY doc")
        .is_err());
    connection.close()?;
    Ok(())
}

#[test]
fn json_scalar_ordering_follows_mysql_type_precedence_after_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-json-type-order.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE documents (id INT NOT NULL PRIMARY KEY, doc JSON)")?;
        connection.execute(
            "INSERT INTO documents (id, doc) VALUES (1,'null'),(2,'false'),(3,'true'),(4,'-2'),(5,'0'),(6,'1'),(7,'1.5'),(8,'\"A\"'),(9,'\"a\"'),(10,'\"1\"'),(11,'[]'),(12,'[1]'),(13,'{}'),(14,'{\"a\":1}'),(15,NULL)",
        )?;
        connection.close()?;
    }
    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    for (predicate, expected) in [
        ("doc > 0", vec![2, 3, 6, 7, 8, 9, 10, 11, 12, 13, 14]),
        ("doc < 0", vec![1, 4]),
        ("doc > 'a'", vec![2, 3, 11, 12, 13, 14]),
        ("doc < 'a'", vec![1, 4, 5, 6, 7, 8, 10]),
        ("doc >= 'a'", vec![2, 3, 9, 11, 12, 13, 14]),
        ("doc <= 0", vec![1, 4, 5]),
    ] {
        let rows = connection
            .prepare_select(&format!(
                "SELECT id FROM documents WHERE {predicate} ORDER BY id"
            ))
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .run_collect_rows()?;
        assert_eq!(
            rows,
            expected
                .into_iter()
                .map(|id| vec![Value::from_i64(id)])
                .collect::<Vec<_>>(),
            "{predicate}"
        );
    }
    connection.close()?;
    Ok(())
}

#[test]
fn create_view_preserves_its_marker_through_reopen_and_vacuum() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-view-reopen.db";
    let expected_context = binary_context().for_kind(SchemaSqlKind::View);
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`id` INTEGER, `name` TEXT)")?;
        connection
            .inner()
            .execute("INSERT INTO users VALUES (1, 'Ada')")?;
        connection.execute("CREATE VIEW `users_view` AS SELECT `name` FROM `users`")?;
        assert_eq!(
            stored_schema_context_for_kind(&connection, "users_view", SchemaSqlKind::View)?,
            expected_context
        );
        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("users_view").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?,
            vec![MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: Some("utf8mb4_0900_ai_ci"),
                name: "name".to_owned(),
                type_name: "TEXT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
                comment: String::new(),
            }]
        );
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT name FROM users_view")?
                .run_collect_rows()?,
            vec![vec![Value::build_text("Ada")]]
        );
        connection.execute("ALTER TABLE `users` ADD COLUMN `email` TEXT")?;
        assert_eq!(
            stored_schema_context(&connection, "users")?,
            binary_context().for_kind(SchemaSqlKind::Table)
        );
        assert!(matches!(
            connection.execute("ALTER TABLE `users` DROP COLUMN `email`"),
            Err(LimboError::ParseError(_))
        ));
        assert!(matches!(
            connection.execute("ALTER TABLE `users` DROP COLUMN `name`"),
            Err(LimboError::ParseError(_))
        ));
        assert!(matches!(
            connection.execute("ALTER TABLE `users` RENAME COLUMN `name` TO `display_name`"),
            Err(LimboError::ParseError(_))
        ));
        assert!(matches!(
            connection.execute("ALTER TABLE `users` RENAME TO `accounts`"),
            Err(LimboError::ParseError(_))
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT name FROM users_view")?
                .run_collect_rows()?,
            vec![vec![Value::build_text("Ada")]]
        );
        connection.inner().close()?;
    }

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = db.connect()?;
        assert_eq!(
            connection
                .prepare("SELECT name FROM users_view")?
                .run_collect_rows()?,
            vec![vec![Value::build_text("Ada")]]
        );
        connection.execute("VACUUM")?;
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        stored_schema_context_for_kind(&connection, "users_view", SchemaSqlKind::View)?,
        expected_context
    );
    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("users_view").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .len(),
        1
    );
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT name FROM users_view")?
            .run_collect_rows()?,
        vec![vec![Value::build_text("Ada")]]
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn create_trigger_fires_and_preserves_its_marker_through_reopen_and_vacuum() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-trigger-reopen.db";
    let expected_context = binary_context().for_kind(SchemaSqlKind::Trigger);
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`name` TEXT)")?;
        connection.execute("CREATE TABLE `audit` (`name` TEXT, `kind` TEXT)")?;
        connection.execute(
            "CREATE TRIGGER `copy_user` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit` (`name`, `kind`) VALUES (NEW.`name`, 'created'); END",
        )?;
        assert_eq!(
            stored_schema_context_for_kind(&connection, "copy_user", SchemaSqlKind::Trigger)?,
            expected_context
        );
        connection
            .inner()
            .execute("INSERT INTO users VALUES ('Ada')")?;
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT name, kind FROM audit")?
                .run_collect_rows()?,
            vec![vec![Value::build_text("Ada"), Value::build_text("created")]]
        );
        assert!(matches!(
            connection.execute(
                "CREATE TRIGGER `copy_user_again` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit` (`name`, `kind`) VALUES (NEW.`name`, 'again'); END"
            ),
            Err(LimboError::ParseError(_))
        ));
        let duplicate_rows = connection
            .inner()
            .prepare("SELECT name FROM sqlite_schema WHERE name = 'copy_user_again'")?
            .run_collect_rows()?;
        assert!(duplicate_rows.is_empty());
        let table_context = stored_schema_context(&connection, "users")?;
        assert!(matches!(
            connection.execute("ALTER TABLE `users` ADD COLUMN `email` TEXT"),
            Err(LimboError::ParseError(_))
        ));
        assert_eq!(stored_schema_context(&connection, "users")?, table_context);
        connection.inner().close()?;
    }

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = db.connect()?;
        connection.execute("INSERT INTO users VALUES ('Grace')")?;
        assert_eq!(
            connection
                .prepare("SELECT name, kind FROM audit ORDER BY rowid")?
                .run_collect_rows()?,
            vec![
                vec![Value::build_text("Ada"), Value::build_text("created")],
                vec![Value::build_text("Grace"), Value::build_text("created")],
            ]
        );
        connection.execute("VACUUM")?;
        connection.close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        stored_schema_context_for_kind(&connection, "copy_user", SchemaSqlKind::Trigger)?,
        expected_context
    );
    connection
        .inner()
        .execute("INSERT INTO users VALUES ('Lin')")?;
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT name, kind FROM audit ORDER BY rowid")?
            .run_collect_rows()?,
        vec![
            vec![Value::build_text("Ada"), Value::build_text("created")],
            vec![Value::build_text("Grace"), Value::build_text("created")],
            vec![Value::build_text("Lin"), Value::build_text("created")],
        ]
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn strict_signed_integer_assignments_use_durable_mysql_ddl() -> Result<()> {
    use std::num::NonZeroUsize;

    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-strict-signed-integers.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection
            .execute("CREATE TABLE `numbers` (`tiny` TINYINT, `wide` INTEGER, `label` TEXT)")?;
        let stored = connection
            .inner()
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 'numbers'")?
            .run_collect_rows()?[0][0]
            .to_string();
        assert!(stored.contains("`tiny` TINYINT"));
        assert!(stored.contains("`wide` INTEGER"));

        connection.execute(
            "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (-128, -2147483648, 'low'), (127, 2147483647, 'high')",
        )?;

        let mut parameterized = connection
            .prepare("INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (?, ?, 'bound')")?;
        parameterized.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(0))?;
        parameterized.bind_at(NonZeroUsize::new(2).unwrap(), Value::from_i64(1))?;
        parameterized.run_ignore_rows()?;

        let error = connection
            .execute(
                "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (0, 2147483648, 'wide-overflow')",
            )
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "INT")
        ));

        let error = connection
            .execute(
                "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (0, 0, 'kept'), (128, 0, 'rollback')",
            )
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT label FROM numbers ORDER BY rowid")?
                .run_collect_rows()?,
            vec![
                vec![Value::build_text("low")],
                vec![Value::build_text("high")],
                vec![Value::build_text("bound")],
            ]
        );

        let error = connection
            .execute("INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES ('bad', 0, 'bad')")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::IncorrectType { .. })
        ));

        let error = connection
            .inner()
            .execute("INSERT INTO numbers (tiny, wide, label) VALUES (128, 0, 'raw')")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));

        connection.execute("CREATE TABLE `source` (`wide` INT)")?;
        connection.execute(
            "CREATE TRIGGER `copy_source` AFTER INSERT ON `source` FOR EACH ROW BEGIN INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (NEW.`wide`, 0, 'trigger'); END",
        )?;
        let error = connection
            .execute("INSERT INTO `source` (`wide`) VALUES (128)")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT COUNT(*) FROM source")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(0)]]
        );

        connection
            .execute("CREATE TEMPORARY TABLE `temp_numbers` (`tiny` TINYINT, `wide` INTEGER)")?;
        let error = connection
            .execute("INSERT INTO `temp_numbers` (`tiny`, `wide`) VALUES (128, 0)")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "TINYINT")
        ));
        connection.execute("INSERT INTO `temp_numbers` (`tiny`, `wide`) VALUES (0, 0)")?;
        let error = connection
            .execute("UPDATE `temp_numbers` SET `wide` = 2147483648 WHERE TRUE")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "INT")
        ));

        let error = connection
            .execute("UPDATE `numbers` SET `tiny` = 128 WHERE TRUE")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT tiny FROM numbers WHERE label = 'bound'")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(0)]]
        );
        connection.inner().close()?;
    }

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.inner().execute("VACUUM")?;
        let error = connection
            .execute("UPDATE `numbers` SET `wide` = 2147483648 WHERE TRUE")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "INT")
        ));
        connection.inner().close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT wide FROM numbers WHERE label = 'low'")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(-2_147_483_648)]]
    );
    connection.inner().close()?;
    Ok(())
}

fn stored_schema_context(
    connection: &MySqlConnection,
    name: &str,
) -> Result<crate::schema_sql::SchemaSqlContext> {
    stored_schema_context_for_kind(connection, name, SchemaSqlKind::Table)
}

fn stored_schema_context_for_kind(
    connection: &MySqlConnection,
    name: &str,
    kind: SchemaSqlKind,
) -> Result<crate::schema_sql::SchemaSqlContext> {
    let rows = connection
        .inner()
        .prepare(format!(
            "SELECT sql FROM sqlite_schema WHERE name = '{name}'"
        ))?
        .run_collect_rows()?;
    let [row] = rows.as_slice() else {
        return Err(LimboError::InternalError(format!(
            "expected one sqlite_schema row for {name}"
        )));
    };
    let stored = row[0].to_string();
    Ok(decode_schema_sql(kind, stored.trim_matches('\''))
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .ok_or_else(|| LimboError::InternalError(format!("missing MySQL marker for {name}")))?
        .context)
}

#[test]
fn rejects_a_context_the_loader_cannot_preserve() {
    let mut context = binary_context();
    context.default_character_set = CharacterSet::Utf8mb4;
    context.default_collation = Collation::Utf8mb4_0900AiCi;
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-invalid-context.db", OpenFlags::Create).unwrap();

    assert!(matches!(
        MySqlConnection::new(db.connect().unwrap(), context),
        Err(LimboError::ParseError(_))
    ));
}

#[test]
fn rejects_a_connection_opened_with_another_dialect() {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-wrong-dialect.db";
    let file = io.open_file(path, OpenFlags::Create, true).unwrap();
    let db = Database::open(
        io,
        path,
        OpenOptions::new(Arc::new(turso_core::SqliteDialect))
            .storage(Arc::new(DatabaseFile::new(file)))
            .flags(OpenFlags::Create)
            .db_opts(DatabaseOpts::new()),
    )
    .unwrap();

    assert!(matches!(
        MySqlConnection::new(db.connect().unwrap(), binary_context()),
        Err(LimboError::InvalidArgument(_))
    ));
}

#[test]
fn prepares_checked_selects_with_parameters_and_aliases() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-select.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE `users` (`id` INTEGER UNIQUE, `name` TEXT)")?;
    connection
        .inner()
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')")?;

    let mut statement = connection.prepare(
        "SELECT u.`name` AS `display name`, ? AS marker FROM `users` AS u WHERE u.`name` IS NOT NULL",
    )?;
    assert_eq!(statement.parameters_count(), 1);
    statement.bind_at(1.try_into().unwrap(), Value::build_text("matched"))?;
    connection.execute("CREATE INDEX `idx_users_name` ON `users` (`name`)")?;
    assert_eq!(
        statement.run_collect_rows()?,
        vec![
            vec![Value::build_text("Ada"), Value::build_text("matched")],
            vec![Value::build_text("Grace"), Value::build_text("matched")]
        ]
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn query_entry_does_not_fall_back_to_unchecked_sqlite_syntax() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-select-fail-closed.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;

    for sql in [
        "SELECT '1' = 1",
        "SELECT random()",
        // EXCEPT and INTERSECT are MySQL's own since 8.0.31 and are taken now;
        // their ALL forms are what this path still refuses, because they keep
        // duplicates the engine cannot keep.
        "SELECT 1 EXCEPT ALL SELECT 2",
        // Integer arithmetic is taken; the shapes above it are not.
        "SELECT 1.5 + 1",
        "SELECT 1 % 2",
        "INSERT INTO t VALUES (1)",
    ] {
        assert!(
            matches!(connection.prepare(sql), Err(LimboError::ParseError(_))),
            "expected rejection for {sql}"
        );
    }
    // Measured on MySQL 8.4.11: `SELECT 1 EXCEPT SELECT 2` answers 1, so it is
    // MySQL's syntax rather than the engine's, and it is taken.
    assert!(connection.prepare("SELECT 1 EXCEPT SELECT 2").is_ok());
    connection.inner().close()?;
    Ok(())
}

#[test]
fn dropped_view_stays_absent_after_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-drop-view.db";
    let db = open_database(io.clone(), path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT)")?;
    connection.execute("CREATE VIEW records_view AS SELECT id FROM records")?;
    connection
        .drop_view(&MySqlTableName::parse("records_view").unwrap())
        .unwrap();
    connection.close()?;
    drop(connection);
    drop(db);
    let db = open_database(io, path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    let tables = connection.list_tables()?;
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name(), "records");
    connection.execute("CREATE VIEW records_view AS SELECT id FROM records")?;
    connection.close()?;
    Ok(())
}

#[test]
fn dropped_table_stays_absent_after_reopen() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-drop-table.db";
    let db = open_database(io.clone(), path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT)")?;
    let command = turso_mysql_parser::parse_optional_drop_table(
        "DROP TABLE records",
        SessionSqlMode::default(),
    )
    .unwrap()
    .expect("DROP TABLE must be recognized");
    assert!(connection
        .drop_table(&command)
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .missing
        .is_empty());
    connection.close()?;
    drop(connection);
    drop(db);
    let db = open_database(io, path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert!(connection.list_tables()?.is_empty());
    connection.close()?;
    Ok(())
}

#[test]
fn dropping_and_recreating_auto_increment_table_gets_new_identity_and_starts_at_one() -> Result<()>
{
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-drop-recreate-auto-increment.db", [0x57; 16])?;
    let ddl =
        "CREATE TABLE generated_records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)";
    connection.execute(ddl)?;
    let first_key = auto_increment_key(&connection, "generated_records")?;
    connection.execute("INSERT INTO generated_records (label) VALUES ('first')")?;
    assert_eq!(connection.last_insert_id(), 1);

    let command = turso_mysql_parser::parse_optional_drop_table(
        "DROP TABLE generated_records",
        SessionSqlMode::default(),
    )
    .unwrap()
    .expect("DROP TABLE must be recognized");
    connection
        .drop_table(&command)
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection.execute(ddl)?;
    let second_key = auto_increment_key(&connection, "generated_records")?;
    assert_ne!(first_key, second_key);
    connection.execute("INSERT INTO generated_records (label) VALUES ('second')")?;
    assert_eq!(connection.last_insert_id(), 1);
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM generated_records")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1)]]
    );
    connection.close()?;
    Ok(())
}

/// A table's columns are read once for each schema and kept until the schema
/// changes.
#[test]
fn a_tables_columns_are_read_once_for_each_schema() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-column-cache.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT, label VARCHAR(10))")?;
    let table = MySqlTableName::parse("records").unwrap();
    let reads = || connection.schema_readings.lock().unwrap().column_reads;

    let first = connection.list_columns(&table).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(connection.list_columns(&table).unwrap(), first);
    assert_eq!(reads(), 1);

    connection.execute("ALTER TABLE records ADD COLUMN n INT")?;
    let altered = connection.list_columns(&table).unwrap();
    assert_eq!(
        altered
            .iter()
            .map(|column| column.name())
            .collect::<Vec<_>>(),
        ["id", "label", "n"]
    );
    assert_eq!(reads(), 2);
    connection.close()?;
    Ok(())
}

/// A write can leave the WAL holding far more than the engine's own
/// checkpoint empties, so past a bound it is truncated, but never inside a
/// transaction.
#[test]
fn a_wal_past_its_bound_is_truncated_between_transactions() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-wal-bound.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT, label TEXT)")?;
    connection.execute("INSERT INTO records (id, label) VALUES (1, 'kept')")?;
    let frames = || connection.inner().wal_state().unwrap().max_frame;
    let written = frames();
    assert!(written > 0);

    connection.truncate_the_wal_past(written)?;
    assert_eq!(frames(), written);

    connection.inner().execute("BEGIN")?;
    connection.truncate_the_wal_past(0)?;
    assert_eq!(frames(), written);
    connection.inner().execute("COMMIT")?;

    connection.truncate_the_wal_past(0)?;
    assert_eq!(frames(), 0);
    assert_eq!(
        connection
            .prepare_select("SELECT label FROM records")?
            .run_collect_rows()?,
        vec![vec![Value::from_text("kept")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn lists_user_tables_and_views_in_name_order() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-table-listing.db";
    let db = open_database(io.clone(), path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;

    connection.execute("CREATE TABLE notes (id INT)")?;
    connection.execute("CREATE TABLE accounts (id INT)")?;
    connection.execute("CREATE VIEW active_accounts AS SELECT id FROM accounts")?;

    let expected = vec![
        MySqlTable {
            name: "accounts".to_owned(),
            kind: MySqlTableKind::BaseTable,
            options: Some(Default::default()),
        },
        MySqlTable {
            name: "active_accounts".to_owned(),
            kind: MySqlTableKind::View,
            options: None,
        },
        MySqlTable {
            name: "notes".to_owned(),
            kind: MySqlTableKind::BaseTable,
            options: Some(Default::default()),
        },
    ];
    assert_eq!(connection.list_tables()?, expected);
    connection.close()?;
    drop(connection);
    drop(db);
    let db = open_database(io, path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(connection.list_tables()?, expected);
    connection.close()?;
    Ok(())
}

#[test]
fn lists_supported_columns_from_durable_mysql_ddl() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-column-listing.db";
    let db = open_database(io.clone(), path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute(
        "CREATE TABLE records (id INT NOT NULL UNIQUE DEFAULT 1, name TEXT DEFAULT 'guest', payload BLOB, tiny TINYINT, small SMALLINT, maybe MEDIUMINT NULL DEFAULT NULL, `Camel` TEXT DEFAULT 'camel')",
    )?;
    connection.execute("CREATE VIEW record_view AS SELECT id, name FROM records")?;
    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("record_view").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: None,
                name: "id".to_owned(),
                type_name: "INT".to_owned(),
                nullable: false,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
                comment: String::new(),
            },
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: Some("utf8mb4_0900_ai_ci"),
                name: "name".to_owned(),
                type_name: "TEXT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
                comment: String::new(),
            },
        ]
    );

    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("RECORDS").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: None,
                name: "id".to_owned(),
                type_name: "INT".to_owned(),
                nullable: false,
                key: MySqlColumnKey::Unique,
                default_sql: Some("1".to_owned()),
                default_value: Some(MySqlColumnDefault::Integer {
                    text: "1".to_owned(),
                    value: 1,
                }),
                extra: String::new(),
                comment: String::new(),
            },
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: Some("utf8mb4_0900_ai_ci"),
                name: "name".to_owned(),
                type_name: "TEXT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: Some("'guest'".to_owned()),
                default_value: Some(MySqlColumnDefault::Text("guest".to_owned())),
                extra: String::new(),
                comment: String::new(),
            },
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: None,
                name: "payload".to_owned(),
                type_name: "BLOB".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
                comment: String::new(),
            },
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: None,
                name: "tiny".to_owned(),
                type_name: "TINYINT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
                comment: String::new(),
            },
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: None,
                name: "small".to_owned(),
                type_name: "SMALLINT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
                comment: String::new(),
            },
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: None,
                name: "maybe".to_owned(),
                type_name: "MEDIUMINT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: Some("NULL".to_owned()),
                default_value: Some(MySqlColumnDefault::Null),
                extra: String::new(),
                comment: String::new(),
            },
            MySqlColumnMetadata {
                character_length: None,
                decimal_size: None,
                temporal_precision: None,
                collation_name: Some("utf8mb4_0900_ai_ci"),
                name: "Camel".to_owned(),
                type_name: "TEXT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: Some("'camel'".to_owned()),
                default_value: Some(MySqlColumnDefault::Text("camel".to_owned())),
                extra: String::new(),
                comment: String::new(),
            },
        ]
    );
    assert!(matches!(
        connection.list_columns(&MySqlTableName::parse("missing").unwrap()),
        Err(MySqlColumnMetadataError::TableNotFound)
    ));
    connection.execute("ALTER TABLE records ADD COLUMN added TEXT DEFAULT 'added'")?;
    let columns = connection
        .list_columns(&MySqlTableName::parse("records").unwrap())
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        columns.last().and_then(MySqlColumnMetadata::default_sql),
        Some("'added'")
    );
    connection.inner().execute("VACUUM")?;
    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("records").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .last()
            .and_then(MySqlColumnMetadata::default_sql),
        Some("'added'")
    );
    connection.close()?;
    drop(connection);
    drop(db);

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("records").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .len(),
        8
    );
    connection.close()?;
    Ok(())
}

#[test]
fn lists_mediumint_columns_for_tables_and_direct_views() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-mediumint-column-metadata.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT, value MEDIUMINT NULL)")?;
    connection.execute("CREATE VIEW record_view AS SELECT value FROM records")?;

    let table_columns = connection
        .list_columns(&MySqlTableName::parse("records").unwrap())
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(table_columns[1].name(), "value");
    assert_eq!(table_columns[1].type_name(), "MEDIUMINT");
    assert!(table_columns[1].nullable());

    let view_columns = connection
        .list_columns(&MySqlTableName::parse("record_view").unwrap())
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(view_columns.len(), 1);
    assert_eq!(view_columns[0].name(), "value");
    assert_eq!(view_columns[0].type_name(), "MEDIUMINT");
    assert!(view_columns[0].nullable());
    assert_eq!(view_columns[0].key(), MySqlColumnKey::None);
    assert_eq!(view_columns[0].default_value(), None);
    assert_eq!(view_columns[0].extra(), "");

    connection.close()?;
    Ok(())
}

#[test]
fn rejects_view_chains_for_column_metadata() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-view-chain-metadata.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT, name TEXT)")?;
    connection.execute("CREATE VIEW records_view AS SELECT id, name FROM records")?;
    connection.execute("CREATE VIEW chained_view AS SELECT id FROM records_view")?;

    assert!(matches!(
        connection.list_columns(&MySqlTableName::parse("records_view").unwrap()),
        Ok(columns) if columns.len() == 2
    ));
    assert!(matches!(
        connection.list_columns(&MySqlTableName::parse("chained_view").unwrap()),
        Err(MySqlColumnMetadataError::UnsupportedDefinition)
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn rejects_duplicate_view_projection_names_as_corrupt() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-view-duplicate-metadata.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE records (id INT, name TEXT)")?;
    connection.execute("CREATE VIEW duplicate_view AS SELECT id, ID FROM records")?;

    assert!(matches!(
        connection.list_columns(&MySqlTableName::parse("duplicate_view").unwrap()),
        Err(MySqlColumnMetadataError::CorruptDefinition)
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn view_projection_rejects_non_column_shapes() {
    for sql in [
        "CREATE VIEW v AS SELECT id AS renamed FROM records",
        "CREATE VIEW v AS SELECT id + 1 FROM records",
        "CREATE VIEW v AS SELECT records.id FROM records",
        "CREATE VIEW v AS SELECT records.id FROM records JOIN other ON records.id = other.id",
        "CREATE VIEW v AS SELECT id FROM records AS source",
    ] {
        let mut parser = Parser::new(sql.as_bytes());
        let Ok(Some(Cmd::Stmt(Stmt::CreateView { select, .. }))) = parser.next_cmd() else {
            panic!("expected CREATE VIEW statement for {sql:?}");
        };
        assert!(
            matches!(
                MySqlConnection::view_projection(&select, false),
                Err(MySqlColumnMetadataError::UnsupportedDefinition)
            ),
            "expected unsupported view projection for {sql:?}"
        );
    }
}

#[test]
fn view_columns_survive_reopen_and_vacuum_into() -> Result<()> {
    let temp_dir = tempfile::tempdir().map_err(|error| {
        LimboError::InternalError(format!("failed to create vacuum output directory: {error}"))
    })?;
    let output_path = temp_dir.path().join("view-metadata-vacuum.db");
    let output_path = output_path.to_str().ok_or_else(|| {
        LimboError::InternalError("vacuum output path is not valid UTF-8".to_string())
    })?;
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-view-metadata.db";
    let expected = vec![
        MySqlColumnMetadata {
            character_length: None,
            decimal_size: None,
            temporal_precision: None,
            collation_name: None,
            name: "id".to_owned(),
            type_name: "INT".to_owned(),
            nullable: false,
            key: MySqlColumnKey::None,
            default_sql: None,
            default_value: None,
            extra: String::new(),
            comment: String::new(),
        },
        MySqlColumnMetadata {
            character_length: None,
            decimal_size: None,
            temporal_precision: None,
            collation_name: Some("utf8mb4_0900_ai_ci"),
            name: "name".to_owned(),
            type_name: "TEXT".to_owned(),
            nullable: true,
            key: MySqlColumnKey::None,
            default_sql: None,
            default_value: None,
            extra: String::new(),
            comment: String::new(),
        },
    ];
    {
        let db = open_database(io, path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute(
            "CREATE TABLE records (id INT NOT NULL UNIQUE DEFAULT 1, name TEXT DEFAULT 'guest')",
        )?;
        connection.execute("CREATE VIEW records_view AS SELECT id, name FROM records")?;
        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("records_view").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?,
            expected
        );
        connection
            .inner()
            .execute(format!("VACUUM INTO '{output_path}'"))?;
        connection.inner().close()?;
    }

    let output_io: Arc<dyn IO> = Arc::new(PlatformIO::new()?);
    let db = open_database(output_io, output_path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("records_view").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        expected
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn lists_primary_and_auto_increment_metadata_after_reopen_and_vacuum_into() -> Result<()> {
    let path = "mysql-session-column-keys.db";
    let database_identity = [0x91; 16];
    let temp_dir = tempfile::tempdir().map_err(|error| {
        LimboError::InternalError(format!("failed to create vacuum output directory: {error}"))
    })?;
    let output_path = temp_dir.path().join("column-keys-vacuum.db");
    let output_path = output_path.to_str().ok_or_else(|| {
        LimboError::InternalError("vacuum output path is not valid UTF-8".to_string())
    })?;
    let (connection, _allocator, io) = open_allocator_connection(path, database_identity)?;
    connection.execute(
        "CREATE TABLE numbers_int (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT DEFAULT ' AUTO_INCREMENT PRIMARY KEY')",
    )?;
    connection.execute(
        "CREATE TABLE numbers_integer (id INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
    )?;
    let assert_columns = |connection: &MySqlConnection| -> Result<()> {
        for (table, type_name) in [("numbers_int", "INT"), ("numbers_integer", "INTEGER")] {
            let columns = connection
                .list_columns(&MySqlTableName::parse(table).unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?;
            assert_eq!(columns[0].name(), "id");
            assert_eq!(columns[0].type_name(), type_name);
            assert!(!columns[0].nullable());
            assert_eq!(columns[0].key(), MySqlColumnKey::Primary);
            assert_eq!(columns[0].extra(), "AUTO_INCREMENT");
            assert_eq!(columns[1].key(), MySqlColumnKey::None);
            assert_eq!(columns[1].extra(), "");
        }
        Ok(())
    };

    assert_columns(&connection)?;
    connection
        .inner()
        .execute(format!("VACUUM INTO '{output_path}'"))?;
    connection.close()?;
    drop(connection);

    let database = open_database_with_identity(io, path, OpenFlags::None, database_identity)?;
    let connection = MySqlConnection::new(database.connect()?, binary_context())?;
    assert_columns(&connection)?;
    connection.close()?;

    let output_io: Arc<dyn IO> = Arc::new(PlatformIO::new()?);
    let database =
        open_database_with_identity(output_io, output_path, OpenFlags::None, database_identity)?;
    let connection = MySqlConnection::new(database.connect()?, binary_context())?;
    assert_columns(&connection)?;
    connection.close()?;
    Ok(())
}

#[test]
fn mysql_column_metadata_accepts_plain_inline_primary_key() {
    let mut parser = Parser::new(b"CREATE TABLE keys (id INT PRIMARY KEY)");
    let Some(Cmd::Stmt(Stmt::CreateTable {
        body: CreateTableBody::ColumnsAndConstraints { columns, .. },
        ..
    })) = parser.next_cmd().unwrap()
    else {
        panic!("expected CREATE TABLE statement");
    };
    let metadata = mysql_column_metadata(&columns[0]).unwrap();
    assert_eq!(metadata.name(), "id");
    assert_eq!(metadata.type_name(), "INT");
    assert!(!metadata.nullable());
    assert_eq!(metadata.key(), MySqlColumnKey::Primary);
    assert_eq!(metadata.extra(), "");

    for sql in [
        b"CREATE TABLE keys (id INT PRIMARY KEY DESC)" as &[u8],
        b"CREATE TABLE keys (id INTEGER PRIMARY KEY AUTOINCREMENT)",
    ] {
        let mut parser = Parser::new(sql);
        let Some(Cmd::Stmt(Stmt::CreateTable {
            body: CreateTableBody::ColumnsAndConstraints { columns, .. },
            ..
        })) = parser.next_cmd().unwrap()
        else {
            panic!("expected CREATE TABLE statement");
        };
        assert!(matches!(
            mysql_column_metadata(&columns[0]),
            Err(MySqlColumnMetadataError::UnsupportedDefinition)
        ));
    }
}

#[test]
fn persisted_column_defaults_decode_with_stored_sql_mode() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-column-defaults.db";
    let db = open_database(io.clone(), path, OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert!(connection
        .execute(r"CREATE TABLE unsupported_escape (value TEXT DEFAULT '\a')")
        .is_err());
    assert!(connection
        .execute("CREATE TABLE trailing_escape (value TEXT DEFAULT 'bad\\")
        .is_err());
    connection.execute(
        r"CREATE TABLE defaults (escaped TEXT DEFAULT 'line\nnext', literal_slash TEXT DEFAULT 'line\\nnext', quoted TEXT DEFAULT 'it''s', escaped_quote TEXT DEFAULT 'it\'s', integer INT DEFAULT 42, negative BIGINT DEFAULT -42, positive BIGINT DEFAULT +42, truth INT DEFAULT TRUE, falsehood INT DEFAULT FALSE, explicit_null INT DEFAULT NULL, omitted TEXT)",
    )?;

    let assert_defaults = |connection: &MySqlConnection| -> Result<()> {
        let columns = connection
            .list_columns(&MySqlTableName::parse("defaults").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        let default_for = |name: &str| {
            columns
                .iter()
                .find(|column| column.name() == name)
                .and_then(MySqlColumnMetadata::default_value)
        };
        assert_eq!(
            default_for("escaped"),
            Some(&MySqlColumnDefault::Text("line\nnext".to_owned()))
        );
        assert_eq!(
            default_for("literal_slash"),
            Some(&MySqlColumnDefault::Text(r"line\nnext".to_owned()))
        );
        assert_eq!(
            default_for("quoted"),
            Some(&MySqlColumnDefault::Text("it's".to_owned()))
        );
        assert_eq!(
            default_for("escaped_quote"),
            Some(&MySqlColumnDefault::Text("it's".to_owned()))
        );
        assert_eq!(
            default_for("integer"),
            Some(&MySqlColumnDefault::Integer {
                text: "42".to_owned(),
                value: 42,
            })
        );
        assert_eq!(
            default_for("negative"),
            Some(&MySqlColumnDefault::Integer {
                text: "-42".to_owned(),
                value: -42,
            })
        );
        assert_eq!(
            default_for("positive"),
            Some(&MySqlColumnDefault::Integer {
                text: "42".to_owned(),
                value: 42,
            })
        );
        assert_eq!(
            default_for("truth"),
            Some(&MySqlColumnDefault::Boolean(true))
        );
        assert_eq!(
            default_for("falsehood"),
            Some(&MySqlColumnDefault::Boolean(false))
        );
        assert_eq!(
            default_for("explicit_null"),
            Some(&MySqlColumnDefault::Null)
        );
        assert_eq!(default_for("omitted"), None);
        Ok(())
    };
    assert_defaults(&connection)?;
    connection.close()?;
    drop(connection);
    drop(db);

    let db = open_database(io.clone(), path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_defaults(&connection)?;
    connection.close()?;
    drop(connection);
    drop(db);

    let path = "mysql-session-column-defaults-no-backslash.db";
    let db = open_database(io.clone(), path, OpenFlags::Create)?;
    let mut context = binary_context();
    context.sql_mode.no_backslash_escapes = true;
    let connection = MySqlConnection::new(db.connect()?, context)?;
    connection.execute(
        r"CREATE TABLE defaults (literal_slash TEXT DEFAULT 'line\nnext', unrecognized TEXT DEFAULT '\a', quoted TEXT DEFAULT 'it''s')",
    )?;
    let columns = connection
        .list_columns(&MySqlTableName::parse("defaults").unwrap())
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        columns
            .iter()
            .find(|column| column.name() == "literal_slash")
            .and_then(MySqlColumnMetadata::default_value),
        Some(&MySqlColumnDefault::Text(r"line\nnext".to_owned()))
    );
    assert_eq!(
        columns
            .iter()
            .find(|column| column.name() == "quoted")
            .and_then(MySqlColumnMetadata::default_value),
        Some(&MySqlColumnDefault::Text("it's".to_owned()))
    );
    assert_eq!(
        columns
            .iter()
            .find(|column| column.name() == "unrecognized")
            .and_then(MySqlColumnMetadata::default_value),
        Some(&MySqlColumnDefault::Text(r"\a".to_owned()))
    );
    connection.close()?;
    drop(connection);
    drop(db);

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    let columns = connection
        .list_columns(&MySqlTableName::parse("defaults").unwrap())
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    assert_eq!(
        columns
            .iter()
            .find(|column| column.name() == "literal_slash")
            .and_then(MySqlColumnMetadata::default_value),
        Some(&MySqlColumnDefault::Text(r"line\nnext".to_owned()))
    );
    assert_eq!(
        columns
            .iter()
            .find(|column| column.name() == "quoted")
            .and_then(MySqlColumnMetadata::default_value),
        Some(&MySqlColumnDefault::Text("it's".to_owned()))
    );
    assert_eq!(
        columns
            .iter()
            .find(|column| column.name() == "unrecognized")
            .and_then(MySqlColumnMetadata::default_value),
        Some(&MySqlColumnDefault::Text(r"\a".to_owned()))
    );
    connection.close()?;
    Ok(())
}

#[test]
fn column_metadata_fails_closed_for_unrepresentable_table_keys() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-column-listing-unsupported.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;

    for name in ["sqlite_sequence", "__turso_internal_seq_records"] {
        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse(name).unwrap()),
            Err(MySqlColumnMetadataError::TableNotFound)
        ));
    }

    // A named index is a separate object and does not stop the columns
    // being read back; it reports the column it leads as MUL, which is what
    // MySQL 8.4.11 reports for one.
    connection.execute("CREATE TABLE records (id INT, name TEXT)")?;
    connection.execute("CREATE INDEX records_name_idx ON records (name)")?;
    let records = connection
        .list_columns(&MySqlTableName::parse("records").unwrap())
        .expect("a named index does not stop the columns being read back");
    assert_eq!(
        records
            .iter()
            .map(|column| (column.name(), column.key()))
            .collect::<Vec<_>>(),
        vec![
            ("id", MySqlColumnKey::None),
            ("name", MySqlColumnKey::Multiple),
        ]
    );

    connection.execute("CREATE TABLE keyed (id INT, name TEXT, UNIQUE (id, name))")?;
    assert!(matches!(
        connection.list_columns(&MySqlTableName::parse("keyed").unwrap()),
        Err(MySqlColumnMetadataError::UnsupportedDefinition)
    ));

    // Measured on MySQL 8.4.11: `INT DEFAULT 1.25` prints `DEFAULT '1'`, the
    // default being rounded to the places the column holds, and a default past
    // the column's range is 1067.
    connection.execute("CREATE TABLE decimal_default (value INT DEFAULT 1.25)")?;
    assert_eq!(
        connection
            .list_columns(&MySqlTableName::parse("decimal_default").unwrap())
            .unwrap()
            .first()
            .and_then(MySqlColumnMetadata::default_value),
        Some(&MySqlColumnDefault::Integer {
            text: "1".to_owned(),
            value: 1
        })
    );
    assert!(connection
        .execute("CREATE TABLE integer_overflow (value BIGINT DEFAULT 9223372036854775808)")
        .is_err());
    connection.close()?;
    Ok(())
}

#[test]
fn column_metadata_distinguishes_corrupt_and_unsupported_ddl() {
    let malformed =
        parse_create_table_ast("CREATE TABLE records (id INT", SessionSqlMode::default())
            .unwrap_err();
    assert!(matches!(
        mysql_metadata_parse_error(malformed),
        MySqlColumnMetadataError::CorruptDefinition
    ));

    let unsupported = parse_create_table_ast(
        "CREATE TABLE records (id TEXT PRIMARY KEY)",
        SessionSqlMode::default(),
    )
    .unwrap_err();
    assert!(matches!(
        mysql_metadata_parse_error(unsupported),
        MySqlColumnMetadataError::UnsupportedDefinition
    ));

    let malformed = parse_create_view_ast(
        "CREATE VIEW records_view AS SELECT",
        SessionSqlMode::default(),
    )
    .unwrap_err();
    assert!(matches!(
        mysql_metadata_parse_error(malformed),
        MySqlColumnMetadataError::CorruptDefinition
    ));

    let unsupported = parse_create_view_ast(
        "CREATE VIEW records_view AS SELECT id FROM records WHERE id IN (1, 2)",
        SessionSqlMode::default(),
    )
    .unwrap_err();
    assert!(matches!(
        mysql_metadata_parse_error(unsupported),
        MySqlColumnMetadataError::UnsupportedDefinition
    ));
}

#[test]
fn table_listing_detects_the_limit_sentinel() {
    assert!(!MySqlConnection::table_list_is_truncated(
        TABLE_LIST_SCAN_LIMIT - 1
    ));
    assert!(MySqlConnection::table_list_is_truncated(
        TABLE_LIST_SCAN_LIMIT
    ));
    assert!(!MySqlConnection::column_index_scan_is_truncated(
        COLUMN_INDEX_SCAN_LIMIT - 1
    ));
    assert!(MySqlConnection::column_index_scan_is_truncated(
        COLUMN_INDEX_SCAN_LIMIT
    ));
}

#[test]
fn select_prepare_preserves_parser_and_engine_error_stages() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-select-errors.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;

    assert!(matches!(
        connection.prepare_select("SELECT FROM"),
        Err(MySqlQueryError::Syntax(_))
    ));
    assert!(matches!(
        connection.prepare_select("SELECT id FROM missing_table"),
        Err(MySqlQueryError::Engine(_))
    ));

    connection.close()?;
    Ok(())
}

#[test]
fn system_catalog_selects_fail_closed_before_transaction_or_core_prepare() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-internal-catalog-guard.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    let queries = [
        "SELECT name FROM sqlite_schema",
        "SELECT name FROM SQLITE_MASTER",
        "SELECT name FROM sqlite_sequence",
        "SELECT name FROM `SQLite_Sequence`",
        "SELECT name FROM \"sqlite_schema\"",
        "SELECT name FROM 'sqlite_master'",
        "SELECT name FROM __turso_internal_types",
        "SELECT name FROM `__TURSO_INTERNAL_seq_records`",
        "/* leading */ SELECT name FROM sqlite_schema AS catalog /* trailing */",
    ];
    let expected = "SELECT from an internal catalog is unsupported";

    for sql in queries {
        assert!(
            matches!(
                connection.prepare_select(sql),
                Err(MySqlQueryError::Unsupported(message)) if message == expected
            ),
            "unexpected prepare_select result for {sql:?}"
        );
        assert!(
            connection.is_auto_commit(),
            "catalog rejection must not start a transaction for {sql:?}"
        );
        assert!(
            matches!(
                connection.prepare_checked_statement(sql),
                Err(MySqlPreparedStatementError::Prepare(
                    MySqlQueryError::Unsupported(message)
                )) if message == expected
            ),
            "unexpected prepare_checked_statement result for {sql:?}"
        );
    }

    assert_eq!(connection.prepared_statement_metadata(1), None);
    connection.close()?;
    Ok(())
}

#[test]
fn generic_core_create_index_requires_mysql_schema_context() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-generic-create-index.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE `users` (`name` TEXT)")?;

    let error = connection
        .inner()
        .execute("CREATE INDEX idx_users_name ON users (name)")
        .unwrap_err();
    assert!(matches!(error, LimboError::ParseError(_)));

    let rows = connection
        .inner()
        .prepare("SELECT name FROM sqlite_schema WHERE name = 'idx_users_name'")?
        .run_collect_rows()?;
    assert!(rows.is_empty());
    connection.inner().close()?;
    Ok(())
}

#[test]
fn generic_core_create_view_requires_mysql_schema_context() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-generic-create-view.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE `users` (`name` TEXT)")?;

    let error = connection
        .inner()
        .execute("CREATE VIEW users_view AS SELECT name FROM users")
        .unwrap_err();
    assert!(matches!(error, LimboError::ParseError(_)));

    let rows = connection
        .inner()
        .prepare("SELECT name FROM sqlite_schema WHERE name = 'users_view'")?
        .run_collect_rows()?;
    assert!(rows.is_empty());
    connection.inner().close()?;
    Ok(())
}

#[test]
fn generic_core_create_materialized_view_is_rejected_without_a_schema_row() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-generic-create-materialized-view.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE `users` (`name` TEXT)")?;

    let error = connection
        .inner()
        .execute("CREATE MATERIALIZED VIEW users_view AS SELECT name FROM users")
        .unwrap_err();
    assert!(matches!(error, LimboError::ParseError(_)));
    assert!(
        error
            .to_string()
            .contains("MySQL schema formatter supports only CREATE VIEW"),
        "unexpected error: {error}"
    );

    let rows = connection
        .inner()
        .prepare("SELECT name FROM sqlite_schema WHERE name = 'users_view'")?
        .run_collect_rows()?;
    assert!(rows.is_empty());
    connection.inner().close()?;
    Ok(())
}

#[test]
fn generic_core_create_trigger_requires_mysql_schema_context() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(
        io,
        "mysql-session-generic-create-trigger.db",
        OpenFlags::Create,
    )?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE `users` (`name` TEXT)")?;
    connection.execute("CREATE TABLE `audit` (`name` TEXT)")?;

    let error = connection
        .inner()
        .execute(
            "CREATE TRIGGER copy_user AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
        )
        .unwrap_err();
    assert!(matches!(error, LimboError::ParseError(_)));
    assert!(
        error
            .to_string()
            .contains("MySQL CREATE TRIGGER requires SchemaSqlSessionContext"),
        "unexpected error: {error}"
    );

    let rows = connection
        .inner()
        .prepare("SELECT name FROM sqlite_schema WHERE name = 'copy_user'")?
        .run_collect_rows()?;
    assert!(rows.is_empty());
    connection.inner().close()?;
    Ok(())
}

#[test]
fn authenticated_creator_trigger_keeps_its_definition() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = open_database(io, "mysql-session-creator-trigger.db", OpenFlags::Create)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    connection.execute("CREATE TABLE users (name TEXT)")?;
    connection.execute("CREATE TABLE audit (name TEXT)")?;
    connection
        .execute_schema_object_ddl_with_creator(
            "CREATE TRIGGER copy_user AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
            SchemaSqlCreator::new("owner".to_owned(), String::new()),
        )
        .unwrap();
    let trigger = connection
        .trigger_metadata(&MySqlTableName::parse("copy_user").unwrap())?
        .unwrap();
    assert_eq!(trigger.creator.username, "owner");
    assert!(trigger.create_statement.contains("DEFINER=`owner`@`%`"));
    connection.inner().close()?;
    Ok(())
}

#[test]
fn prepared_select_stores_metadata_without_starting_a_transaction() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-select-metadata.db", [0x69; 16])?;
    connection.execute("CREATE TABLE users (id INT, name TEXT)")?;
    connection.execute("INSERT INTO users (id, name) VALUES (7, 'Ada')")?;

    assert!(connection.is_auto_commit());
    let metadata = connection
        .prepare_checked_statement("SELECT id, ? AS input, 'ready' AS status FROM users")
        .unwrap();

    assert_eq!(metadata.statement_id, 1);
    assert_eq!(metadata.parameter_count, 1);
    assert_eq!(
        metadata.result_columns,
        vec![
            MySqlPreparedResultColumn {
                name: "id".to_string(),
                type_name: Some("INTEGER".to_string()),
            },
            MySqlPreparedResultColumn {
                name: "input".to_string(),
                type_name: None,
            },
            MySqlPreparedResultColumn {
                name: "status".to_string(),
                type_name: Some("TEXT".to_string()),
            },
        ]
    );
    assert!(connection.is_auto_commit());
    assert_eq!(
        connection.prepared_statement_metadata(metadata.statement_id),
        Some(metadata.clone())
    );

    let rows = connection
        .with_prepared_statement(metadata.statement_id, |statement| {
            statement.bind_at(std::num::NonZero::new(1).unwrap(), Value::from_i64(7))?;
            statement.run_collect_rows()
        })
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::from_i64(7),
            Value::from_i64(7),
            Value::from_text("ready"),
        ]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_metadata_preserves_declared_integer_widths_for_empty_and_null_rows() -> Result<()> {
    let path = "mysql-session-prepared-declared-types.db";
    let database_identity = [0x9a; 16];
    let (connection, _allocator, io) = open_allocator_connection(path, database_identity)?;
    connection.execute(
        "CREATE TABLE widths (tiny TINYINT, small SMALLINT, int_value INT, integer_value INTEGER, big BIGINT)",
    )?;
    let query = "SELECT tiny, small, int_value, integer_value, big FROM widths";
    let expected_types = ["INTEGER"; 5];
    let expected_declared_types = ["TINYINT", "SMALLINT", "INT", "INTEGER", "BIGINT"];
    let assert_metadata = |connection: &MySqlConnection| -> Result<u32> {
        let metadata = connection.prepare_checked_statement(query).unwrap();
        assert_eq!(
            metadata
                .result_columns
                .iter()
                .map(|column| column.type_name.as_deref())
                .collect::<Vec<_>>(),
            expected_types
                .iter()
                .map(|type_name| Some(*type_name))
                .collect::<Vec<_>>()
        );
        let type_metadata = connection
            .prepared_statement_result_column_type_metadata(metadata.statement_id)
            .unwrap();
        assert_eq!(
            type_metadata
                .iter()
                .map(|column| column.declared_type_name())
                .collect::<Vec<_>>(),
            expected_declared_types
                .iter()
                .map(|type_name| Some(*type_name))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            type_metadata
                .iter()
                .map(|column| column.source_reference())
                .collect::<Vec<_>>(),
            vec![
                Some(("widths", 0)),
                Some(("widths", 1)),
                Some(("widths", 2)),
                Some(("widths", 3)),
                Some(("widths", 4)),
            ]
        );
        Ok(metadata.statement_id)
    };

    let statement_id = assert_metadata(&connection)?;
    assert!(connection
        .execute_prepared_select(statement_id, &[], None)
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .is_empty());
    connection
        .inner()
        .execute("INSERT INTO widths VALUES (NULL, NULL, NULL, NULL, NULL)")?;
    assert_eq!(
        connection
            .execute_prepared_select(statement_id, &[], None)
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![
            MySqlPreparedValue::Null,
            MySqlPreparedValue::Null,
            MySqlPreparedValue::Null,
            MySqlPreparedValue::Null,
            MySqlPreparedValue::Null,
        ]]
    );

    let metadata = connection
        .prepare_checked_statement(
            "SELECT tiny AS tiny_alias, 1 AS literal_expression, NULL AS null_expression FROM widths",
        )
        .unwrap();
    assert_eq!(
        metadata
            .result_columns
            .iter()
            .map(|column| column.type_name.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("INTEGER"), Some("INTEGER"), None]
    );
    let type_metadata = connection
        .prepared_statement_result_column_type_metadata(metadata.statement_id)
        .unwrap();
    assert_eq!(
        type_metadata
            .iter()
            .map(|column| column.declared_type_name())
            .collect::<Vec<_>>(),
        vec![Some("TINYINT"), None, None]
    );
    assert_eq!(
        type_metadata
            .iter()
            .map(|column| column.source_reference())
            .collect::<Vec<_>>(),
        vec![Some(("widths", 0)), None, None]
    );
    let alias_metadata = connection
        .prepare_checked_statement("SELECT tiny AS tiny_alias FROM widths AS source")
        .unwrap();
    let alias_type_metadata = connection
        .prepared_statement_result_column_type_metadata(alias_metadata.statement_id)
        .unwrap();
    assert_eq!(
        alias_type_metadata[0].source_reference(),
        Some(("source", 0))
    );
    connection.close()?;
    drop(connection);

    let database = open_database_with_identity(io, path, OpenFlags::None, database_identity)?;
    let connection = MySqlConnection::new(database.connect()?, binary_context())?;
    assert_metadata(&connection)?;
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_metadata_refreshes_after_wildcard_reprepare() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-wildcard-reprepare.db", [0xa1; 16])?;
    connection.execute("CREATE TABLE reprepare_metadata (id INT)")?;
    let metadata = connection
        .prepare_checked_statement(
            "SELECT *, 0001 AS literal_value FROM reprepare_metadata LIMIT 0",
        )
        .unwrap();
    assert_eq!(metadata.result_columns.len(), 2);
    connection.execute("ALTER TABLE reprepare_metadata ADD COLUMN ignored TEXT")?;
    assert!(connection
        .execute_prepared_select(metadata.statement_id, &[], None)
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .is_empty());
    let refreshed = connection
        .prepared_statement_metadata(metadata.statement_id)
        .expect("prepared metadata must remain registered after execution");
    let type_metadata = connection
        .prepared_statement_result_column_type_metadata(metadata.statement_id)
        .expect("prepared type metadata must remain registered after execution");
    assert_eq!(refreshed.result_columns.len(), 3);
    assert!(matches!(
        type_metadata[2].static_metadata(),
        Some(StaticSelectMetadata::Integer { digit_count, .. }) if *digit_count == 4
    ));
    assert_eq!(
        type_metadata
            .iter()
            .map(|column| column.source_reference())
            .collect::<Vec<_>>(),
        vec![
            Some(("reprepare_metadata", 0)),
            Some(("reprepare_metadata", 1)),
            None,
        ]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_select_checks_integer_comparison_parameters_and_null_logic() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-select-comparison.db", [0x6d; 16])?;
    connection.execute("CREATE TABLE records (id INT, small SMALLINT, body TEXT)")?;
    connection.execute(
        "INSERT INTO records (id, small, body) VALUES (1, -32768, 'one'), (2, 32767, 'two')",
    )?;

    let metadata = connection
        .prepare_checked_statement("SELECT ? AS marker, id FROM records WHERE id = ?")
        .unwrap();
    assert_eq!(metadata.parameter_count, 2);
    assert!(matches!(
        connection.prepare_select("SELECT id FROM records WHERE id = ?"),
        Err(MySqlQueryError::Unsupported(message))
            if message.contains("checked prepared-statement API")
    ));
    assert!(matches!(
        connection.with_prepared_statement(metadata.statement_id, |_| Ok(())),
        Err(MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(message)))
            if message.contains("checked prepared-statement API")
    ));
    assert_eq!(
        connection
            .execute_prepared_select(
                metadata.statement_id,
                &[
                    MySqlPreparedValue::Text("marker".to_string()),
                    MySqlPreparedValue::Integer(2),
                ],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![
            MySqlPreparedValue::Text("marker".to_string()),
            MySqlPreparedValue::Integer(2),
        ]]
    );

    let null_metadata = connection
        .prepare_checked_statement("SELECT id FROM records WHERE id = NULL")
        .unwrap();
    assert!(connection
        .execute_prepared_select(null_metadata.statement_id, &[], None)
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .is_empty());

    let compound_metadata = connection
        .prepare_checked_statement(
            "SELECT id FROM records WHERE (? IS NULL AND id = ?) OR id = NULL",
        )
        .unwrap();
    assert_eq!(compound_metadata.parameter_count, 2);
    assert_eq!(
        connection
            .execute_prepared_select(
                compound_metadata.statement_id,
                &[MySqlPreparedValue::Null, MySqlPreparedValue::Integer(2)],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![MySqlPreparedValue::Integer(2)]]
    );

    let invalid_metadata = connection
        .prepare_checked_statement("SELECT id FROM records WHERE id = ?")
        .unwrap();
    for value in [
        MySqlPreparedValue::Real(2.0),
        MySqlPreparedValue::Text("2.0".to_string()),
        MySqlPreparedValue::Text(" 2".to_string()),
        MySqlPreparedValue::Blob(vec![b'2']),
    ] {
        assert!(matches!(
            connection.execute_prepared_select(invalid_metadata.statement_id, &[value], None),
            Err(MySqlPreparedStatementError::Engine(
                LimboError::InvalidArgument(_)
            ))
        ));
    }
    // A word naming a whole number is bound as that number, which is what
    // MySQL reads it as.
    assert_eq!(
        connection
            .execute_prepared_select(
                invalid_metadata.statement_id,
                &[MySqlPreparedValue::Text("02".to_string())],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![MySqlPreparedValue::Integer(2)]]
    );

    connection.close()?;
    Ok(())
}

#[test]
fn prepared_select_comparison_requires_durable_column_metadata() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-prepared-select-comparison-types.db",
        [0x6e; 16],
    )?;
    connection.execute("CREATE TABLE records (id INT, body TEXT)")?;

    // A `?` against a text column is taken, because the statement is
    // rendered with MySQL's collation once the column's type is known.
    assert!(connection
        .prepare_checked_statement("SELECT body FROM records WHERE body = ?")
        .is_ok());
    // A number against a text column is not; MySQL coerces it.
    assert!(matches!(
        connection.prepare_select("SELECT body FROM records WHERE body = 1"),
        Err(MySqlQueryError::Unsupported(message)) if message.contains("signed integer")
    ));
    let metadata = connection
        .prepare_checked_statement("SELECT id FROM records WHERE id = ?")
        .unwrap();
    connection.execute("ALTER TABLE records ADD COLUMN note TEXT")?;
    assert!(connection
        .execute_prepared_select(
            metadata.statement_id,
            &[MySqlPreparedValue::Integer(1)],
            None,
        )
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .is_empty());
    connection.execute("CREATE VIEW integer_records AS SELECT id FROM records")?;
    let view_metadata = connection
        .prepare_checked_statement("SELECT id FROM integer_records WHERE id = ?")
        .unwrap();
    assert!(connection
        .execute_prepared_select(
            view_metadata.statement_id,
            &[MySqlPreparedValue::Integer(1)],
            None,
        )
        .map_err(|error| LimboError::InternalError(error.to_string()))?
        .is_empty());

    connection.close()?;
    Ok(())
}

#[test]
fn selects_with_all_strict_integer_comparisons_keep_three_valued_logic() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-select-comparisons.db", [0x70; 16])?;
    connection.execute("CREATE TABLE records (id INT, body TEXT)")?;
    connection.execute("INSERT INTO records (id, body) VALUES (1, 'one'), (2, 'two')")?;

    for (sql, expected) in [
        (
            "SELECT id FROM records WHERE id < 2",
            vec![vec![Value::from_i64(1)]],
        ),
        (
            "SELECT id FROM records WHERE id <= 1",
            vec![vec![Value::from_i64(1)]],
        ),
        (
            "SELECT id FROM records WHERE id > 1",
            vec![vec![Value::from_i64(2)]],
        ),
        (
            "SELECT id FROM records WHERE id >= 2",
            vec![vec![Value::from_i64(2)]],
        ),
        (
            "SELECT id FROM records WHERE id <> 1",
            vec![vec![Value::from_i64(2)]],
        ),
        (
            "SELECT id FROM records WHERE id != 1",
            vec![vec![Value::from_i64(2)]],
        ),
        ("SELECT id FROM records WHERE id < NULL", Vec::new()),
        ("SELECT id FROM records WHERE id <> NULL", Vec::new()),
    ] {
        assert_eq!(
            connection.prepare_select(sql)?.run_collect_rows()?,
            expected,
            "{sql}"
        );
    }

    let prepared = connection
        .prepare_checked_statement(
            "SELECT id FROM records WHERE (? IS NULL AND id < ?) OR NOT (id >= NULL)",
        )
        .unwrap();
    assert_eq!(
        connection
            .execute_prepared_select(
                prepared.statement_id,
                &[MySqlPreparedValue::Null, MySqlPreparedValue::Integer(2)],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![MySqlPreparedValue::Integer(1)]]
    );

    connection.execute(
        "CREATE TABLE bounds (tiny TINYINT, small SMALLINT, medium MEDIUMINT, int_value INT, big BIGINT)",
    )?;
    connection.execute(
        "INSERT INTO bounds (tiny, small, medium, int_value, big) VALUES (-128, -32768, -8388608, -2147483648, -9223372036854775808), (127, 32767, 8388607, 2147483647, 9223372036854775807)",
    )?;
    for (sql, expected) in [
        (
            "SELECT tiny FROM bounds WHERE tiny < 128",
            vec![vec![Value::from_i64(-128)], vec![Value::from_i64(127)]],
        ),
        (
            "SELECT tiny FROM bounds WHERE tiny > -129",
            vec![vec![Value::from_i64(-128)], vec![Value::from_i64(127)]],
        ),
        ("SELECT tiny FROM bounds WHERE tiny < -128", Vec::new()),
        ("SELECT tiny FROM bounds WHERE tiny > 127", Vec::new()),
    ] {
        assert_eq!(
            connection.prepare_select(sql)?.run_collect_rows()?,
            expected,
            "{sql}"
        );
    }

    connection.close()?;
    Ok(())
}

#[test]
fn comparisons_leave_out_rows_whose_column_is_null() -> Result<()> {
    // Measured against MySQL 8.4.11: a NULL column compares to unknown, so
    // the row is left out of the predicate and out of its negation.
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-comparison-null-rows.db", [0x74; 16])?;
    connection.execute("CREATE TABLE records (id INT, nullable_int INT)")?;
    connection
        .execute("INSERT INTO records (id, nullable_int) VALUES (1, 1), (2, NULL), (3, 1)")?;

    for (sql, expected) in [
        (
            "SELECT id FROM records WHERE nullable_int < 1 ORDER BY id",
            Vec::new(),
        ),
        (
            "SELECT id FROM records WHERE nullable_int <> 1 ORDER BY id",
            Vec::new(),
        ),
        (
            "SELECT id FROM records WHERE nullable_int != 1 ORDER BY id",
            Vec::new(),
        ),
        (
            "SELECT id FROM records WHERE NOT (nullable_int < 1) ORDER BY id",
            vec![vec![Value::from_i64(1)], vec![Value::from_i64(3)]],
        ),
        (
            "SELECT id FROM records WHERE nullable_int >= NULL ORDER BY id",
            Vec::new(),
        ),
    ] {
        assert_eq!(
            connection.prepare_select(sql)?.run_collect_rows()?,
            expected,
            "{sql}"
        );
    }

    // A bound NULL compares the same way a literal one does.
    let prepared = connection
        .prepare_checked_statement("SELECT id FROM records WHERE nullable_int > ?")
        .unwrap();
    assert!(connection
        .execute_prepared_select(prepared.statement_id, &[MySqlPreparedValue::Null], None)
        .unwrap()
        .is_empty());
    // Values MySQL would coerce are refused rather than guessed at. A word
    // naming a whole number is the one MySQL reads as exactly that number.
    for value in [
        MySqlPreparedValue::Text("1.5".to_string()),
        MySqlPreparedValue::Text("1abc".to_string()),
        MySqlPreparedValue::Real(1.5),
    ] {
        assert!(
            connection
                .execute_prepared_select(prepared.statement_id, &[value.clone()], None)
                .is_err(),
            "{value:?}"
        );
    }
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_integer_comparisons_recheck_schema_and_type_after_reprepare() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-prepared-select-comparison-reprepare.db",
        [0x71; 16],
    )?;
    connection.execute("CREATE TABLE records (id INT)")?;
    connection.execute("INSERT INTO records (id) VALUES (1), (2)")?;
    let metadata = connection
        .prepare_checked_statement("SELECT id FROM records WHERE id > ?")
        .unwrap();

    connection.execute("ALTER TABLE records ADD COLUMN note TEXT")?;
    assert_eq!(
        connection
            .execute_prepared_select(
                metadata.statement_id,
                &[MySqlPreparedValue::Integer(1)],
                None,
            )
            .map_err(|error| LimboError::InternalError(error.to_string()))?,
        vec![vec![MySqlPreparedValue::Integer(2)]]
    );

    let command = turso_mysql_parser::parse_optional_drop_table(
        "DROP TABLE records",
        SessionSqlMode::default(),
    )
    .unwrap()
    .expect("DROP TABLE must be recognized");
    connection
        .drop_table(&command)
        .map_err(|error| LimboError::InternalError(error.to_string()))?;
    connection.execute("CREATE TABLE records (id TEXT)")?;
    assert!(matches!(
        connection.execute_prepared_select(
            metadata.statement_id,
            &[MySqlPreparedValue::Integer(1)],
            None,
        ),
        Err(MySqlPreparedStatementError::Engine(LimboError::InvalidArgument(message)))
            if message.contains("signed integer")
    ));

    connection.close()?;
    Ok(())
}

#[test]
fn executes_prepared_select_values_and_reuses_the_statement() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-select-execute.db", [0x6c; 16])?;
    let metadata = connection
        .prepare_checked_statement("SELECT ?, ?, ?, ?, ?")
        .unwrap();
    let first = vec![
        MySqlPreparedValue::Null,
        MySqlPreparedValue::Integer(-7),
        MySqlPreparedValue::Real(1.5),
        MySqlPreparedValue::Text("Ada".to_string()),
        MySqlPreparedValue::Blob(vec![0x01, 0x02]),
    ];
    assert_eq!(
        connection
            .execute_prepared_select(metadata.statement_id, &first, None)
            .unwrap(),
        vec![vec![
            MySqlPreparedValue::Null,
            MySqlPreparedValue::Integer(-7),
            MySqlPreparedValue::Real(1.5),
            MySqlPreparedValue::Text("Ada".to_string()),
            MySqlPreparedValue::Blob(vec![0x01, 0x02]),
        ]]
    );

    let count_error = connection
        .execute_prepared_select(metadata.statement_id, &[], None)
        .unwrap_err();
    assert!(matches!(
        count_error,
        MySqlPreparedStatementError::ParameterCountMismatch {
            expected: 5,
            actual: 0
        }
    ));

    let second = vec![
        MySqlPreparedValue::Integer(42),
        MySqlPreparedValue::Real(-2.25),
        MySqlPreparedValue::Text("Grace".to_string()),
        MySqlPreparedValue::Blob(vec![0xff]),
        MySqlPreparedValue::Null,
    ];
    assert_eq!(
        connection
            .execute_prepared_select(metadata.statement_id, &second, None)
            .unwrap(),
        vec![vec![
            MySqlPreparedValue::Integer(42),
            MySqlPreparedValue::Real(-2.25),
            MySqlPreparedValue::Text("Grace".to_string()),
            MySqlPreparedValue::Blob(vec![0xff]),
            MySqlPreparedValue::Null,
        ]]
    );
    let expanded = connection
        .with_prepared_statement(metadata.statement_id, |statement| {
            Ok(statement.expanded_sql())
        })
        .unwrap();
    assert!(
        expanded.contains("'Grace'"),
        "unexpected expanded SQL: {expanded}"
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_select_table_read_starts_implicit_transaction_at_execute() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-select-read-txn.db", [0x6d; 16])?;
    connection.execute("CREATE TABLE users (id INTEGER)")?;
    connection.execute("INSERT INTO users (id) VALUES (7)")?;

    let metadata = connection
        .prepare_checked_statement("SELECT id, ? FROM users")
        .unwrap();
    assert!(connection.is_auto_commit());

    connection.set_autocommit(false).unwrap();
    assert!(!connection.session_autocommit());
    assert!(connection.is_auto_commit());
    assert_eq!(
        connection
            .execute_prepared_select(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("bound".to_string())],
                None,
            )
            .unwrap(),
        vec![vec![
            MySqlPreparedValue::Integer(7),
            MySqlPreparedValue::Text("bound".to_string()),
        ]]
    );
    assert!(!connection.is_auto_commit());

    connection.set_autocommit(true).unwrap();
    assert!(connection.is_auto_commit());
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_select_timeout_resets_statement_for_reuse() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-select-timeout.db", [0x6e; 16])?;
    let metadata = connection.prepare_checked_statement("SELECT ?").unwrap();

    assert!(matches!(
        connection.execute_prepared_select(
            metadata.statement_id,
            &[MySqlPreparedValue::Integer(1)],
            Some(Duration::ZERO),
        ),
        Err(MySqlPreparedStatementError::Engine(_))
    ));
    assert_eq!(
        connection
            .execute_prepared_select(
                metadata.statement_id,
                &[MySqlPreparedValue::Integer(2)],
                None,
            )
            .unwrap(),
        vec![vec![MySqlPreparedValue::Integer(2)]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_select_callback_error_resets_statement_for_reuse() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-prepared-select-callback-error.db",
        [0x6f; 16],
    )?;
    let metadata = connection.prepare_checked_statement("SELECT ?").unwrap();

    assert!(matches!(
        connection.execute_prepared_select_with_row_callback(
            metadata.statement_id,
            &[MySqlPreparedValue::Integer(1)],
            None,
            |_| Err(LimboError::TooBig),
        ),
        Err(MySqlPreparedStatementError::Engine(LimboError::TooBig))
    ));
    assert_eq!(
        connection
            .execute_prepared_select(
                metadata.statement_id,
                &[MySqlPreparedValue::Integer(2)],
                None,
            )
            .unwrap(),
        vec![vec![MySqlPreparedValue::Integer(2)]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_insert_reuses_bindings_without_starting_a_transaction_at_prepare() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-insert-reuse.db", [0x70; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.set_autocommit(false)?;

    let metadata = connection
        .prepare_checked_statement("INSERT INTO notes (id, body) VALUES (?, ?)")
        .unwrap();
    assert!(metadata.result_columns.is_empty());
    assert!(connection.is_auto_commit());

    let first = connection
        .execute_prepared_statement(
            metadata.statement_id,
            &[
                MySqlPreparedValue::Integer(1),
                MySqlPreparedValue::Text("Ada".to_string()),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(
        first,
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 0,
        })
    );
    assert!(!connection.is_auto_commit());
    connection.execute_transaction_command("COMMIT")?;

    let second = connection
        .execute_prepared_statement(
            metadata.statement_id,
            &[
                MySqlPreparedValue::Integer(2),
                MySqlPreparedValue::Text("Grace".to_string()),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(
        second,
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 0,
        })
    );
    connection.set_autocommit(true)?;
    assert_eq!(
        connection
            .prepare_select("SELECT id, body FROM notes")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(1), Value::from_text("Ada")],
            vec![Value::from_i64(2), Value::from_text("Grace")],
        ]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_update_uses_requested_affected_rows_mode_and_prepared_delete_returns_ok() -> Result<()>
{
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-update-delete.db", [0x71; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept'), (2, 'kept')")?;

    let update = connection
        .prepare_checked_statement("UPDATE notes SET body = ? WHERE TRUE")
        .unwrap();
    assert_eq!(
        connection
            .execute_prepared_statement(
                update.statement_id,
                &[MySqlPreparedValue::Text("kept".to_string())],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 0,
            last_insert_id: 0,
        })
    );
    assert_eq!(
        connection
            .execute_prepared_statement(
                update.statement_id,
                &[MySqlPreparedValue::Text("kept".to_string())],
                None,
                MySqlAffectedRowsMode::Matched,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 2,
            last_insert_id: 0,
        })
    );

    let delete = connection
        .prepare_checked_statement("DELETE FROM notes WHERE TRUE")
        .unwrap();
    assert_eq!(
        connection
            .execute_prepared_statement(
                delete.statement_id,
                &[],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 2,
            last_insert_id: 0,
        })
    );
    connection.close()?;
    Ok(())
}

#[test]
fn failed_prepared_write_resets_the_statement_for_reuse() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-write-reset.db", [0x72; 16])?;
    connection.execute("CREATE TABLE notes (id INTEGER UNIQUE, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept')")?;
    let metadata = connection
        .prepare_checked_statement("INSERT INTO notes (id, body) VALUES (?, ?)")
        .unwrap();

    assert!(matches!(
        connection.execute_prepared_statement(
            metadata.statement_id,
            &[
                MySqlPreparedValue::Integer(1),
                MySqlPreparedValue::Text("duplicate".to_string()),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        ),
        Err(MySqlPreparedStatementError::Engine(_))
    ));
    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[
                    MySqlPreparedValue::Integer(2),
                    MySqlPreparedValue::Text("reused".to_string()),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 0,
        })
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_write_timeout_resets_the_statement_without_mutating_rows() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-write-timeout.db", [0x74; 16])?;
    connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
    connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept')")?;
    let metadata = connection
        .prepare_checked_statement("UPDATE notes SET body = ? WHERE TRUE")
        .unwrap();

    assert!(matches!(
        connection.execute_prepared_statement(
            metadata.statement_id,
            &[MySqlPreparedValue::Text("late".to_string())],
            Some(Duration::ZERO),
            MySqlAffectedRowsMode::Changed,
        ),
        Err(MySqlPreparedStatementError::Engine(LimboError::Interrupt))
    ));
    assert_eq!(
        connection
            .prepare_select("SELECT body FROM notes")?
            .run_collect_rows()?,
        vec![vec![Value::from_text("kept")]]
    );
    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("updated".to_string())],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 0,
        })
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_auto_increment_insert_does_not_reserve_or_expose_a_prototype() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-prepared-auto-increment-rejected.db",
        [0x73; 16],
    )?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;

    let metadata = connection
        .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
        .unwrap();
    assert_eq!(metadata.parameter_count, 1);
    assert!(metadata.result_columns.is_empty());
    assert!(matches!(
        connection.with_prepared_statement(metadata.statement_id, |_| Ok(())),
        Err(MySqlPreparedStatementError::Prepare(
            MySqlQueryError::Unsupported(_)
        ))
    ));
    connection
        .reset_prepared_statement(metadata.statement_id)
        .unwrap();

    let mut reservation = _allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
    assert_eq!(_io.block(|| reservation.step())?.first(), 1);
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_auto_increment_insert_reuses_multirow_parameters_in_source_order() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-auto-increment-reuse.db", [0x74; 16])?;
    connection.execute(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT, value INT)",
    )?;
    let metadata = connection
        .prepare_checked_statement("INSERT INTO users (name, value) VALUES (?, ?), (?, ?)")
        .unwrap();
    assert_eq!(metadata.parameter_count, 4);

    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[
                    MySqlPreparedValue::Text("Ada".to_string()),
                    MySqlPreparedValue::Integer(10),
                    MySqlPreparedValue::Text("Grace".to_string()),
                    MySqlPreparedValue::Integer(20),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 2,
            last_insert_id: 1,
        })
    );
    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[
                    MySqlPreparedValue::Text("Linus".to_string()),
                    MySqlPreparedValue::Integer(30),
                    MySqlPreparedValue::Text("Marie".to_string()),
                    MySqlPreparedValue::Integer(40),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 2,
            last_insert_id: 3,
        })
    );
    assert_eq!(connection.last_insert_id(), 3);
    assert_eq!(
        connection
            .prepare_select("SELECT id, name, value FROM users")?
            .run_collect_rows()?,
        vec![
            vec![
                Value::from_i64(1),
                Value::from_text("Ada"),
                Value::from_i64(10),
            ],
            vec![
                Value::from_i64(2),
                Value::from_text("Grace"),
                Value::from_i64(20),
            ],
            vec![
                Value::from_i64(3),
                Value::from_text("Linus"),
                Value::from_i64(30),
            ],
            vec![
                Value::from_i64(4),
                Value::from_text("Marie"),
                Value::from_i64(40),
            ],
        ]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn mixed_auto_increment_rows_burn_unused_ids_and_survive_rollback() -> Result<()> {
    let (connection, allocator, io) =
        open_allocator_connection("mysql-session-mixed-auto-increment.db", [0xa6; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    let mut seed = allocator.advance_past(auto_increment_key(&connection, "users")?, 9)?;
    io.block(|| seed.step())?;
    let result = connection
        .execute_checked_write(
            "INSERT INTO users (id, name) VALUES (NULL, 'a'), (3, 'b'), (DEFAULT, 'c'), (0, 'd')",
            None,
        )
        .unwrap();
    assert_eq!(result.affected_rows, 4);
    assert_eq!(result.last_insert_id, 10);
    assert_eq!(connection.last_insert_id(), 10);
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM users ORDER BY name")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(10)],
            vec![Value::from_i64(3)],
            vec![Value::from_i64(11)],
            vec![Value::from_i64(12)],
        ]
    );
    connection.execute_transaction_command("BEGIN").unwrap();
    connection
        .execute_checked_write(
            "INSERT INTO users (id, name) VALUES (NULL, 'rolled'), (5, 'rolled2')",
            None,
        )
        .unwrap();
    connection.execute_transaction_command("ROLLBACK").unwrap();
    let result = connection
        .execute_checked_write("INSERT INTO users (name) VALUES ('after')", None)
        .unwrap();
    assert_eq!(result.last_insert_id, 16);
    connection.close()?;
    Ok(())
}

#[test]
fn mixed_explicit_high_water_ids_advance_in_row_order() -> Result<()> {
    let (connection, allocator, io) =
        open_allocator_connection("mysql-session-unsafe-mixed-ids.db", [0xa9; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    let mut seed = allocator.advance_past(auto_increment_key(&connection, "users")?, 9)?;
    io.block(|| seed.step())?;
    let first = connection
        .execute_checked_write(
            "INSERT INTO users (id, name) VALUES (NULL, 'a'), (50, 'b')",
            None,
        )
        .unwrap();
    assert_eq!(first.affected_rows, 2);
    assert_eq!(first.last_insert_id, 10);
    let second = connection
        .execute_checked_write(
            "INSERT INTO users (id, name) VALUES (60, 'c'), (NULL, 'd')",
            None,
        )
        .unwrap();
    assert_eq!(second.affected_rows, 2);
    assert_eq!(second.last_insert_id, 61);
    let result = connection
        .execute_checked_write("INSERT INTO users (name) VALUES ('kept')", None)
        .unwrap();
    assert_eq!(result.last_insert_id, 63);
    let low_first = connection
        .execute_checked_write(
            "INSERT INTO users (id, name) VALUES (4, 'low'), (NULL, 'next')",
            None,
        )
        .unwrap();
    assert_eq!(low_first.last_insert_id, 64);
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM users ORDER BY name")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(10)],
            vec![Value::from_i64(50)],
            vec![Value::from_i64(60)],
            vec![Value::from_i64(61)],
            vec![Value::from_i64(63)],
            vec![Value::from_i64(4)],
            vec![Value::from_i64(64)],
        ]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn mixed_high_water_failure_and_rollback_keep_the_oracle_counter() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-mixed-high-failure.db", [0xaa; 16])?;
    connection.execute(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, code TEXT UNIQUE)",
    )?;
    connection
        .execute_checked_write("INSERT INTO users (code) VALUES ('a')", None)
        .unwrap();
    assert!(connection
        .execute_checked_write(
            "INSERT INTO users (id, code) VALUES (NULL, 'b'), (50, 'c'), (NULL, 'a')",
            None,
        )
        .is_err());
    assert_eq!(
        connection
            .prepare_select("SELECT id, code FROM users")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(1), Value::from_text("a")]]
    );
    let after_failure = connection
        .execute_checked_write("INSERT INTO users (code) VALUES ('after')", None)
        .unwrap();
    assert_eq!(after_failure.last_insert_id, 52);

    connection.execute_transaction_command("BEGIN").unwrap();
    let rolled_back = connection
        .execute_checked_write(
            "INSERT INTO users (id, code) VALUES (NULL, 'x'), (100, 'y'), (NULL, 'z')",
            None,
        )
        .unwrap();
    assert_eq!(rolled_back.last_insert_id, 53);
    connection.execute_transaction_command("ROLLBACK").unwrap();
    let after_rollback = connection
        .execute_checked_write("INSERT INTO users (code) VALUES ('later')", None)
        .unwrap();
    assert_eq!(after_rollback.last_insert_id, 102);

    connection.execute(
        "CREATE TABLE failed_explicit (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, code TEXT UNIQUE)",
    )?;
    connection
        .execute_checked_write("INSERT INTO failed_explicit (code) VALUES ('a')", None)
        .unwrap();
    assert!(connection
        .execute_checked_write(
            "INSERT INTO failed_explicit (id, code) VALUES (NULL, 'b'), (50, 'a')",
            None,
        )
        .is_err());
    let after_failed_explicit = connection
        .execute_checked_write("INSERT INTO failed_explicit (code) VALUES ('next')", None)
        .unwrap();
    assert_eq!(after_failed_explicit.last_insert_id, 4);
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_mixed_high_water_ids_preserve_bound_values() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-mixed-high.db", [0xab; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, code TEXT)")?;
    let metadata = connection
        .prepare_checked_statement("INSERT INTO users (id, code) VALUES (?, ?), (?, ?), (?, ?)")
        .unwrap();
    let result = connection
        .execute_prepared_statement(
            metadata.statement_id,
            &[
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Text("a".to_string()),
                MySqlPreparedValue::Integer(50),
                MySqlPreparedValue::Text("b".to_string()),
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Text("c".to_string()),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(
        result,
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 3,
            last_insert_id: 1,
        })
    );
    assert_eq!(
        connection
            .prepare_select("SELECT id, code FROM users ORDER BY code")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(1), Value::from_text("a")],
            vec![Value::from_i64(50), Value::from_text("b")],
            vec![Value::from_i64(51), Value::from_text("c")],
        ]
    );
    let next = connection
        .execute_checked_write("INSERT INTO users (code) VALUES ('d')", None)
        .unwrap();
    assert_eq!(next.last_insert_id, 52);
    connection.close()?;
    Ok(())
}

#[test]
fn held_high_water_lease_excludes_another_connections_reservation() -> Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database_identity = [0xac; 16];
    let path = "mysql-session-mixed-high-concurrent.db";
    let database =
        open_database_with_identity(Arc::clone(&io), path, OpenFlags::Create, database_identity)?;
    let allocator = DurableRangeAllocator::open(
        io.as_ref(),
        &format!("{path}.auto-increment"),
        AllocatorDatabaseIdentity::new(database_identity)?,
        AllocatorOpenMode::Create,
        FileSyncType::Fsync,
    )?;
    let mut initialization = allocator.initialize()?;
    io.block(|| initialization.step())?;
    let first = MySqlConnection::new_with_auto_increment_and_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        allocator.clone(),
        Arc::clone(&io),
        MySqlPreparedStatementAuthority::default(),
    )?;
    let second = MySqlConnection::new_with_auto_increment_and_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        allocator.clone(),
        Arc::clone(&io),
        MySqlPreparedStatementAuthority::default(),
    )?;
    first.execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, code TEXT)")?;
    let key = auto_increment_key(&first, "users")?;
    let mut lease = allocator.lease_high_water(key)?;
    assert_eq!(io.block(|| lease.read())?, 0);
    assert!(second
        .execute_checked_write("INSERT INTO users (code) VALUES ('waiting')", None)
        .is_err());
    assert_eq!(io.block(|| lease.advance_past(50))?, 50);
    lease.release()?;
    let inserted = second
        .execute_checked_write("INSERT INTO users (code) VALUES ('after')", None)
        .unwrap();
    assert_eq!(inserted.last_insert_id, 51);
    second.close()?;
    first.close()?;
    Ok(())
}

#[test]
fn prepared_auto_increment_accepts_explicit_and_null_ids() -> Result<()> {
    let (connection, allocator, io) =
        open_allocator_connection("mysql-session-prepared-mixed-ids.db", [0xa7; 16])?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    let mut seed = allocator.advance_past(auto_increment_key(&connection, "users")?, 16)?;
    io.block(|| seed.step())?;
    let metadata = connection
        .prepare_checked_statement("INSERT INTO users (id, name) VALUES (?, ?), (?, ?)")
        .unwrap();
    assert_eq!(metadata.parameter_count, 4);
    let result = connection
        .execute_prepared_statement(
            metadata.statement_id,
            &[
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Text("a".to_string()),
                MySqlPreparedValue::Integer(3),
                MySqlPreparedValue::Text("b".to_string()),
            ],
            None,
            MySqlAffectedRowsMode::Changed,
        )
        .unwrap();
    assert_eq!(
        result,
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 2,
            last_insert_id: 17,
        })
    );
    assert_eq!(connection.last_insert_id(), 17);
    assert_eq!(
        connection
            .prepare_select("SELECT id FROM users ORDER BY name")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(17)], vec![Value::from_i64(3)]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn multirow_ignore_and_upsert_reuse_skipped_generated_ids() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-multirow-conflict-ids.db", [0xa8; 16])?;
    connection.execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, code TEXT UNIQUE, value INT)")?;
    connection
        .execute_checked_write(
            "INSERT INTO users (code, value) VALUES ('a', 1), ('b', 2)",
            None,
        )
        .unwrap();
    let ignored = connection
        .execute_checked_write(
            "INSERT IGNORE INTO users (code, value) VALUES ('a', 11), ('c', 3), ('b', 22), ('d', 4)",
            None,
        )
        .unwrap();
    assert_eq!(ignored.affected_rows, 2);
    assert_eq!(ignored.last_insert_id, 3);
    let upserted = connection
        .execute_checked_write(
            "INSERT INTO users (code, value) VALUES ('a', 12), ('e', 5), ('b', 23), ('f', 6) ON DUPLICATE KEY UPDATE value = VALUES(value)",
            None,
        )
        .unwrap();
    assert_eq!(upserted.affected_rows, 6);
    assert_eq!(upserted.last_insert_id, 7);
    assert_eq!(connection.last_insert_id(), 7);
    assert_eq!(
        connection
            .prepare_select("SELECT id, value FROM users ORDER BY code")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(1), Value::from_i64(12)],
            vec![Value::from_i64(2), Value::from_i64(23)],
            vec![Value::from_i64(3), Value::from_i64(3)],
            vec![Value::from_i64(4), Value::from_i64(4)],
            vec![Value::from_i64(7), Value::from_i64(5)],
            vec![Value::from_i64(8), Value::from_i64(6)],
        ]
    );
    let after = connection
        .execute_checked_write("INSERT INTO users (code) VALUES ('after')", None)
        .unwrap();
    assert_eq!(after.last_insert_id, 11);
    connection.close()?;
    Ok(())
}

#[test]
fn failed_prepared_auto_increment_insert_burns_its_range_without_changing_last_id() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-prepared-auto-increment-failure.db",
        [0x75; 16],
    )?;
    connection.execute(
        "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT UNIQUE)",
    )?;
    let metadata = connection
        .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
        .unwrap();
    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("Ada".to_string())],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 1,
        })
    );
    assert!(matches!(
        connection.execute_prepared_statement(
            metadata.statement_id,
            &[MySqlPreparedValue::Text("Ada".to_string())],
            None,
            MySqlAffectedRowsMode::Changed,
        ),
        Err(MySqlPreparedStatementError::Engine(_))
    ));
    assert_eq!(connection.last_insert_id(), 1);
    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("Grace".to_string())],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 3,
        })
    );
    assert_eq!(
        connection
            .prepare_select("SELECT id, name FROM users")?
            .run_collect_rows()?,
        vec![
            vec![Value::from_i64(1), Value::from_text("Ada")],
            vec![Value::from_i64(3), Value::from_text("Grace")],
        ]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn rolled_back_prepared_auto_increment_insert_does_not_reuse_its_id() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-prepared-auto-increment-rollback.db",
        [0x78; 16],
    )?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    let metadata = connection
        .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
        .unwrap();

    connection.execute_transaction_command("BEGIN").unwrap();
    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("discarded".to_string())],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 1,
        })
    );
    connection.execute_transaction_command("ROLLBACK").unwrap();

    assert_eq!(
        connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("kept".to_string())],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap(),
        MySqlPreparedExecutionResult::Write(MySqlWriteResult {
            affected_rows: 1,
            last_insert_id: 2,
        })
    );
    assert_eq!(
        connection
            .prepare_select("SELECT id, name FROM users")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(2), Value::from_text("kept")]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_auto_increment_insert_zero_timeout_does_not_reserve() -> Result<()> {
    let (connection, allocator, io) = open_allocator_connection(
        "mysql-session-prepared-auto-increment-timeout.db",
        [0x76; 16],
    )?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;
    let metadata = connection
        .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
        .unwrap();
    assert!(matches!(
        connection.execute_prepared_statement(
            metadata.statement_id,
            &[MySqlPreparedValue::Text("late".to_string())],
            Some(Duration::ZERO),
            MySqlAffectedRowsMode::Changed,
        ),
        Err(MySqlPreparedStatementError::Engine(LimboError::Interrupt))
    ));
    let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
    assert_eq!(io.block(|| reservation.step())?.first(), 1);
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_auto_increment_updates_still_fail_closed() -> Result<()> {
    let (connection, _allocator, _io) = open_allocator_connection(
        "mysql-session-prepared-auto-increment-allocator-rejected.db",
        [0x77; 16],
    )?;
    connection
        .execute("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")?;

    assert!(connection
        .prepare_checked_statement("INSERT INTO users (id, name) VALUES (?, ?)")
        .is_ok());
    assert!(matches!(
        connection.prepare_checked_statement("UPDATE users SET id = ? WHERE TRUE"),
        Err(MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(message)))
            if message == "prepared AUTO_INCREMENT column updates are not supported"
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_statement_ids_are_monotonic_across_removal_and_clear() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-statement-ids.db", [0x6a; 16])?;

    let first = connection.prepare_checked_statement("SELECT 1").unwrap();
    let second = connection.prepare_checked_statement("SELECT 2").unwrap();
    assert_eq!((first.statement_id, second.statement_id), (1, 2));

    assert!(connection.remove_prepared_statement(first.statement_id));
    assert!(!connection.remove_prepared_statement(first.statement_id));
    assert_eq!(
        connection.prepared_statement_metadata(first.statement_id),
        None
    );

    let third = connection.prepare_checked_statement("SELECT 3").unwrap();
    assert_eq!(third.statement_id, 3);
    connection.clear_prepared_statements();
    assert_eq!(
        connection.prepared_statement_metadata(second.statement_id),
        None
    );
    assert_eq!(
        connection.prepared_statement_metadata(third.statement_id),
        None
    );

    let fourth = connection.prepare_checked_statement("SELECT 4").unwrap();
    assert_eq!(fourth.statement_id, 4);
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_statement_authority_enforces_limits_and_returns_failed_reservations(
) -> std::result::Result<(), Box<dyn Error>> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database = open_database(
        Arc::clone(&io),
        "mysql-session-prepared-quota.db",
        OpenFlags::Create,
    )?;
    let authority = MySqlPreparedStatementAuthority::new(1).unwrap();
    let first = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;
    let second = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;

    assert_eq!(authority.active_count(), 0);
    assert!(matches!(
        first.prepare_checked_statement("not a prepared statement"),
        Err(MySqlPreparedStatementError::Prepare(_))
    ));
    assert_eq!(authority.active_count(), 0);

    let prepared = first.prepare_checked_statement("SELECT 1")?;
    assert_eq!(authority.active_count(), 1);
    assert!(matches!(
        second.prepare_checked_statement("SELECT 2"),
        Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 1 })
    ));
    assert_eq!(authority.active_count(), 1);
    assert!(first.remove_prepared_statement(prepared.statement_id));
    assert_eq!(authority.active_count(), 0);
    second.prepare_checked_statement("SELECT 2")?;
    assert_eq!(authority.active_count(), 1);
    second.clear_prepared_statements();
    assert_eq!(authority.active_count(), 0);
    let closed = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;
    let closed_statement = closed.prepare_checked_statement("SELECT 3")?;
    let closed_clone = closed.clone();
    assert_eq!(authority.active_count(), 1);
    closed.close()?;
    assert_eq!(authority.active_count(), 0);
    assert!(closed
        .prepared_statement_metadata(closed_statement.statement_id)
        .is_none());
    drop(closed_clone);
    let dropped = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;
    dropped.prepare_checked_statement("SELECT 4")?;
    assert_eq!(authority.active_count(), 1);
    drop(dropped);
    assert_eq!(authority.active_count(), 0);
    first.close()?;
    second.close()?;
    Ok(())
}

#[test]
fn prepared_statement_authority_supports_zero_and_dynamic_lowering(
) -> std::result::Result<(), Box<dyn Error>> {
    assert_eq!(
        MySqlPreparedStatementAuthority::default().maximum(),
        DEFAULT_MAX_PREPARED_STMT_COUNT
    );
    assert!(matches!(
        MySqlPreparedStatementAuthority::new(MAX_PREPARED_STMT_COUNT + 1),
        Err(MySqlPreparedStatementAuthorityError::MaximumOutOfRange { maximum })
            if maximum == MAX_PREPARED_STMT_COUNT + 1
    ));
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database = open_database(
        Arc::clone(&io),
        "mysql-session-prepared-quota-lowering.db",
        OpenFlags::Create,
    )?;
    let zero = MySqlPreparedStatementAuthority::new(0).unwrap();
    let disabled = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        zero.clone(),
    )?;
    assert!(matches!(
        disabled.prepare_checked_statement("SELECT 1"),
        Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 0 })
    ));
    assert_eq!(zero.active_count(), 0);
    disabled.close()?;

    let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
    let connection = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;
    let first = connection.prepare_checked_statement("SELECT 1")?;
    let second = connection.prepare_checked_statement("SELECT 2")?;
    authority.set_maximum(1).unwrap();
    assert!(matches!(
        connection.prepare_checked_statement("SELECT 3"),
        Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 1 })
    ));
    connection.remove_prepared_statement(first.statement_id);
    assert_eq!(authority.active_count(), 1);
    assert!(matches!(
        connection.prepare_checked_statement("SELECT 3"),
        Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 1 })
    ));
    connection.remove_prepared_statement(second.statement_id);
    assert_eq!(authority.active_count(), 0);
    connection.prepare_checked_statement("SELECT 3")?;
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_statement_authority_returns_permits_for_prepare_failures_and_id_exhaustion(
) -> std::result::Result<(), Box<dyn Error>> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database = open_database(
        Arc::clone(&io),
        "mysql-session-prepared-quota-failure.db",
        OpenFlags::Create,
    )?;
    let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
    let connection = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;

    assert!(matches!(
        connection.prepare_checked_statement("SELECT * FROM missing_table"),
        Err(MySqlPreparedStatementError::Prepare(
            MySqlQueryError::Engine(_)
        ))
    ));
    assert_eq!(authority.active_count(), 0);

    let first_success = connection.prepare_checked_statement("SELECT 1")?;
    assert_eq!(first_success.statement_id, 1);
    connection.remove_prepared_statement(first_success.statement_id);
    assert_eq!(authority.active_count(), 0);

    {
        let mut registry = connection
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        registry.next_id = Some(u32::MAX);
    }
    let prepared = connection.prepare_checked_statement("SELECT 1")?;
    assert_eq!(prepared.statement_id, u32::MAX);
    assert_eq!(authority.active_count(), 1);
    assert!(matches!(
        connection.prepare_checked_statement("SELECT 2"),
        Err(MySqlPreparedStatementError::StatementIdExhausted)
    ));
    assert_eq!(authority.active_count(), 1);
    assert!(connection.remove_prepared_statement(u32::MAX));
    assert_eq!(authority.active_count(), 0);
    assert!(matches!(
        connection.prepare_checked_statement("SELECT 3"),
        Err(MySqlPreparedStatementError::StatementIdExhausted)
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_statement_authority_never_exceeds_its_limit_during_concurrent_reserve() {
    use std::sync::{Arc, Barrier};

    let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
    let barrier = Arc::new(Barrier::new(8));
    let workers = (0..8)
        .map(|_| {
            let authority = authority.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                authority.reserve().ok()
            })
        })
        .collect::<Vec<_>>();
    let permits = workers
        .into_iter()
        .map(|worker| worker.join().expect("quota worker panicked"))
        .collect::<Vec<_>>();
    assert_eq!(permits.iter().filter(|permit| permit.is_some()).count(), 2);
    assert_eq!(authority.active_count(), 2);
    drop(permits);
    assert_eq!(authority.active_count(), 0);
}

#[test]
fn concurrent_connection_prepares_never_exceed_the_shared_limit(
) -> std::result::Result<(), Box<dyn Error>> {
    use std::sync::{Arc, Barrier};

    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database = open_database(
        Arc::clone(&io),
        "mysql-session-prepared-quota-concurrent.db",
        OpenFlags::Create,
    )?;
    let authority = MySqlPreparedStatementAuthority::new(1).unwrap();
    let first = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;
    let second = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;
    let barrier = Arc::new(Barrier::new(2));
    let workers = [first, second]
        .into_iter()
        .map(|connection| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let result = connection.prepare_checked_statement("SELECT 1");
                (connection, result)
            })
        })
        .collect::<Vec<_>>();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("prepare worker panicked"))
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(authority.active_count(), 1);
    drop(results);
    assert_eq!(authority.active_count(), 0);
    Ok(())
}

#[test]
fn cleared_or_reset_in_flight_prepares_cannot_resurrect_a_statement(
) -> std::result::Result<(), Box<dyn Error>> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let database = open_database(
        Arc::clone(&io),
        "mysql-session-prepared-quota-generation.db",
        OpenFlags::Create,
    )?;
    let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
    let connection = MySqlConnection::new_with_prepared_statement_authority(
        database.connect()?,
        binary_context(),
        authority.clone(),
    )?;

    let reservation = connection.reserve_prepared_statement().unwrap();
    let statement_id = reservation.statement_id;
    connection.clear_prepared_statements();
    assert!(matches!(
        connection.commit_prepared_statement(
            reservation,
            None,
            MySqlPreparedStatementMetadata {
                statement_id,
                parameter_count: 0,
                result_columns: Vec::new(),
            },
            Vec::new(),
            Vec::new(),
            PreparedExecutionPlan::Select {
                reads_table: false,
                source_tables: Vec::new(),
                checked_comparisons: Vec::new(),
                row_count_parameters: Vec::new(),
            },
        ),
        Err(MySqlPreparedStatementError::Prepare(
            MySqlQueryError::Unsupported(message)
        )) if message == "prepared statement was cleared during prepare"
    ));
    assert_eq!(authority.active_count(), 0);
    assert!(connection
        .prepared_statement_metadata(statement_id)
        .is_none());

    let reservation = connection.reserve_prepared_statement().unwrap();
    let statement_id = reservation.statement_id;
    connection.reset_connection().unwrap();
    assert!(matches!(
        connection.commit_prepared_statement(
            reservation,
            None,
            MySqlPreparedStatementMetadata {
                statement_id,
                parameter_count: 0,
                result_columns: Vec::new(),
            },
            Vec::new(),
            Vec::new(),
            PreparedExecutionPlan::Select {
                reads_table: false,
                source_tables: Vec::new(),
                checked_comparisons: Vec::new(),
                row_count_parameters: Vec::new(),
            },
        ),
        Err(MySqlPreparedStatementError::Prepare(
            MySqlQueryError::Unsupported(message)
        )) if message == "prepared statement was cleared during prepare"
    ));
    assert_eq!(authority.active_count(), 0);
    connection.close()?;
    Ok(())
}

#[test]
fn prepared_statement_reset_clears_bindings_and_schema_sql_is_unsupported() -> Result<()> {
    let (connection, _allocator, _io) =
        open_allocator_connection("mysql-session-prepared-statement-reset.db", [0x6b; 16])?;
    let metadata = connection.prepare_checked_statement("SELECT ?").unwrap();
    connection
        .with_prepared_statement(metadata.statement_id, |statement| {
            statement.bind_at(std::num::NonZero::new(1).unwrap(), Value::from_i64(7))?;
            Ok(())
        })
        .unwrap();
    connection
        .reset_prepared_statement(metadata.statement_id)
        .unwrap();
    connection
        .with_prepared_statement(metadata.statement_id, |statement| {
            assert_eq!(statement.expanded_sql(), "SELECT NULL");
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        connection.prepare_checked_statement("CREATE TABLE users (id INT)"),
        Err(MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(message)))
            if message == "prepared statements support only SELECT, INSERT, UPDATE, and DELETE"
    ));
    assert!(matches!(
        connection.reset_prepared_statement(0),
        Err(MySqlPreparedStatementError::UnknownStatement { statement_id: 0 })
    ));
    connection.close()?;
    Ok(())
}

#[test]
fn strict_smallint_assignments_use_durable_mysql_ddl() -> Result<()> {
    use std::num::NonZeroUsize;

    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-strict-smallint.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE numbers (value SMALLINT, label TEXT)")?;
        connection.execute(
            "INSERT INTO numbers (value, label) VALUES (-32768, 'low'), (32767, 'high')",
        )?;

        let mut statement =
            connection.prepare("INSERT INTO numbers (value, label) VALUES (?, 'bound')")?;
        statement.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(-1))?;
        statement.run_ignore_rows()?;

        let mut statement = connection
            .prepare("INSERT INTO numbers (value, label) VALUES (?, 'prepared-overflow')")?;
        statement.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(32768))?;
        let error = statement.run_ignore_rows().unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));

        let error = connection
            .execute("INSERT INTO numbers (value, label) VALUES (-32769, 'underflow')")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));

        let error = connection
            .execute("INSERT INTO numbers (value, label) VALUES (0, 'kept'), (32768, 'rollback')")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT value, label FROM numbers ORDER BY rowid")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(-32768), Value::from_text("low")],
                vec![Value::from_i64(32767), Value::from_text("high")],
                vec![Value::from_i64(-1), Value::from_text("bound")],
            ]
        );

        let error = connection
            .inner()
            .execute("INSERT INTO numbers (value, label) VALUES (32768, 'raw')")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));

        connection.execute("CREATE TABLE source (value INT)")?;
        connection.execute(
            "CREATE TRIGGER copy_source AFTER INSERT ON source FOR EACH ROW BEGIN INSERT INTO numbers (value, label) VALUES (NEW.value, 'trigger'); END",
        )?;
        let error = connection
            .execute("INSERT INTO source (value) VALUES (32768)")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT COUNT(*) FROM source")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(0)]]
        );

        connection.execute("CREATE TEMPORARY TABLE temp_numbers (value SMALLINT)")?;
        let error = connection
            .execute("INSERT INTO temp_numbers (value) VALUES (32768)")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));
        connection.execute("INSERT INTO temp_numbers (value) VALUES (0)")?;
        let error = connection
            .execute("UPDATE temp_numbers SET value = 32768 WHERE TRUE")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));
        connection.inner().close()?;
    }

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.inner().execute("VACUUM")?;
        let error = connection
            .execute("UPDATE numbers SET value = 32768 WHERE TRUE")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
        ));
        connection.inner().close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT value FROM numbers WHERE label = 'low'")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(-32768)]]
    );
    connection.inner().close()?;
    Ok(())
}

#[test]
fn strict_bigint_assignments_use_durable_mysql_ddl() -> Result<()> {
    use std::num::NonZeroUsize;

    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-session-strict-bigint.db";
    {
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `numbers` (`value` BIGINT, `label` TEXT)")?;
        let stored = connection
            .inner()
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 'numbers'")?
            .run_collect_rows()?[0][0]
            .to_string();
        assert!(stored.contains("`value` BIGINT"));

        connection.execute(
            "INSERT INTO `numbers` (`value`, `label`) VALUES (-9223372036854775808, 'low'), (9223372036854775807, 'high')",
        )?;

        let mut parameterized = connection
            .prepare("INSERT INTO `numbers` (`value`, `label`) VALUES (?, 'prepared-low')")?;
        parameterized.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(i64::MIN))?;
        parameterized.run_ignore_rows()?;

        let mut parameterized = connection
            .prepare("INSERT INTO `numbers` (`value`, `label`) VALUES (?, 'prepared-high')")?;
        parameterized.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(i64::MAX))?;
        parameterized.run_ignore_rows()?;

        let error = connection
            .execute(
                "INSERT INTO `numbers` (`value`, `label`) VALUES (0, 'kept'), ('bad', 'rollback')",
            )
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::IncorrectType { type_name, .. } if type_name == "BIGINT")
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT label FROM numbers ORDER BY rowid")?
                .run_collect_rows()?,
            vec![
                vec![Value::build_text("low")],
                vec![Value::build_text("high")],
                vec![Value::build_text("prepared-low")],
                vec![Value::build_text("prepared-high")],
            ]
        );
        connection.inner().close()?;
    }

    {
        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.inner().execute("VACUUM")?;
        let columns = connection
            .list_columns(&MySqlTableName::parse("numbers").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(columns[0].type_name(), "BIGINT");

        let error = connection
            .execute("UPDATE `numbers` SET `value` = 'bad' WHERE TRUE")
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Assignment(error)
                if matches!(error.as_ref(), AssignmentError::IncorrectType { type_name, .. } if type_name == "BIGINT")
        ));
        connection.inner().close()?;
    }

    let db = open_database(io, path, OpenFlags::None)?;
    let connection = MySqlConnection::new(db.connect()?, binary_context())?;
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT value FROM numbers WHERE label = 'low'")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(i64::MIN)]]
    );
    assert_eq!(
        connection
            .inner()
            .prepare("SELECT value FROM numbers WHERE label = 'high'")?
            .run_collect_rows()?,
        vec![vec![Value::from_i64(i64::MAX)]]
    );
    connection.inner().close()?;
    Ok(())
}
