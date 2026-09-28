//! The calls that read what a session knows about itself: who it is, which
//! connection it is, and what its last command changed and found.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use turso_mysql::session_registry::RunningStatement;

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// A session of the account `app`, which came over TCP from 172.17.0.9 as
/// connection 56, with `reports` selected.
fn session_from(peer: IpAddr) -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("app"));
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .with_connection_facts(MySqlConnectionFacts::over_tcp(
            56,
            Some(SocketAddr::new(peer, 40964)),
        ))
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

/// A session over `catalog` of `account`, connection `id` from 172.17.0.3.
fn listed_session(
    catalog: &Arc<MySqlDatabaseCatalog>,
    account: &str,
    id: u32,
    port: u16,
) -> Adapter {
    let mut adapter = AuthorizedDatabaseAdapterFactory::new(
        Arc::clone(catalog),
        binary_context(),
        Arc::new(RecordingAuthorizer::with_schema_creator(account)),
    )
    .with_connection_facts(MySqlConnectionFacts::over_tcp(
        id,
        Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(172, 17, 0, 3)),
            port,
        )),
    ))
    .build(AuthenticatedPrincipal::from_account_id_for_testing(
        AccountId::from_bytes([81; 32]),
    ))
    .unwrap();
    adapter.authorize_connection().unwrap();
    adapter
}

/// Measured on MySQL 8.4.11 for an account without `PROCESS`, which every
/// account here is: `SHOW PROCESSLIST` lists every connection of its own
/// account in the order of their IDs and none of another's, a waiting one as
/// `Sleep` with an empty state and no statement, one running a statement as
/// `Query` with it, and the asking one as `Query` in `init`. Without `FULL`
/// the statement is cut to 100 characters.
#[test]
fn processlist_lists_the_sessions_of_the_asking_account() {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("app"));
    let (_directory, catalog, _factory) = catalog_factory(authorizer);
    let mut idle = listed_session(&catalog, "app", 80, 40964);
    idle.execute_init_db("reports").unwrap();
    let busy = listed_session(&catalog, "app", 83, 41004);
    let _other_account = listed_session(&catalog, "reader", 82, 40990);
    let mut asking = listed_session(&catalog, "app", 84, 41020);
    asking.execute_init_db("reports").unwrap();
    busy.listed
        .as_ref()
        .unwrap()
        .statement_began(RunningStatement {
            command: "Query",
            text: "SELECT SLEEP(3)".to_owned(),
        });

    let listed = answered(&mut asking, "SHOW PROCESSLIST");
    assert_eq!(
        listed
            .rows
            .iter()
            .map(|row| row
                .iter()
                .map(|value| value
                    .as_ref()
                    .map(|value| String::from_utf8(value.clone()).unwrap()))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            row(&[
                Some("80"),
                Some("app"),
                Some("172.17.0.3:40964"),
                Some("reports"),
                Some("Sleep"),
                Some("0"),
                Some(""),
                None
            ]),
            row(&[
                Some("83"),
                Some("app"),
                Some("172.17.0.3:41004"),
                None,
                Some("Query"),
                Some("0"),
                Some("executing"),
                Some("SELECT SLEEP(3)")
            ]),
            row(&[
                Some("84"),
                Some("app"),
                Some("172.17.0.3:41020"),
                Some("reports"),
                Some("Query"),
                Some("0"),
                Some("init"),
                Some("SHOW PROCESSLIST")
            ]),
        ]
    );
    assert_eq!(
        listed
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.column_length
            ))
            .collect::<Vec<_>>(),
        [
            ("Id", MYSQL_TYPE_LONGLONG, 22),
            ("User", MYSQL_TYPE_VAR_STRING, 128),
            ("Host", MYSQL_TYPE_VAR_STRING, 1020),
            ("db", MYSQL_TYPE_VAR_STRING, 256),
            ("Command", MYSQL_TYPE_VAR_STRING, 64),
            ("Time", MYSQL_TYPE_LONG, 8),
            ("State", MYSQL_TYPE_VAR_STRING, 120),
            ("Info", MYSQL_TYPE_VAR_STRING, 400),
        ]
    );

    let long = format!("/* {} */ SHOW PROCESSLIST", "y".repeat(120));
    let cut = answered(&mut asking, &long);
    assert_eq!(
        cut.rows[2][7].as_deref(),
        Some(long.chars().take(100).collect::<String>().as_bytes())
    );
    let whole = format!("/* {} */ SHOW FULL PROCESSLIST", "y".repeat(120));
    let full = answered(&mut asking, &whole);
    assert_eq!(full.rows[2][7].as_deref(), Some(whole.as_bytes()));
    assert_eq!(full.columns[7].column_type, MYSQL_TYPE_LONG_BLOB);

    drop(idle);
    assert_eq!(answered(&mut asking, "SHOW PROCESSLIST").rows.len(), 2);
}

/// A session the runtime did not accept is not among them, and cannot list
/// them.
#[test]
fn a_session_the_runtime_did_not_accept_lists_no_process() {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([81; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    for sql in [
        "SHOW PROCESSLIST",
        "SHOW GLOBAL STATUS LIKE 'Threads_connected'",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    assert_eq!(
        rows(&mut adapter, "SHOW STATUS LIKE 'Uptime'")[0][0].as_deref(),
        Some("Uptime")
    );
    assert_eq!(adapter.statistics(), None);
}

/// The `mysql` client's `status` asks for `COM_STATISTICS`. Measured on MySQL
/// 8.4.11 the line reads `Uptime: 106  Threads: 2  Questions: 52  Slow
/// queries: 0  Opens: 136  Flush tables: 3  Open tables: 55  Queries per
/// second avg: 0.490`; this answers the counters it keeps, `Questions`
/// counted over every session.
#[test]
fn statistics_answer_the_counters_this_server_keeps() {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("app"));
    let (_directory, catalog, _factory) = catalog_factory(authorizer);
    let mut other = listed_session(&catalog, "reader", 8, 40001);
    let mut asking = listed_session(&catalog, "app", 9, 40002);
    for _ in 0..2 {
        other.question_asked();
    }
    for _ in 0..3 {
        asking.question_asked();
    }
    let line = asking.statistics().unwrap();
    let (uptime, rest) = line
        .strip_prefix("Uptime: ")
        .and_then(|rest| rest.split_once("  "))
        .unwrap();
    let uptime = uptime.parse::<u64>().unwrap();
    let average = if uptime == 0 { 0 } else { 5000 / uptime };
    assert_eq!(
        rest,
        format!(
            "Threads: 2  Questions: 5  Queries per second avg: {}.{:03}",
            average / 1000,
            average % 1000
        )
    );
}

/// Measured on MySQL 8.4.11: the counters a health check reads are the
/// server's in either scope, in the columns `SHOW VARIABLES` answers in read
/// from `session_status` or `global_status`, and a counter this server does
/// not keep answers no row, as a name MySQL's build leaves out does.
#[test]
fn status_answers_the_counters_this_server_keeps() {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("app"));
    let (_directory, catalog, _factory) = catalog_factory(authorizer);
    let _first = listed_session(&catalog, "app", 7, 40000);
    let _second = listed_session(&catalog, "reader", 8, 40001);
    let mut asking = listed_session(&catalog, "app", 9, 40002);
    let global = answered(&mut asking, "SHOW GLOBAL STATUS LIKE 'Threads_connected'");
    assert_eq!(
        global.rows,
        [[Some(b"Threads_connected".to_vec()), Some(b"3".to_vec())]]
    );
    assert_eq!(global.columns[0].table, "global_status");
    assert_eq!(global.columns[1].column_length, 4096);
    let session = answered(&mut asking, "SHOW STATUS");
    assert_eq!(session.columns[0].table, "session_status");
    assert_eq!(
        session
            .rows
            .iter()
            .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["Threads_connected", "Uptime", "Uptime_since_flush_status"]
    );
    let uptime = String::from_utf8(session.rows[1][1].clone().unwrap()).unwrap();
    assert!(uptime.parse::<u64>().is_ok(), "{uptime}");
    assert!(answered(&mut asking, "SHOW STATUS LIKE 'Questions'")
        .rows
        .is_empty());
    assert_eq!(
        query(&mut asking, "SHOW STATUS WHERE Variable_name = 'Uptime'"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// Laravel's `db:show` counts the connections by reading one counter out of
/// `performance_schema`, prepared. Measured on MySQL 8.4.11 over both
/// protocols: the counter is named without regard to case, and the one
/// column is a nullable `VAR_STRING` of 4096 named after its alias, whose
/// origin is `performance_schema.session_status.VARIABLE_VALUE`. A counter
/// this server does not keep is refused rather than answered with no row,
/// which MySQL answers only for a name it has not got.
#[test]
fn laravel_counts_the_connections_out_of_performance_schema() {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("app"));
    let (_directory, catalog, _factory) = catalog_factory(authorizer);
    let _first = listed_session(&catalog, "app", 7, 40000);
    let _second = listed_session(&catalog, "reader", 8, 40001);
    let mut asking = listed_session(&catalog, "app", 9, 40002);
    asking.execute_init_db("reports").unwrap();
    let laravel = "select variable_value as `Value` from performance_schema.session_status where variable_name = 'threads_connected'";
    let text = answered(&mut asking, laravel);
    assert_eq!(text.rows, [[Some(b"3".to_vec())]]);
    let [column] = text.columns.as_slice() else {
        panic!("one column");
    };
    assert_eq!(
        (
            column.name.as_str(),
            column.original_name.as_str(),
            column.table.as_str(),
            column.original_table.as_str(),
            column.schema.as_str(),
            column.column_length,
            column.flags,
            column.column_type,
        ),
        (
            "Value",
            "VARIABLE_VALUE",
            "session_status",
            "session_status",
            "performance_schema",
            4096,
            0,
            MYSQL_TYPE_VAR_STRING
        )
    );
    let statement = asking.execute_stmt_prepare(laravel).unwrap();
    assert_eq!(statement.columns, text.columns);
    let Ok(PreparedStatementExecutionResult::ResultSet(binary)) =
        asking.execute_stmt_execute(statement.statement_id, &[])
    else {
        panic!("the prepared read must answer a row");
    };
    assert_eq!(binary.rows, [[BinaryResultValue::Text("3".to_owned())]]);

    let uptime = answered(
        &mut asking,
        "SELECT VARIABLE_VALUE FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Uptime'",
    );
    assert_eq!(uptime.columns[0].name, "VARIABLE_VALUE");
    assert_eq!(uptime.columns[0].table, "global_status");
    for sql in [
        "select variable_value from performance_schema.session_status where variable_name = 'Questions'",
        "select * from performance_schema.session_status where variable_name = 'Uptime'",
    ] {
        assert_eq!(query(&mut asking, sql), Err(FrontendErrorKind::Unsupported), "{sql}");
        assert_eq!(
            asking.execute_stmt_prepare(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
}
