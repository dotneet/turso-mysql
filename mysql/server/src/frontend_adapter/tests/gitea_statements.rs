//! Statements Gitea v1.27.3's integration suite sends through xorm and
//! go-sql-driver/mysql, replayed as the framework harness captured them, over
//! text and prepared statements as the driver sends each one.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([211; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

/// One value as go-sql-driver/mysql binds it.
#[derive(Clone, Copy, Debug)]
enum Bound<'a> {
    /// A Go `string`, bound as `STRING`.
    Word(&'a str),
}

/// The null bitmap, the new-parameters flag, the types and the values, as
/// go-sql-driver/mysql sends them.
fn payload(values: &[Bound<'_>]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        let code = match value {
            Bound::Word(_) => MYSQL_TYPE_STRING,
        };
        payload.extend_from_slice(&[code, 0]);
    }
    for value in values {
        match value {
            Bound::Word(text) => {
                payload.push(u8::try_from(text.len()).unwrap());
                payload.extend_from_slice(text.as_bytes());
            }
        }
    }
    payload
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

/// What xorm's `GetTables` asks before it syncs a schema, and again for every
/// `DBMetas`.
const XORM_GET_TABLES: &str = "SELECT `TABLE_NAME`, `ENGINE`, `AUTO_INCREMENT`, `TABLE_COMMENT`, `TABLE_COLLATION` from `INFORMATION_SCHEMA`.`TABLES` WHERE `TABLE_SCHEMA`=? AND (`ENGINE`='MyISAM' OR `ENGINE` = 'InnoDB' OR `ENGINE` = 'TokuDB')";

/// Measured on MySQL 8.4.11: `AUTO_INCREMENT` is what `SHOW CREATE TABLE`
/// prints as `AUTO_INCREMENT=<n>` — NULL for a table that counts nothing and
/// for one that has handed out no number yet — and an unsigned `LONGLONG` of
/// 21 with the binary flag.
#[test]
fn xorms_table_listing_answers_each_counters_next_number() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `version` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `version` BIGINT(20) NULL)",
    );
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `auth_token` (`id` VARCHAR(255) PRIMARY KEY NOT NULL, `token_hash` VARCHAR(255) NULL, `user_id` BIGINT(20) NULL, `expires_unix` BIGINT(20) NULL) ENGINE=InnoDB",
    );
    let listing = |adapter: &mut Adapter| {
        let result = prepared_rows(adapter, XORM_GET_TABLES, &[Bound::Word("reports")]);
        let counter = &result.columns[2];
        assert_eq!(counter.name, "AUTO_INCREMENT");
        assert_eq!(counter.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(counter.column_length, 21);
        assert_eq!(
            counter.flags,
            MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        );
        result.rows
    };
    let text = |value: &str| BinaryResultValue::Text(value.to_owned());
    let table = |name: &str, counter: BinaryResultValue| {
        vec![
            text(name),
            text("InnoDB"),
            counter,
            BinaryResultValue::Blob(Vec::new()),
            text("utf8mb4_0900_ai_ci"),
        ]
    };
    assert_eq!(
        listing(&mut adapter),
        vec![
            table("auth_token", BinaryResultValue::Null),
            table("records", BinaryResultValue::Null),
            table("version", BinaryResultValue::Null),
        ]
    );
    run(
        &mut adapter,
        "INSERT INTO `version` (`version`) VALUES (1), (2)",
    );
    assert_eq!(
        listing(&mut adapter)[2][2],
        BinaryResultValue::UnsignedInteger(3)
    );
    let text = |value: &str| Some(value.to_owned());
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, AUTO_INCREMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()"
        ),
        vec![
            vec![text("auth_token"), None],
            vec![text("records"), None],
            vec![text("version"), text("3")],
        ]
    );
}
