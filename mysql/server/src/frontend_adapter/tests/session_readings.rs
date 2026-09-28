//! The calls that read what a session knows about itself: who it is, which
//! connection it is, and what its last command changed and found.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// A session of the account `app`, which came over TCP from 172.17.0.9 as
/// connection 56, with `reports` selected.
fn session_from(peer: IpAddr) -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("app"));
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .with_connection_facts(MySqlConnectionFacts::over_tcp(56, Some(peer)))
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([81; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    (directory, adapter)
}

fn session() -> (tempfile::TempDir, Adapter) {
    session_from(IpAddr::V4(Ipv4Addr::new(172, 17, 0, 9)))
}

/// Runs one statement the way a `COM_QUERY` carrying it reaches the session.
fn query(adapter: &mut Adapter, sql: &str) -> Result<CommandExecutionResult, FrontendErrorKind> {
    adapter.command_arrived(ArrivedCommand::Query);
    adapter.execute_query(sql)
}

fn answered(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match query(adapter, sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql}: {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    answered(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn row(values: &[Option<&str>]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| value.map(str::to_owned))
        .collect()
}

/// `(ROW_COUNT(), FOUND_ROWS())`, each `None` when it is refused.
fn counts(adapter: &mut Adapter) -> (Option<String>, Option<String>) {
    let read = |adapter: &mut Adapter, sql: &str| {
        // Reading one is itself a statement answering rows, so each is read
        // from a copy of the session's counts taken before either.
        match adapter.execute_query(sql) {
            Ok(CommandExecutionResult::ResultSet(result)) => {
                Some(String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap())
            }
            Err(FrontendErrorKind::Unsupported) => None,
            other => panic!("{sql}: {other:?}"),
        }
    };
    adapter.command_arrived(ArrivedCommand::Query);
    let saved = adapter.session_variables.clone();
    let row_count = read(adapter, "SELECT ROW_COUNT()");
    adapter.session_variables = saved;
    let found_rows = read(adapter, "SELECT FOUND_ROWS()");
    (row_count, found_rows)
}

fn known(row_count: &str, found_rows: &str) -> (Option<String>, Option<String>) {
    (Some(row_count.to_owned()), Some(found_rows.to_owned()))
}

/// What the `mysql` client's `status` sends. Measured: `USER()` is the name the
/// client logged in with at the address it came from — the oracle resolves
/// no names — and each user call is a nullable `VAR_STRING` of 1152 with 31
/// decimals, `DATABASE()` one of 256.
#[test]
fn the_mysql_client_status_reads_the_database_and_the_user() {
    let (_directory, mut adapter) = session();
    let status = answered(&mut adapter, "select DATABASE(), USER() limit 1");
    assert_eq!(
        status.rows,
        [[Some(b"reports".to_vec()), Some(b"app@172.17.0.9".to_vec())]]
    );
    let shapes = status
        .columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shapes,
        [
            (
                "DATABASE()",
                MYSQL_TYPE_VAR_STRING,
                256,
                NOT_FIXED_DECIMALS,
                0
            ),
            ("USER()", MYSQL_TYPE_VAR_STRING, 1152, NOT_FIXED_DECIMALS, 0),
        ]
    );
}

/// `CURRENT_USER` is the account the login matched, which is one for any
/// host here, and may be written without its parentheses; `SESSION_USER()`
/// and `SYSTEM_USER()` are `USER()`. `CONNECTION_ID()` is the ID the
/// handshake sent, a NOT NULL unsigned `LONGLONG` of 21.
#[test]
fn the_user_calls_and_the_connection_id_read_the_login() {
    let (_directory, mut adapter) = session();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT CURRENT_USER(), CURRENT_USER, SESSION_USER(), SYSTEM_USER() AS s, CONNECTION_ID()"
        ),
        [row(&[
            Some("app@%"),
            Some("app@%"),
            Some("app@172.17.0.9"),
            Some("app@172.17.0.9"),
            Some("56"),
        ])]
    );
    let read = answered(&mut adapter, "SELECT CURRENT_USER, CONNECTION_ID()");
    assert_eq!(read.columns[0].name, "CURRENT_USER");
    assert_eq!(read.columns[0].column_length, 1152);
    let id = &read.columns[1];
    assert_eq!(id.column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(id.column_length, 21);
    assert_eq!(
        id.flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    // An IPv4 client a dual-stack socket reports inside IPv6 is written as
    // the IPv4 address.
    let mapped = Ipv4Addr::new(10, 0, 0, 5).to_ipv6_mapped();
    let (_directory, mut adapter) = session_from(IpAddr::V6(mapped));
    assert_eq!(
        rows(&mut adapter, "SELECT USER()"),
        [row(&[Some("app@10.0.0.5")])]
    );
    let (_directory, mut adapter) = session_from(IpAddr::V6(Ipv6Addr::LOCALHOST));
    assert_eq!(
        rows(&mut adapter, "SELECT USER()"),
        [row(&[Some("app@::1")])]
    );
}

/// What Django reads when it connects.
#[test]
fn django_reads_the_version_the_client_character_set_and_the_database() {
    let (_directory, mut adapter) = session();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT VERSION(), @@character_set_client, DATABASE()"
        ),
        [row(&[
            Some(crate::SERVER_VERSION),
            Some("utf8mb4"),
            Some("reports")
        ])]
    );
}

/// A connection the runtime did not accept knows neither who it is nor which
/// connection, and refuses the calls rather than guess.
#[test]
fn a_connection_that_knows_no_login_refuses_the_user_calls() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([81; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    for sql in [
        "SELECT USER()",
        "SELECT CURRENT_USER",
        "SELECT CONNECTION_ID()",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    // Nothing is selected yet, which `DATABASE()` answers NULL for.
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SELECT DATABASE(), VERSION()")
    else {
        panic!("DATABASE() is answered");
    };
    assert_eq!(result.rows[0][0], None);
}

/// Measured, statement by statement: a fresh connection reads 0 for both; a
/// statement answering rows makes `ROW_COUNT()` -1 and `FOUND_ROWS()` the
/// rows answered, `SHOW` included but `SHOW WARNINGS` leaving it alone; one
/// answering OK makes `ROW_COUNT()` the rows it changed and leaves
/// `FOUND_ROWS()` alone; and one that fails makes `ROW_COUNT()` -1.
#[test]
fn row_count_and_found_rows_read_what_the_last_statement_did() {
    let (_directory, mut adapter) = session();
    assert_eq!(counts(&mut adapter), known("0", "0"));

    let read = answered(&mut adapter, "SELECT ROW_COUNT(), FOUND_ROWS()");
    for column in &read.columns {
        assert_eq!(column.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(column.column_length, 21);
        assert_eq!(
            column.flags,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        );
    }

    query(&mut adapter, "CREATE TABLE fr (id INT PRIMARY KEY, v INT)").unwrap();
    query(&mut adapter, "INSERT INTO fr VALUES (1, 1), (2, 2), (3, 3)").unwrap();
    assert_eq!(counts(&mut adapter), known("3", "1"));
    answered(&mut adapter, "SELECT * FROM fr WHERE id > 1");
    assert_eq!(counts(&mut adapter), known("-1", "2"));
    query(&mut adapter, "DELETE FROM fr WHERE id = 3").unwrap();
    assert_eq!(counts(&mut adapter), known("1", "1"));
    answered(&mut adapter, "SHOW TABLES");
    assert_eq!(counts(&mut adapter), known("-1", "2"));
    answered(&mut adapter, "SELECT * FROM fr");
    query(&mut adapter, "SET @a = 1").unwrap();
    assert_eq!(counts(&mut adapter), known("0", "2"));
    answered(&mut adapter, "SELECT * FROM fr");
    answered(&mut adapter, "SHOW WARNINGS");
    assert_eq!(counts(&mut adapter), known("-1", "2"));
    answered(&mut adapter, "SELECT * FROM fr");
    assert!(query(&mut adapter, "INSERT INTO fr VALUES (1, 1)").is_err());
    assert_eq!(counts(&mut adapter), known("-1", "2"));

    // An `UPDATE` makes `FOUND_ROWS()` the rows it matched, which it does not
    // report here.
    query(&mut adapter, "UPDATE fr SET v = 1").unwrap();
    assert_eq!(counts(&mut adapter), (Some("1".to_owned()), None));

    // `COM_PING` makes `ROW_COUNT()` 0.
    answered(&mut adapter, "SELECT * FROM fr");
    adapter.command_arrived(ArrivedCommand::Ping);
    assert_eq!(counts(&mut adapter), known("0", "2"));

    // A command whose effect was not measured leaves both unknown, until a
    // statement says what they are again; so does a `COM_QUERY` refused
    // before any statement of it ran.
    adapter.command_arrived(ArrivedCommand::Other);
    assert_eq!(counts(&mut adapter), (None, None));
    answered(&mut adapter, "SELECT * FROM fr");
    assert_eq!(counts(&mut adapter), known("-1", "2"));
    adapter.command_arrived(ArrivedCommand::Query);
    assert_eq!(counts(&mut adapter), (None, None));

    // `COM_INIT_DB` makes `ROW_COUNT()` 0.
    answered(&mut adapter, "SELECT * FROM fr");
    adapter.command_arrived(ArrivedCommand::InitDb);
    adapter.execute_init_db("reports").unwrap();
    assert_eq!(counts(&mut adapter), known("0", "2"));
}
