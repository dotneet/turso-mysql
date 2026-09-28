//! The settings a client sends when it opens a connection, and what they
//! change afterwards.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

const RAILS_OPENS_WITH: &str = "SET NAMES utf8mb4,  @@SESSION.sql_mode = CONCAT(CONCAT(@@sql_mode, ',STRICT_ALL_TABLES'), ',NO_AUTO_VALUE_ON_ZERO'),  @@SESSION.wait_timeout = 2147483";

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([81; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result,
        other => panic!("{sql}: {other:?}"),
    }
}

fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
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

/// Under `NO_AUTO_VALUE_ON_ZERO`, which Rails always turns on, a written 0 is
/// stored as 0 and a second one is a duplicate, while NULL still takes the
/// next number. Measured on MySQL 8.4.11: `(0, 1)`, `(2)`, `(NULL, 3)` and
/// `(0, 4)` leave rows 0, 1 and 2 and refuse the last with 1062, and once the
/// mode is gone a written 0 takes the next number again.
#[test]
fn a_written_zero_is_stored_under_no_auto_value_on_zero() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, RAILS_OPENS_WITH);
    run(
        &mut adapter,
        "CREATE TABLE z (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)",
    );
    run(&mut adapter, "INSERT INTO z (id, v) VALUES (0, 1)");
    let second = run(&mut adapter, "INSERT INTO z (v) VALUES (2)");
    assert_eq!(second.last_insert_id, 1);
    run(&mut adapter, "INSERT INTO z (id, v) VALUES (NULL, 3)");
    assert_eq!(
        adapter.execute_query("INSERT INTO z (id, v) VALUES (0, 4)"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, v FROM z ORDER BY id"),
        [
            [Some("0".to_owned()), Some("1".to_owned())],
            [Some("1".to_owned()), Some("2".to_owned())],
            [Some("2".to_owned()), Some("3".to_owned())],
        ]
    );

    run(&mut adapter, "SET sql_mode = DEFAULT");
    let generated = run(&mut adapter, "INSERT INTO z (id, v) VALUES (0, 5)");
    assert_eq!(generated.last_insert_id, 3);
}

/// A prepared `INSERT` binding 0 reads it the same way the written one does.
#[test]
fn a_bound_zero_is_stored_under_no_auto_value_on_zero() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, RAILS_OPENS_WITH);
    run(
        &mut adapter,
        "CREATE TABLE z (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, v INT)",
    );
    let prepared = adapter
        .execute_stmt_prepare("INSERT INTO z (id, v) VALUES (?, 1)")
        .unwrap();
    // The null bitmap, the new-parameters flag, LONGLONG and its sign byte,
    // and the eight bytes of 0.
    let payload = [0, 1, MYSQL_TYPE_LONGLONG, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    adapter
        .execute_stmt_execute(prepared.statement_id, &payload)
        .unwrap();
    assert_eq!(
        rows(&mut adapter, "SELECT id FROM z"),
        [[Some("0".to_owned())]]
    );
}

/// The idle time a session asks for is the one its connection keeps, and
/// `DEFAULT` gives the server's own back.
#[test]
fn the_idle_time_a_session_asks_for_reaches_its_connection() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(adapter.session_wait_timeout(), None);
    run(&mut adapter, RAILS_OPENS_WITH);
    assert_eq!(
        adapter.session_wait_timeout(),
        Some(Duration::from_secs(2_147_483))
    );
    run(&mut adapter, "SET @@SESSION.wait_timeout = DEFAULT");
    assert_eq!(adapter.session_wait_timeout(), None);
}

/// `TRADITIONAL` stands for modes this server behaves as — the strict ones,
/// the two zero-date ones, division by zero and no engine substitution — and
/// MySQL keeps it as a mode of its own. Measured on MySQL 8.4.11: it reads
/// back as `STRICT_TRANS_TABLES,STRICT_ALL_TABLES,NO_ZERO_IN_DATE,
/// NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,TRADITIONAL,NO_ENGINE_SUBSTITUTION`,
/// and `ANSI`, which stands for modes this server does not keep, is refused.
#[test]
fn a_session_may_ask_for_the_traditional_modes() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "SET @@session.sql_mode = 'TRADITIONAL'");
    assert_eq!(
        rows(&mut adapter, "SELECT @@sql_mode"),
        [[Some(
            "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,STRICT_ALL_TABLES,NO_ZERO_IN_DATE,\
             NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,TRADITIONAL,NO_ENGINE_SUBSTITUTION"
                .to_owned()
        )]]
    );
    run(
        &mut adapter,
        "SET sql_mode = 'traditional,ONLY_FULL_GROUP_BY'",
    );
    assert!(adapter.execute_query("SET sql_mode = 'ANSI'").is_err());
}

/// A `SELECT` running past `max_execution_time` is stopped. Measured on MySQL
/// 8.4.11: any whole number of milliseconds is taken and read back as a
/// LONGLONG of 21, `DEFAULT` is 0 — no limit — and a `SELECT` running longer
/// answers 3024 while an `UPDATE` runs on.
#[test]
fn a_select_running_past_max_execution_time_is_stopped() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE counted (n INT NOT NULL PRIMARY KEY)",
    );
    let values = (1..=300)
        .map(|n| format!("({n})"))
        .collect::<Vec<_>>()
        .join(", ");
    run(
        &mut adapter,
        &format!("INSERT INTO counted VALUES {values}"),
    );
    let slow = "SELECT COUNT(*) FROM counted a CROSS JOIN counted b CROSS JOIN counted c";

    assert_eq!(
        rows(&mut adapter, "SELECT @@max_execution_time"),
        [[Some("0".to_owned())]]
    );
    run(&mut adapter, "SET SESSION max_execution_time = 1");
    assert_eq!(
        rows(&mut adapter, "SELECT @@session.max_execution_time"),
        [[Some("1".to_owned())]]
    );
    assert_eq!(
        adapter.execute_query(slow),
        Err(FrontendErrorKind::QueryTimeout)
    );
    let prepared = adapter.execute_stmt_prepare(slow).unwrap();
    assert_eq!(
        adapter.execute_stmt_execute(prepared.statement_id, &[]),
        Err(FrontendErrorKind::QueryTimeout)
    );
    // The session is still usable, and a write is not held to the limit.
    run(&mut adapter, "UPDATE counted SET n = n WHERE n = 1");

    run(&mut adapter, "SET max_execution_time = DEFAULT");
    assert_eq!(
        rows(&mut adapter, "SELECT @@max_execution_time"),
        [[Some("0".to_owned())]]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(*) FROM counted a CROSS JOIN counted b"
        ),
        [[Some("90000".to_owned())]]
    );
    assert!(adapter
        .execute_query("SET GLOBAL max_execution_time = 5")
        .is_err());
}

/// Measured on MySQL 8.4.11: `max_allowed_packet` reads back as 64 MiB in
/// every scope and every spelling of a session assignment to it answers 1621,
/// leaving it where it was.
#[test]
fn max_allowed_packet_reads_back_64_mib_and_the_session_cannot_set_it() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SET SESSION max_allowed_packet = 1024",
        "SET max_allowed_packet = 1024",
        "SET @@max_allowed_packet = 1024",
        "SET @@session.max_allowed_packet = DEFAULT",
        "SET LOCAL max_allowed_packet = 1024",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::SessionMaxAllowedPacketIsReadOnly),
            "{sql}"
        );
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT @@max_allowed_packet, @@session.max_allowed_packet, @@global.max_allowed_packet"
        ),
        [vec![Some("67108864".to_owned()); 3]]
    );
    assert_eq!(
        rows(&mut adapter, "SHOW VARIABLES LIKE 'max_allowed_packet'"),
        [vec![
            Some("max_allowed_packet".to_owned()),
            Some("67108864".to_owned())
        ]]
    );
}

/// The `mysql` client on a shell without a UTF-8 locale names latin1 in its
/// handshake. The session then reads its statements and sends its results in
/// latin1, as after `SET NAMES latin1`, so a result is refused rather than
/// sent in utf8mb4, and the `SET NAMES utf8mb4` every driver sends makes the
/// session whole.
#[test]
fn a_latin1_handshake_refuses_results_until_set_names_utf8mb4() {
    let (_directory, mut adapter) = adapter();
    adapter.take_client_collation(8).unwrap();
    assert_eq!(
        adapter.execute_query("SELECT 1"),
        Err(FrontendErrorKind::Unsupported)
    );
    run(&mut adapter, "SET NAMES utf8mb4");
    assert_eq!(rows(&mut adapter, "SELECT 1"), [[Some("1".to_owned())]]);
    assert_eq!(
        adapter.take_client_collation(33),
        Err(FrontendErrorKind::UnsupportedClientCharacterSet)
    );
}

/// What `mysqldump` 8.4 sends before anything else. Measured on MySQL 8.4.11:
/// both timeouts read back as set, as unsigned LONGLONGs of 21; `DEFAULT`
/// gives back 30 for reading and the server's own for writing; and a value
/// outside one second to a year is clamped with warning 1292, which this
/// refuses instead. The write deadline is the one the connection keeps.
#[test]
fn the_network_timeouts_a_dump_asks_for_are_kept() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT @@net_read_timeout, @@net_write_timeout"
        ),
        [[Some("30".to_owned()), Some("60".to_owned())]]
    );
    assert_eq!(adapter.session_net_write_timeout(), None);

    run(
        &mut adapter,
        "SET SESSION NET_READ_TIMEOUT= 86400, SESSION NET_WRITE_TIMEOUT= 86400",
    );
    let Ok(CommandExecutionResult::ResultSet(read)) = adapter
        .execute_query("SELECT @@net_read_timeout, @@net_write_timeout, @@GLOBAL.net_read_timeout")
    else {
        panic!("the timeouts are answered");
    };
    assert_eq!(
        read.rows,
        [[
            Some(b"86400".to_vec()),
            Some(b"86400".to_vec()),
            Some(b"30".to_vec())
        ]]
    );
    for column in &read.columns {
        assert_eq!(column.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(column.column_length, 21);
        assert_eq!(
            column.flags,
            MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        );
    }
    assert_eq!(
        adapter.session_net_write_timeout(),
        Some(Duration::from_secs(86_400))
    );

    run(&mut adapter, "SET net_write_timeout = 600");
    assert_eq!(
        adapter.session_net_write_timeout(),
        Some(Duration::from_secs(600))
    );
    for refused in [
        "SET net_read_timeout = 0",
        "SET net_write_timeout = 31536001",
    ] {
        assert_eq!(
            adapter.execute_query(refused),
            Err(FrontendErrorKind::Unsupported),
            "{refused}"
        );
    }
    run(
        &mut adapter,
        "SET net_read_timeout = DEFAULT, net_write_timeout = DEFAULT",
    );
    assert_eq!(adapter.session_net_write_timeout(), None);
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT @@net_read_timeout, @@net_write_timeout"
        ),
        [[Some("30".to_owned()), Some("60".to_owned())]]
    );
}

/// `mysqldump` asks for the new words for replication before it lists
/// routines or events. Measured on MySQL 8.4.11: `NONE`, 0 and `DEFAULT` all
/// leave it at `NONE`; `BEFORE_8_0_26` asks for the old words, which this
/// server has no output to print in, and is refused.
#[test]
fn a_dump_may_ask_for_the_new_replication_words() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "SET @@SESSION.terminology_use_previous = NONE",
    );
    run(&mut adapter, "SET terminology_use_previous = 0");
    assert_eq!(
        rows(&mut adapter, "SELECT @@terminology_use_previous"),
        [[Some("NONE".to_owned())]]
    );
    assert_eq!(
        adapter.execute_query("SET terminology_use_previous = 'BEFORE_8_0_26'"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// Prisma's driver reads this on every connection and moves to the socket it
/// names. Measured on MySQL 8.4.11: a `VAR_STRING` of 87380 with 31 decimals
/// and no flags; a path left unset reads as NULL there — `@@init_file` — and
/// as an empty value in `SHOW VARIABLES`; and `@@SESSION.socket` is 1238.
/// A server listening on TCP here listens on no socket at all.
#[test]
fn the_socket_is_the_one_this_server_listens_on() {
    let (_directory, mut adapter) = adapter();
    let Ok(CommandExecutionResult::ResultSet(read)) =
        adapter.execute_query("SELECT @@socket, @@max_allowed_packet, @@wait_timeout")
    else {
        panic!("the socket is answered");
    };
    assert_eq!(read.rows[0][0], None);
    let socket = &read.columns[0];
    assert_eq!(socket.name, "@@socket");
    assert_eq!(socket.column_type, MYSQL_TYPE_VAR_STRING);
    assert_eq!(socket.column_length, 87_380);
    assert_eq!(socket.decimals, NOT_FIXED_DECIMALS);
    assert_eq!(socket.flags, 0);
    assert_eq!(
        rows(&mut adapter, "SHOW VARIABLES LIKE 'socket'"),
        [[Some("socket".to_owned()), Some(String::new())]]
    );
    assert_eq!(
        adapter.execute_query("SELECT @@SESSION.socket"),
        Err(FrontendErrorKind::Unsupported)
    );

    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut local = factory
        .with_connection_facts(MySqlConnectionFacts::on_unix_socket(
            b"/run/turso/mysql.sock".to_vec(),
        ))
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([81; 32]),
        ))
        .unwrap();
    local.authorize_connection().unwrap();
    assert_eq!(
        rows(&mut local, "SELECT @@socket, @@GLOBAL.socket AS s"),
        [[
            Some("/run/turso/mysql.sock".to_owned()),
            Some("/run/turso/mysql.sock".to_owned())
        ]]
    );
    assert_eq!(
        rows(&mut local, "SHOW VARIABLES LIKE 'socket'"),
        [[
            Some("socket".to_owned()),
            Some("/run/turso/mysql.sock".to_owned())
        ]]
    );
}
