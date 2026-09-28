//! Statements sqlx 0.8.6 and its `sqlx` CLI send, replayed as the framework
//! harness's eighth run captured them, over text and prepared statements as
//! sqlx sends each one.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const ACCOUNT: [u8; 32] = [197; 32];

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    (directory, connected(factory))
}

/// A new session over the same files, the catalog opened again from disk
/// once the session before it has let go of it.
fn reopened(directory: &tempfile::TempDir, adapter: Adapter) -> Adapter {
    drop(adapter);
    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    let factory = AuthorizedDatabaseAdapterFactory::new(
        catalog,
        binary_context(),
        Arc::new(RecordingAuthorizer::default()),
    );
    connected(factory)
}

fn connected(factory: AuthorizedDatabaseAdapterFactory<RecordingAuthorizer>) -> Adapter {
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes(ACCOUNT),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    run(&mut adapter, SESSION);
    adapter
}

/// What sqlx sends first on every connection.
const SESSION: &str = "SET sql_mode=(SELECT CONCAT(@@sql_mode, ',PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION')),time_zone='+00:00',NAMES utf8mb4 COLLATE utf8mb4_unicode_ci;";

/// The table `sqlx migrate` keeps its record in, created before every run.
const MIGRATIONS_TABLE: &str = "\nCREATE TABLE IF NOT EXISTS _sqlx_migrations (\n    version BIGINT PRIMARY KEY,\n    description TEXT NOT NULL,\n    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n    success BOOLEAN NOT NULL,\n    checksum BLOB NOT NULL,\n    execution_time BIGINT NOT NULL\n);\n                ";

const RECORD_MIGRATION: &str = "\n    INSERT INTO _sqlx_migrations ( version, description, success, checksum, execution_time )\n    VALUES ( ?, ?, FALSE, ?, -1 )\n                ";

/// One value as sqlx 0.8.6 binds it.
#[derive(Clone, Copy, Debug)]
enum Bound<'a> {
    /// `&str`, bound as `VAR_STRING`.
    Word(&'a str),
    /// `i64`, bound as `LONGLONG`.
    Whole(i64),
    /// `&[u8]`, bound as `BLOB`.
    Bytes(&'a [u8]),
}

/// The null bitmap, the new-parameters flag, the types and the values, or
/// nothing at all for a statement without parameters, as sqlx sends it.
fn payload(values: &[Bound<'_>]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        let code = match value {
            Bound::Word(_) => MYSQL_TYPE_VAR_STRING,
            Bound::Whole(_) => MYSQL_TYPE_LONGLONG,
            Bound::Bytes(_) => MYSQL_TYPE_BLOB,
        };
        payload.extend_from_slice(&[code, 0]);
    }
    for value in values {
        match value {
            Bound::Word(text) => lenenc(&mut payload, text.as_bytes()),
            Bound::Bytes(bytes) => lenenc(&mut payload, bytes),
            Bound::Whole(number) => payload.extend_from_slice(&number.to_le_bytes()),
        }
    }
    payload
}

fn lenenc(payload: &mut Vec<u8>, bytes: &[u8]) {
    if let Ok(length) = u8::try_from(bytes.len()) {
        if length < 251 {
            payload.push(length);
            payload.extend_from_slice(bytes);
            return;
        }
    }
    payload.push(0xfc);
    payload.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_le_bytes());
    payload.extend_from_slice(bytes);
}

fn prepared(
    adapter: &mut Adapter,
    sql: &str,
    values: &[Bound<'_>],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    let statement = adapter.execute_stmt_prepare(sql)?;
    let result = adapter.execute_stmt_execute(statement.statement_id, &payload(values));
    adapter.execute_stmt_close(statement.statement_id);
    result
}

fn prepared_rows(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> BinaryResultSet {
    match prepared(adapter, sql, values) {
        Ok(PreparedStatementExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must answer rows, answered {other:?}"),
    }
}

fn prepared_changes(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> CommandOkResult {
    match prepared(adapter, sql, values) {
        Ok(PreparedStatementExecutionResult::Ok(ok)) => ok,
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let result = adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    let CommandExecutionResult::ResultSet(result) = result else {
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

/// `sqlx migrate run` keeps its record under a signed `BIGINT PRIMARY KEY`,
/// the version being the migration's timestamp. MySQL takes a signed key as
/// it takes an `INT` one: a negative version is a version like any other, a
/// second row with the same one is 1062 and a row without one is 1364.
#[test]
fn sqlx_keeps_its_migrations_under_a_signed_bigint_key() {
    let (directory, mut adapter) = adapter();
    run(&mut adapter, MIGRATIONS_TABLE);
    let mut adapter = reopened(&directory, adapter);
    run(&mut adapter, MIGRATIONS_TABLE);
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TABLE _sqlx_migrations")[0][1],
        Some(
            "CREATE TABLE `_sqlx_migrations` (\n  `version` bigint NOT NULL,\n  `description` text NOT NULL,\n  `installed_on` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,\n  `success` tinyint(1) NOT NULL,\n  `checksum` blob NOT NULL,\n  `execution_time` bigint NOT NULL,\n  PRIMARY KEY (`version`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
                .to_owned()
        )
    );

    let checksum: Vec<u8> = (0..48).map(|byte| byte * 5).collect();
    let recorded = prepared_changes(
        &mut adapter,
        RECORD_MIGRATION,
        &[
            Bound::Whole(20260101000000),
            Bound::Word("create blog"),
            Bound::Bytes(&checksum),
        ],
    );
    assert_eq!(recorded.affected_rows, 1);
    run(
        &mut adapter,
        "INSERT INTO _sqlx_migrations ( version, description, success, checksum, execution_time ) VALUES ( -5, 'negative', FALSE, x'00', -1 )",
    );
    for (sql, refusal) in [
        (
            "INSERT INTO _sqlx_migrations ( version, description, success, checksum, execution_time ) VALUES ( -5, 'again', FALSE, x'00', -1 )",
            FrontendErrorKind::ConstraintViolation,
        ),
        (
            "INSERT INTO _sqlx_migrations ( description, success, checksum, execution_time ) VALUES ( 'no version', FALSE, x'00', -1 )",
            FrontendErrorKind::MissingRequiredDefault,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(refusal), "{sql}");
    }

    let result = prepared_rows(
        &mut adapter,
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
        &[],
    );
    let [version, stored] = result.columns.as_slice() else {
        panic!("two columns");
    };
    assert_eq!(
        (version.column_type, version.flags, version.column_length),
        (MYSQL_TYPE_LONGLONG, 0x5003, 20),
        "NOT_NULL PRI_KEY NO_DEFAULT_VALUE PART_KEY"
    );
    assert_eq!(
        (stored.column_type, stored.flags),
        (MYSQL_TYPE_BLOB, 0x1091)
    );
    assert_eq!(
        result.rows,
        [
            vec![
                BinaryResultValue::Integer(-5),
                BinaryResultValue::Blob(vec![0])
            ],
            vec![
                BinaryResultValue::Integer(20260101000000),
                BinaryResultValue::Blob(checksum),
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COLUMN_NAME, COLUMN_TYPE, COLUMN_KEY, IS_NULLABLE FROM information_schema.COLUMNS WHERE TABLE_NAME = '_sqlx_migrations' AND COLUMN_NAME = 'version'",
        ),
        [[
            Some("version".to_owned()),
            Some("bigint".to_owned()),
            Some("PRI".to_owned()),
            Some("NO".to_owned()),
        ]]
    );
}
