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

use mysql_async::{prelude::Queryable, Conn, Error, OptsBuilder, Pool, PoolConstraints, PoolOpts};
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
        let endpoint = socket_directory.join("mysql.sock");
        let binary = required(RUNTIME_BINARY_ENV);
        let child = Command::new(binary)
            .args([
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
            ])
            .arg(fixture.service_uid.to_string())
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
            .prep("SELECT id FROM runtime_generated WHERE id = ?")
            .await
            .expect("pooled connection prepares before returning to the pool");
        let rows: Vec<(i64,)> = pooled
            .exec(&statement, (1_i64,))
            .await
            .expect("pooled connection executes the prepared statement before reset");
        assert_eq!(rows, vec![(1,)]);
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
            .prep("SELECT id FROM runtime_generated WHERE id = ?")
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
    let parent = tempfile::Builder::new()
        .prefix("runtime-e2e-")
        .tempdir_in(account_root)
        .expect("fixture account root accepts a private test directory");
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
