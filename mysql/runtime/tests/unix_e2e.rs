// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! End-to-end coverage run by the privileged Linux cross-UID fixture.

#![cfg(unix)]

use std::{
    env, fs,
    os::unix::{fs::FileTypeExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use mysql_async::{
    consts::ColumnType, prelude::Queryable, Conn, Error, OptsBuilder, Pool, PoolConstraints,
    PoolOpts, Row, Value,
};
use tempfile::TempDir;
use turso_mysql::MySqlDatabaseCatalog;

const SOCKET_ENV: &str = "TURSO_MYSQL_CROSS_UID_SOCKET";
const AUTHORITY_ENV: &str = "TURSO_MYSQL_CROSS_UID_AUTHORITY";
const SERVICE_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_SERVICE_UID";
const CLIENT_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_CLIENT_UID";
const ACCOUNT_STORE_ROOT_ENV: &str = "TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT";
const RUNTIME_BINARY_ENV: &str = "TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY";
const CHILD_WAIT: Duration = Duration::from_secs(5);
const PASSWORD: &str = "cross-uid-gate-password";
const REPORT_READER_PASSWORD: &str = "cross-uid-reports-password";

struct Fixture {
    authority_socket: PathBuf,
    authority: String,
    service_uid: u32,
    account_root: PathBuf,
}

impl Fixture {
    fn from_environment() -> Self {
        assert_running_as(CLIENT_UID_ENV);
        Self {
            authority_socket: PathBuf::from(required(SOCKET_ENV)),
            authority: required(AUTHORITY_ENV),
            service_uid: required(SERVICE_UID_ENV)
                .parse()
                .expect("fixture service UID is valid"),
            account_root: PathBuf::from(required(ACCOUNT_STORE_ROOT_ENV)),
        }
    }
}

struct RuntimeProcess {
    child: Child,
    endpoint: PathBuf,
    stopped: bool,
}

impl RuntimeProcess {
    fn start(fixture: &Fixture, data_root: &Path, socket_directory: &Path) -> Self {
        Self::start_with_max_prepared_stmt_count(fixture, data_root, socket_directory, None)
    }

    fn start_with_max_prepared_stmt_count(
        fixture: &Fixture,
        data_root: &Path,
        socket_directory: &Path,
        max_prepared_stmt_count: Option<usize>,
    ) -> Self {
        let endpoint = socket_directory.join("mysql.sock");
        let binary = required(RUNTIME_BINARY_ENV);
        let max_prepared_stmt_count = max_prepared_stmt_count.map(|value| value.to_string());
        let mut command = Command::new(binary);
        command.args([
            "--data-root",
            path_argument(data_root),
            "--account-store-root",
            path_argument(&fixture.account_root),
            "--socket-directory",
            path_argument(socket_directory),
            "--socket-name",
            "mysql.sock",
            "--authority-id",
            &fixture.authority,
            "--authority-socket",
            path_argument(&fixture.authority_socket),
            "--authority-service-uid",
        ]);
        command.arg(fixture.service_uid.to_string());
        if let Some(max_prepared_stmt_count) = max_prepared_stmt_count.as_deref() {
            command.args(["--max-prepared-stmt-count", max_prepared_stmt_count]);
        }
        let child = command
            .args([
                "--authority-rpc-timeout-ms",
                "1000",
                "--reload-interval-ms",
                "1000",
                "--max-connections",
                "8",
                "--max-admissions",
                "4",
                "--max-write-bytes",
                "8192",
                "--max-write-frames",
                "16",
                "--checkpoint-timeout-ms",
                "1000",
                "--tls-timeout-ms",
                "1000",
                "--authentication-timeout-ms",
                "1000",
                "--idle-timeout-ms",
                "1000",
                "--query-timeout-ms",
                "1000",
                "--write-timeout-ms",
                "1000",
                "--shutdown-timeout-ms",
                "1000",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("runtime executable starts");
        let mut runtime = Self {
            child,
            endpoint,
            stopped: false,
        };
        runtime.wait_for_socket();
        runtime
    }

    fn wait_for_socket(&mut self) {
        let deadline = Instant::now() + CHILD_WAIT;
        loop {
            if let Ok(metadata) = fs::symlink_metadata(&self.endpoint) {
                assert!(metadata.file_type().is_socket());
                return;
            }
            if let Some(status) = self.child.try_wait().expect("runtime child can be polled") {
                panic!("runtime exited before binding its socket: {status}");
            }
            if Instant::now() >= deadline {
                self.kill_and_wait();
                panic!("runtime did not bind its socket");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop_after_sigterm(&mut self) {
        let pid = libc::pid_t::try_from(self.child.id()).expect("child PID fits pid_t");
        // SAFETY: `pid` identifies the child process owned by this test.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);

        let deadline = Instant::now() + CHILD_WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("runtime child can be polled") {
                break status;
            }
            if Instant::now() >= deadline {
                self.kill_and_wait();
                panic!("runtime did not exit after SIGTERM");
            }
            thread::sleep(Duration::from_millis(20));
        };
        self.stopped = true;
        assert!(
            status.success(),
            "runtime did not exit successfully after SIGTERM: {status}"
        );
        let error = fs::symlink_metadata(&self.endpoint).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stopped = true;
    }
}

impl Drop for RuntimeProcess {
    fn drop(&mut self) {
        if !self.stopped {
            self.kill_and_wait();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the privileged Linux cross-UID fixture"]
async fn mysql_async_0_37_1_bootstrap_authenticates_and_serves_prepared_queries_and_pool_reset_over_a_unix_socket(
) {
    let fixture = Fixture::from_environment();
    let roots = private_roots(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(roots.data_root()).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, roots.data_root(), roots.socket_directory());
    let options = OptsBuilder::default()
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)));
    let mut connection = Conn::new(options)
        .await
        .expect("external MySQL driver authenticates over the Unix socket");

    connection
        .query_drop("USE reports")
        .await
        .expect("account can select its granted database");

    connection
        .query_drop(
            "CREATE TABLE runtime_integer_widths (tiny TINYINT, small SMALLINT, int_value INT, integer_value INTEGER, big BIGINT)",
        )
        .await
        .expect("ordinary query creates the integer width test table");
    let integer_statement = connection
        .prep("SELECT tiny, small, int_value, integer_value, big FROM runtime_integer_widths")
        .await
        .expect("external driver prepares the integer width query");
    let expected_integer_metadata = vec![
        (ColumnType::MYSQL_TYPE_TINY, 4),
        (ColumnType::MYSQL_TYPE_SHORT, 6),
        (ColumnType::MYSQL_TYPE_LONG, 11),
        (ColumnType::MYSQL_TYPE_LONG, 11),
        (ColumnType::MYSQL_TYPE_LONGLONG, 20),
    ];
    assert_eq!(
        integer_column_metadata(&integer_statement.columns()),
        expected_integer_metadata
    );

    let mut empty_result = connection
        .exec_iter(&integer_statement, ())
        .await
        .expect("external driver executes the empty integer width query");
    assert_eq!(
        integer_column_metadata(empty_result.columns_ref()),
        expected_integer_metadata
    );
    let empty_rows: Vec<Row> = empty_result
        .collect()
        .await
        .expect("external driver collects the empty integer width result");
    assert!(empty_rows.is_empty());

    for values in [
        "(-128, -32768, -2147483648, -2147483648, -9223372036854775808)",
        "(127, 32767, 2147483647, 2147483647, 9223372036854775807)",
        "(NULL, NULL, NULL, NULL, NULL)",
    ] {
        connection
            .query_drop(format!(
                "INSERT INTO runtime_integer_widths (tiny, small, int_value, integer_value, big) VALUES {values}"
            ))
            .await
            .unwrap_or_else(|error| panic!("integer extrema insert {values} failed: {error}"));
    }

    let mut integer_result = connection
        .exec_iter(&integer_statement, ())
        .await
        .expect("external driver executes the populated integer width query");
    assert_eq!(
        integer_column_metadata(integer_result.columns_ref()),
        expected_integer_metadata
    );
    let integer_rows: Vec<Row> = integer_result
        .collect()
        .await
        .expect("external driver collects the integer extrema result");
    let integer_values = integer_rows
        .into_iter()
        .map(Row::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(
        integer_values,
        vec![
            vec![
                Value::Int(-128),
                Value::Int(-32768),
                Value::Int(-2147483648),
                Value::Int(-2147483648),
                Value::Int(i64::MIN),
            ],
            vec![
                Value::Int(127),
                Value::Int(32767),
                Value::Int(2147483647),
                Value::Int(2147483647),
                Value::Int(i64::MAX),
            ],
            vec![
                Value::NULL,
                Value::NULL,
                Value::NULL,
                Value::NULL,
                Value::NULL
            ],
        ]
    );

    let mut text_result = connection
        .query_iter("SELECT tiny, small, int_value, integer_value, big FROM runtime_integer_widths")
        .await
        .expect("external driver executes the text integer width query");
    assert_eq!(
        integer_column_metadata(text_result.columns_ref()),
        expected_integer_metadata
    );
    let text_rows: Vec<Row> = text_result
        .collect()
        .await
        .expect("external driver collects the text integer extrema result");
    let text_values = text_rows.into_iter().map(Row::unwrap).collect::<Vec<_>>();
    assert_eq!(
        text_values,
        vec![
            vec![
                Value::Bytes(b"-128".to_vec()),
                Value::Bytes(b"-32768".to_vec()),
                Value::Bytes(b"-2147483648".to_vec()),
                Value::Bytes(b"-2147483648".to_vec()),
                Value::Bytes(b"-9223372036854775808".to_vec()),
            ],
            vec![
                Value::Bytes(b"127".to_vec()),
                Value::Bytes(b"32767".to_vec()),
                Value::Bytes(b"2147483647".to_vec()),
                Value::Bytes(b"2147483647".to_vec()),
                Value::Bytes(b"9223372036854775807".to_vec()),
            ],
            vec![
                Value::NULL,
                Value::NULL,
                Value::NULL,
                Value::NULL,
                Value::NULL,
            ],
        ]
    );

    connection
        .query_drop("CREATE TABLE runtime_entries (id INT, label TEXT)")
        .await
        .expect("ordinary query creates the test table");
    connection
        .query_drop("INSERT INTO runtime_entries (id, label) VALUES (1, 'ordinary')")
        .await
        .expect("ordinary query inserts a row");

    let statement = connection
        .prep("INSERT INTO runtime_entries (id, label) VALUES (?, ?)")
        .await
        .expect("external driver prepares a statement");
    connection
        .exec_drop(&statement, (2_i64, "prepared"))
        .await
        .expect("external driver executes a prepared statement");
    let mut rows: Vec<(i64, String)> = connection
        .query("SELECT id, label FROM runtime_entries")
        .await
        .expect("external driver reads prepared and ordinary rows");
    rows.sort_by_key(|row| row.0);
    assert_eq!(
        rows,
        vec![(1, "ordinary".to_owned()), (2, "prepared".to_owned())]
    );

    let isolated_options = OptsBuilder::default()
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)));
    let mut isolated_connection = Conn::new(isolated_options)
        .await
        .expect("second external MySQL driver connection authenticates");
    let no_database_error = isolated_connection
        .query::<(i64, String), _>("SELECT id, label FROM runtime_entries")
        .await
        .expect_err(
            "a second connection must not inherit the first connection's selected database",
        );
    assert!(
        matches!(
            no_database_error,
            mysql_async::Error::Server(error) if error.code == 1046 && error.state == "3D000"
        ),
        "a connection without a selected database must receive MySQL error 1046/3D000"
    );
    isolated_connection
        .query_drop("USE reports")
        .await
        .expect("second connection can select its database independently");
    let isolated_rows: Vec<(i64, String)> = isolated_connection
        .query("SELECT id, label FROM runtime_entries")
        .await
        .expect("second connection reads the shared database after selecting it");
    assert_eq!(isolated_rows.len(), 2);
    isolated_connection
        .disconnect()
        .await
        .expect("second external MySQL driver connection closes cleanly");

    let first_connection_rows: Vec<(i64, String)> = connection
        .query("SELECT id, label FROM runtime_entries")
        .await
        .expect("first connection retains its own selected database");
    assert_eq!(first_connection_rows.len(), 2);
    connection
        .disconnect()
        .await
        .expect("external driver closes cleanly");

    let reconnect_options = OptsBuilder::default()
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)));
    let mut reconnected = Conn::new(reconnect_options)
        .await
        .expect("external MySQL driver reconnects over the Unix socket");
    reconnected
        .query_drop("USE reports")
        .await
        .expect("reconnected driver can select the database");
    let reconnected_rows: Vec<(i64, String)> = reconnected
        .query("SELECT id, label FROM runtime_entries")
        .await
        .expect("reconnected driver reads the previously committed rows");
    assert_eq!(reconnected_rows.len(), 2);
    reconnected
        .disconnect()
        .await
        .expect("reconnected external MySQL driver closes cleanly");

    let pool_options = OptsBuilder::default()
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)))
        .pool_opts(PoolOpts::default().with_constraints(
            PoolConstraints::new(1, 1).expect("pool constraints have a valid range"),
        ));
    let pool = Pool::new(pool_options);
    let old_statement = {
        let mut pooled = pool
            .get_conn()
            .await
            .expect("pool obtains a Unix MySQL connection");
        pooled
            .query_drop("USE reports")
            .await
            .expect("pooled connection selects the test database");
        pooled
            .query_drop(
                "CREATE TABLE runtime_generated (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
            )
            .await
            .expect("pooled connection creates an auto-increment table");
        pooled
            .query_drop("SET SESSION autocommit = 0")
            .await
            .expect("pooled connection disables autocommit");
        pooled
            .query_drop("INSERT INTO runtime_generated (label) VALUES ('rolled back')")
            .await
            .expect("pooled connection writes an uncommitted generated row");
        let generated_id: Option<i64> = pooled
            .query_first("SELECT LAST_INSERT_ID()")
            .await
            .expect("pooled connection reads its generated ID");
        assert_eq!(generated_id, Some(1));

        let statement = pooled
            .prep("SELECT id FROM runtime_generated WHERE ? IS NOT NULL")
            .await
            .expect("pooled connection prepares before returning to the pool");
        let rows: Vec<(i64,)> = pooled
            .exec(&statement, (1_i64,))
            .await
            .expect("pooled connection executes the prepared statement before reset");
        assert_eq!(rows, vec![(1,)]);
        let null_rows: Vec<(i64,)> = pooled
            .exec(&statement, (Option::<i64>::None,))
            .await
            .expect("pooled statement binds NULL before reset");
        assert!(null_rows.is_empty());
        statement
    };

    {
        let mut pooled = pool
            .get_conn()
            .await
            .expect("pool resets and reuses the Unix MySQL connection");
        let rows: Vec<(i64, String)> = pooled
            .query("SELECT id, label FROM runtime_generated")
            .await
            .expect("selected database remains available after pool reset");
        assert!(rows.is_empty(), "pool reset must rollback the pending row");
        let last_insert_id: Option<i64> = pooled
            .query_first("SELECT LAST_INSERT_ID()")
            .await
            .expect("pool reset clears LAST_INSERT_ID");
        assert_eq!(last_insert_id, Some(0));

        let old_rows: Result<Vec<(i64,)>, _> = pooled.exec(&old_statement, (1_i64,)).await;
        assert!(
            matches!(old_rows, Err(Error::Server(error)) if error.code == 1243),
            "pool reset must invalidate prepared statements with ER_UNKNOWN_STMT (1243)"
        );

        let new_statement = pooled
            .prep("SELECT id FROM runtime_generated WHERE ? IS NOT NULL")
            .await
            .expect("pooled connection prepares after a reset");
        let rows: Vec<(i64,)> = pooled
            .exec(&new_statement, (1_i64,))
            .await
            .expect("pooled connection executes a new prepared statement");
        assert!(rows.is_empty());
        pooled
            .query_drop("INSERT INTO runtime_generated (label) VALUES ('committed')")
            .await
            .expect("restored autocommit commits the next write");
    }

    {
        let mut pooled = pool
            .get_conn()
            .await
            .expect("pool returns the reset connection again");
        let rows: Vec<(i64, String)> = pooled
            .query("SELECT id, label FROM runtime_generated")
            .await
            .expect("pooled connection reads the committed row");
        assert_eq!(rows, vec![(2, "committed".to_owned())]);
        let last_insert_id: Option<i64> = pooled
            .query_first("SELECT LAST_INSERT_ID()")
            .await
            .expect("pool reset clears the generated ID from the prior checkout");
        assert_eq!(last_insert_id, Some(0));
    }
    pool.disconnect()
        .await
        .expect("pool disconnects cleanly after Unix reset coverage");

    runtime.stop_after_sigterm();
}

fn integer_column_metadata(columns: &[mysql_async::Column]) -> Vec<(ColumnType, u32)> {
    columns
        .iter()
        .map(|column| (column.column_type(), column.column_length()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the privileged Linux cross-UID fixture"]
async fn mysql_async_0_37_1_mediumint_result_metadata_and_boundaries_over_a_unix_socket() {
    let fixture = Fixture::from_environment();
    let roots = private_roots(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(roots.data_root()).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, roots.data_root(), roots.socket_directory());
    let options = OptsBuilder::default()
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)));
    let mut connection = Conn::new(options)
        .await
        .expect("external MySQL driver authenticates over the Unix socket");
    connection
        .query_drop("USE reports")
        .await
        .expect("account can select its granted database");
    connection
        .query_drop("CREATE TABLE runtime_mediumint (value MEDIUMINT)")
        .await
        .expect("ordinary query creates the MEDIUMINT test table");

    let statement = connection
        .prep("SELECT value FROM runtime_mediumint")
        .await
        .expect("external driver prepares the MEDIUMINT query");
    let expected_metadata = vec![(ColumnType::MYSQL_TYPE_INT24, 9)];
    assert_eq!(
        integer_column_metadata(&statement.columns()),
        expected_metadata
    );

    let mut empty_result = connection
        .exec_iter(&statement, ())
        .await
        .expect("external driver executes the empty MEDIUMINT query");
    assert_eq!(
        integer_column_metadata(empty_result.columns_ref()),
        expected_metadata
    );
    let empty_rows: Vec<Row> = empty_result
        .collect()
        .await
        .expect("external driver collects the empty MEDIUMINT result");
    assert!(empty_rows.is_empty());

    for value in ["-8388608", "8388607", "NULL"] {
        connection
            .query_drop(format!(
                "INSERT INTO runtime_mediumint (value) VALUES ({value})"
            ))
            .await
            .expect("ordinary query inserts a MEDIUMINT boundary row");
    }

    let mut prepared_result = connection
        .exec_iter(&statement, ())
        .await
        .expect("external driver executes the populated MEDIUMINT query");
    assert_eq!(
        integer_column_metadata(prepared_result.columns_ref()),
        expected_metadata
    );
    let prepared_rows: Vec<Row> = prepared_result
        .collect()
        .await
        .expect("external driver collects the prepared MEDIUMINT result");
    let expected_values = vec![
        vec![Value::Int(-8_388_608)],
        vec![Value::Int(8_388_607)],
        vec![Value::NULL],
    ];
    let prepared_values = prepared_rows
        .into_iter()
        .map(Row::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(prepared_values, expected_values);

    let mut text_result = connection
        .query_iter("SELECT value FROM runtime_mediumint")
        .await
        .expect("external driver executes the text MEDIUMINT query");
    assert_eq!(
        integer_column_metadata(text_result.columns_ref()),
        expected_metadata
    );
    let text_rows: Vec<Row> = text_result
        .collect()
        .await
        .expect("external driver collects the text MEDIUMINT result");
    let text_values = text_rows.into_iter().map(Row::unwrap).collect::<Vec<_>>();
    assert_eq!(
        text_values,
        vec![
            vec![Value::Bytes(b"-8388608".to_vec())],
            vec![Value::Bytes(b"8388607".to_vec())],
            vec![Value::NULL],
        ]
    );

    connection
        .disconnect()
        .await
        .expect("external driver closes cleanly");
    runtime.stop_after_sigterm();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the privileged Linux cross-UID fixture"]
async fn mysql_async_0_37_1_prepared_statement_quota_and_reset_over_a_unix_socket() {
    let fixture = Fixture::from_environment();
    let roots = private_roots(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(roots.data_root()).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start_with_max_prepared_stmt_count(
        &fixture,
        roots.data_root(),
        roots.socket_directory(),
        Some(1),
    );
    let options = |username| {
        OptsBuilder::default()
            .user(Some(username))
            .pass(Some(PASSWORD))
            .socket(Some(path_argument(&runtime.endpoint)))
    };
    let mut first = Conn::new(options("gateadmin"))
        .await
        .expect("first external MySQL connection authenticates over the Unix socket");
    first
        .query_drop("USE reports")
        .await
        .expect("first connection selects its database");
    first
        .query_drop("CREATE TABLE runtime_prepared_quota (id INT)")
        .await
        .expect("first connection creates the quota test table");
    first
        .query_drop("INSERT INTO runtime_prepared_quota (id) VALUES (7)")
        .await
        .expect("first connection inserts the quota test row");
    let first_statement = first
        .prep("SELECT id FROM runtime_prepared_quota WHERE ? IS NOT NULL")
        .await
        .expect("the first prepared statement consumes the configured quota");

    let mut second = Conn::new(options("gateadmin"))
        .await
        .expect("second external MySQL connection authenticates over the Unix socket");
    second
        .query_drop("USE reports")
        .await
        .expect("second connection selects its database");
    let quota_error = match second
        .prep("SELECT id FROM runtime_prepared_quota WHERE ? IS NOT NULL")
        .await
    {
        Ok(_) => panic!("the second prepared statement must exceed the configured quota"),
        Err(error) => error,
    };
    assert!(
        matches!(
            quota_error,
            Error::Server(error) if error.code == 1461 && error.state == "42000"
        ),
        "quota exhaustion must return MySQL error 1461/42000"
    );

    first
        .close(first_statement)
        .await
        .expect("COM_STMT_CLOSE releases the prepared statement quota");
    first
        .ping()
        .await
        .expect("a same-connection round trip completes COM_STMT_CLOSE processing");
    let second_statement = second
        .prep("SELECT id FROM runtime_prepared_quota WHERE ? IS NOT NULL")
        .await
        .expect("the released quota accepts another prepared statement");

    assert!(second.reset().await.expect("COM_RESET_CONNECTION succeeds"));
    let retained_rows: Vec<(i64,)> = second
        .query("SELECT id FROM runtime_prepared_quota")
        .await
        .expect("COM_RESET_CONNECTION retains the selected database");
    assert_eq!(retained_rows, vec![(7,)]);
    drop(second_statement);

    let first_after_reset = first
        .prep("SELECT id FROM runtime_prepared_quota WHERE ? IS NOT NULL")
        .await
        .expect("COM_RESET_CONNECTION releases the prepared statement quota");
    first
        .close(first_after_reset)
        .await
        .expect("the final prepared statement closes cleanly");

    first
        .disconnect()
        .await
        .expect("first external MySQL connection closes cleanly");
    second
        .disconnect()
        .await
        .expect("second external MySQL connection closes cleanly");
    runtime.stop_after_sigterm();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the privileged Linux cross-UID fixture"]
async fn mysql_async_0_37_1_table_grants_authorize_records_and_deny_other_over_a_unix_socket() {
    let fixture = Fixture::from_environment();
    let roots = private_roots(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(roots.data_root()).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, roots.data_root(), roots.socket_directory());
    let admin_options = OptsBuilder::default()
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)));
    let mut admin = Conn::new(admin_options)
        .await
        .expect("database-wide account authenticates over the Unix socket");
    admin
        .query_drop("USE reports")
        .await
        .expect("database-wide account can select reports");
    admin
        .query_drop("CREATE TABLE records (id INT, label TEXT)")
        .await
        .expect("database-wide account creates the granted table");
    admin
        .query_drop("CREATE TABLE other (id INT, label TEXT)")
        .await
        .expect("database-wide account creates the other table");
    admin
        .query_drop("INSERT INTO records (id, label) VALUES (7, 'kept')")
        .await
        .expect("database-wide account inserts the granted row");
    admin
        .query_drop("INSERT INTO other (id, label) VALUES (8, 'other')")
        .await
        .expect("database-wide account inserts the other row");
    let admin_rows: Vec<(i64, String)> = admin
        .query("SELECT id, label FROM `OTHER`")
        .await
        .expect("database Query permission allows reading another table");
    assert_eq!(admin_rows, vec![(8, "other".to_owned())]);
    admin
        .disconnect()
        .await
        .expect("database-wide account closes cleanly");

    let reader_options = OptsBuilder::default()
        .user(Some("reportreader"))
        .pass(Some(REPORT_READER_PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)));
    let mut reader = Conn::new(reader_options)
        .await
        .expect("table-grant account authenticates over the Unix socket");
    reader
        .query_drop("USE REPORTS")
        .await
        .expect("table-grant account can select its granted database");
    let records: Vec<(i64, String)> = reader
        .query("SELECT id, label FROM `RECORDS`")
        .await
        .expect("canonical table SELECT is allowed by the table grant");
    assert_eq!(records, vec![(7, "kept".to_owned())]);

    let other_error = reader
        .query::<(i64, String), _>("SELECT id, label FROM `OTHER`")
        .await
        .expect_err("a table grant must not allow another table");
    assert!(
        matches!(
            other_error,
            Error::Server(error) if error.code == 1045 && error.state == "28000"
        ),
        "a denied table query must receive MySQL error 1045/28000"
    );
    reader
        .disconnect()
        .await
        .expect("table-grant account closes cleanly");

    runtime.stop_after_sigterm();
}

struct PrivateRoots {
    _parent: TempDir,
    data_root: PathBuf,
    socket_directory: PathBuf,
}

impl PrivateRoots {
    fn data_root(&self) -> &Path {
        &self.data_root
    }

    fn socket_directory(&self) -> &Path {
        &self.socket_directory
    }
}

fn private_roots(account_root: &Path) -> PrivateRoots {
    let runtime_root = PathBuf::from(required("TURSO_MYSQL_RUNTIME_TEST_ROOT"));
    assert!(!runtime_root.starts_with(account_root));
    let parent = tempfile::Builder::new()
        .prefix("runtime-e2e-")
        .tempdir_in(runtime_root)
        .expect("fixture runtime root accepts a private test directory");
    set_private_mode(parent.path());
    let data_root = private_child(parent.path(), "data");
    let socket_directory = private_child(parent.path(), "socket");
    PrivateRoots {
        _parent: parent,
        data_root,
        socket_directory,
    }
}

fn private_child(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir(&path).expect("fixture private child is created");
    set_private_mode(&path);
    path
}

fn set_private_mode(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .expect("fixture directory becomes private");
}

fn path_argument(path: &Path) -> &str {
    path.to_str().expect("fixture paths are UTF-8")
}

fn assert_running_as(uid_environment: &str) {
    let expected = required(uid_environment)
        .parse::<u32>()
        .expect("fixture UID is valid");
    // SAFETY: geteuid has no arguments and only reads process credentials.
    assert_eq!(unsafe { libc::geteuid() }, expected);
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("fixture environment {name} is missing"))
}
