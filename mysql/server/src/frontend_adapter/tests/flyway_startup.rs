//! What Flyway 11.7 sends through Connector/J 9.7 before and while it
//! migrates a MySQL database — replayed exactly as the framework harness
//! logged it, over text and prepared statements alike.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// Flyway's check that the database holds nothing, with the database written.
const EMPTINESS: &str = "SELECT SUM(found) FROM ((SELECT 1 as found FROM information_schema.tables WHERE table_schema='spring') UNION ALL (SELECT 1 as found FROM information_schema.views WHERE table_schema='spring' LIMIT 1) UNION ALL (SELECT 1 as found FROM information_schema.table_constraints WHERE table_schema='spring' LIMIT 1) UNION ALL (SELECT 1 as found FROM information_schema.triggers WHERE event_object_schema='spring'  LIMIT 1) UNION ALL (SELECT 1 as found FROM information_schema.routines WHERE routine_schema='spring' LIMIT 1) UNION ALL (SELECT 1 as found FROM information_schema.events WHERE event_schema='spring' LIMIT 1)) as all_found";

const HISTORY_TABLE: &str = "CREATE TABLE `spring`.`flyway_schema_history` (\n    `installed_rank` INT NOT NULL,\n    `version` VARCHAR(50),\n    `description` VARCHAR(200) NOT NULL,\n    `type` VARCHAR(20) NOT NULL,\n    `script` VARCHAR(1000) NOT NULL,\n    `checksum` INT,\n    `installed_by` VARCHAR(100) NOT NULL,\n    `installed_on` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n    `execution_time` INT NOT NULL,\n    `success` BOOL NOT NULL,\n    CONSTRAINT `flyway_schema_history_pk` PRIMARY KEY (`installed_rank`)\n)";

const HISTORY_INDEX: &str =
    "CREATE INDEX `flyway_schema_history_s_idx` ON `spring`.`flyway_schema_history` (`success`)";

/// A session of the account `e2e`, which came over TCP, with `spring`
/// selected.
fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("e2e"));
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("spring").unwrap();
    let mut adapter = factory
        .with_connection_facts(MySqlConnectionFacts::over_tcp(
            12,
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(172, 18, 0, 5)),
                40964,
            )),
        ))
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([154; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("spring").unwrap();
    (directory, adapter)
}

fn answered(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql}: {other:?}"),
    }
}

fn run(adapter: &mut Adapter, sql: &str) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(_)) => {}
        other => panic!("{sql}: {other:?}"),
    }
}

fn values(result: &TextResultSet) -> Vec<Vec<Option<String>>> {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| value.clone().map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

/// Prepares, executes with each `?` bound to a word, and closes.
fn prepared(adapter: &mut Adapter, sql: &str, words: &[&str]) -> BinaryResultSet {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = execute(adapter, statement.statement_id, words)
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"));
    adapter.execute_stmt_close(statement.statement_id);
    result
}

fn execute(
    adapter: &mut Adapter,
    statement_id: u32,
    words: &[&str],
) -> Result<BinaryResultSet, FrontendErrorKind> {
    let mut payload = Vec::new();
    if !words.is_empty() {
        payload.extend(vec![0u8; words.len().div_ceil(8)]);
        payload.push(1);
        for _ in words {
            payload.extend([MYSQL_TYPE_VAR_STRING, 0]);
        }
        for word in words {
            payload.push(u8::try_from(word.len()).unwrap());
            payload.extend(word.as_bytes());
        }
    }
    match adapter.execute_stmt_execute(statement_id, &payload)? {
        PreparedStatementExecutionResult::ResultSet(result) => Ok(result),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn shape(column: &ColumnDefinitionConfig) -> (&str, u8, u32, u8, u16) {
    (
        column.name.as_str(),
        column.column_type,
        column.column_length,
        column.decimals,
        column.flags,
    )
}

/// The probes Flyway makes before it reads anything. Each was answered with an
/// error, which Flyway tolerates but logs; each now answers what MySQL does.
///
/// `@@performance_schema` reads 0 here, and measured on MySQL started with it
/// off, `global_variables` still lists the server's variables while
/// `user_variables_by_thread` lists nothing even after a session set one.
#[test]
fn flyway_s_probes_answer_what_mysql_answers() {
    let (_directory, mut adapter) = adapter();

    let gtid = answered(&mut adapter, "SELECT @@GLOBAL.ENFORCE_GTID_CONSISTENCY");
    assert_eq!(values(&gtid), [[Some("OFF".to_owned())]]);
    assert_eq!(
        shape(&gtid.columns[0]),
        (
            "@@GLOBAL.ENFORCE_GTID_CONSISTENCY",
            MYSQL_TYPE_VAR_STRING,
            87_380,
            NOT_FIXED_DECIMALS,
            0
        )
    );
    // Measured: 1238, the variable being the server's.
    assert!(adapter
        .execute_query("SELECT @@SESSION.enforce_gtid_consistency")
        .is_err());

    let strict = answered(
        &mut adapter,
        "select VARIABLE_VALUE from performance_schema.global_variables where variable_name = 'pxc_strict_mode'",
    );
    assert!(strict.rows.is_empty());
    assert_eq!(
        shape(&strict.columns[0]),
        ("VARIABLE_VALUE", MYSQL_TYPE_VAR_STRING, 4096, 0, 0)
    );
    assert_eq!(strict.columns[0].schema, "performance_schema");
    assert_eq!(strict.columns[0].table, "global_variables");
    assert_eq!(
        values(&answered(
            &mut adapter,
            "select VARIABLE_VALUE from performance_schema.global_variables where variable_name = 'autocommit'",
        )),
        [[Some("ON".to_owned())]]
    );

    run(&mut adapter, "SET @flyway_probe = 1");
    let user_variables = answered(
        &mut adapter,
        "SELECT variable_name FROM performance_schema.user_variables_by_thread WHERE variable_value IS NOT NULL",
    );
    assert!(user_variables.rows.is_empty());
    assert_eq!(
        shape(&user_variables.columns[0]),
        (
            "variable_name",
            MYSQL_TYPE_VAR_STRING,
            256,
            0,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_PRI_KEY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG
        )
    );
    assert_eq!(user_variables.columns[0].original_name, "VARIABLE_NAME");

    // Flyway asks whether it runs on RDS. The pattern matches a name by its
    // case, as `SHOW TABLES LIKE` does.
    let rds = answered(&mut adapter, "SHOW DATABASES LIKE 'RDSAdmin';");
    assert!(rds.rows.is_empty());
    assert_eq!(rds.columns[0].name, "Database (RDSAdmin)");
    assert_eq!(
        values(&answered(&mut adapter, "SHOW DATABASES LIKE 'spr%'")),
        [[Some("spring".to_owned())]]
    );
    assert!(answered(&mut adapter, "SHOW DATABASES LIKE 'SPR%'")
        .rows
        .is_empty());

    // Who is running the migration, which Flyway records as `installed_by`.
    let user = answered(&mut adapter, "SELECT SUBSTRING_INDEX(USER(),'@',1)");
    assert_eq!(values(&user), [[Some("e2e".to_owned())]]);
    assert_eq!(
        shape(&user.columns[0]),
        (
            "SUBSTRING_INDEX(USER(),'@',1)",
            MYSQL_TYPE_VAR_STRING,
            1152,
            NOT_FIXED_DECIMALS,
            0
        )
    );
    let user = prepared(&mut adapter, "SELECT SUBSTRING_INDEX(USER(),'@',1)", &[]);
    assert_eq!(user.rows, [[BinaryResultValue::Text("e2e".to_owned())]]);

    let events = answered(
        &mut adapter,
        "SELECT event_name FROM information_schema.events WHERE event_schema='spring'",
    );
    assert!(events.rows.is_empty());
    assert_eq!(
        shape(&events.columns[0]),
        (
            "EVENT_NAME",
            MYSQL_TYPE_VAR_STRING,
            256,
            0,
            MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG
        )
    );
    assert_eq!(events.columns[0].original_table, "EVENTS");
    assert!(prepared(
        &mut adapter,
        "SELECT event_name FROM information_schema.events WHERE event_schema=?",
        &["spring"],
    )
    .rows
    .is_empty());
    // DBeaver reads the whole table: MySQL's twenty-four columns.
    let every = answered(
        &mut adapter,
        "SELECT * FROM information_schema.EVENTS WHERE EVENT_SCHEMA='spring'",
    );
    assert!(every.rows.is_empty());
    assert_eq!(every.columns.len(), 24);
    assert_eq!(
        shape(&every.columns[20]),
        (
            "ORIGINATOR",
            MYSQL_TYPE_LONG,
            10,
            0,
            MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG
        )
    );
}

/// Flyway's check that the database is empty, then its history table made
/// with the database written before the table, then read back.
///
/// Measured: the sum is a nullable `NEWDECIMAL` of 42, NULL over an empty
/// database and 2 once the history table is there — one for the table and
/// one for its primary key among the constraints.
#[test]
fn flyway_finds_the_database_empty_and_keeps_its_history_in_it() {
    let (_directory, mut adapter) = adapter();
    let bound = EMPTINESS.replace("'spring'", "?");
    let spring = ["spring"; 6];

    let empty = answered(&mut adapter, EMPTINESS);
    assert_eq!(values(&empty), [[None]]);
    assert_eq!(
        shape(&empty.columns[0]),
        (
            "SUM(found)",
            MYSQL_TYPE_NEWDECIMAL,
            42,
            0,
            MYSQL_BINARY_FLAG
        )
    );
    assert_eq!(
        prepared(&mut adapter, &bound, &spring).rows,
        [[BinaryResultValue::Null]]
    );

    run(&mut adapter, HISTORY_TABLE);
    run(&mut adapter, HISTORY_INDEX);
    run(
        &mut adapter,
        "INSERT INTO `spring`.`flyway_schema_history` (`installed_rank`, `version`, `description`, `type`, `script`, `checksum`, `installed_by`, `execution_time`, `success`) VALUES (1, '1', 'create blog', 'SQL', 'V1__create_blog.sql', 491316118, 'e2e', 68, 1)",
    );
    assert_eq!(
        values(&answered(&mut adapter, EMPTINESS)),
        [[Some("2".to_owned())]]
    );
    assert_eq!(
        prepared(&mut adapter, &bound, &spring).rows,
        [[BinaryResultValue::Text("2".to_owned())]]
    );
    // The catalog tables list the selected database alone, so a count over
    // another would call it empty whatever it holds.
    assert_eq!(
        adapter.execute_query(&EMPTINESS.replace("'spring'", "'other'")),
        Err(FrontendErrorKind::Unsupported)
    );

    let history = answered(
        &mut adapter,
        "SELECT `installed_rank`,`version`,`description`,`type`,`script`,`checksum`,`installed_on`,`installed_by`,`execution_time`,`success` FROM `spring`.`flyway_schema_history` WHERE `installed_rank` > -1 ORDER BY `installed_rank`",
    );
    assert_eq!(history.rows.len(), 1);
    // Measured: the column its own index starts with carries the
    // multiple-key flag, and a TIMESTAMP the server fills in the timestamp
    // one.
    assert_eq!(
        history.columns[6].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_TIMESTAMP_FLAG
    );
    assert_eq!(
        history.columns[9].flags,
        MYSQL_NOT_NULL_FLAG
            | MYSQL_MULTIPLE_KEY_FLAG
            | MYSQL_NO_DEFAULT_VALUE_FLAG
            | MYSQL_PART_KEY_FLAG
    );
    let created = answered(&mut adapter, "SHOW CREATE TABLE flyway_schema_history");
    let created = String::from_utf8(created.rows[0][1].clone().unwrap()).unwrap();
    assert!(created.starts_with("CREATE TABLE `flyway_schema_history` ("));
    assert!(created.contains("KEY `flyway_schema_history_s_idx` (`success`)"));
}

/// Connector/J prepares the database check once and runs it again inside the
/// transaction Flyway opens after `SET foreign_key_checks=1`. Switching the
/// check asks every prepared statement to be read again, and one reading an
/// `information_schema` table refused to be, answering 1235.
#[test]
fn a_prepared_catalog_read_survives_switching_foreign_key_checks() {
    let (_directory, mut adapter) = adapter();
    let statement = adapter
        .execute_stmt_prepare(
            "SELECT COUNT(1) FROM information_schema.schemata WHERE schema_name=? LIMIT 1",
        )
        .unwrap();
    for setting in [
        "SET autocommit=0",
        "SET foreign_key_checks=1, sql_safe_updates=0",
    ] {
        run(&mut adapter, setting);
        assert_eq!(
            execute(&mut adapter, statement.statement_id, &["spring"])
                .unwrap()
                .rows,
            [[BinaryResultValue::Integer(1)]],
            "after {setting}"
        );
    }
    run(&mut adapter, "SET foreign_key_checks=0");
    assert_eq!(
        execute(&mut adapter, statement.statement_id, &["spring"])
            .unwrap()
            .rows,
        [[BinaryResultValue::Integer(1)]]
    );
}
