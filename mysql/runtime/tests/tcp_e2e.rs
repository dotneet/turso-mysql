// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! External-driver coverage for the mandatory-TLS TCP runtime boundary.

#![cfg(unix)]

use std::{
    env, fs,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use mysql_async::{prelude::Queryable, Conn, OptsBuilder, Row, SslOpts};
use tempfile::TempDir;
use turso_mysql::MySqlDatabaseCatalog;
use turso_mysql_server::{
    ClientHandshakeResponseConfig, PacketCodec, CACHING_SHA2_PASSWORD_PLUGIN,
    CLIENT_HANDSHAKE_SEQUENCE_ID, DEFAULT_UTF8MB4_COLLATION, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
    REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
};

const AUTHORITY_ENV: &str = "TURSO_MYSQL_CROSS_UID_AUTHORITY";
const SERVICE_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_SERVICE_UID";
const CLIENT_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_CLIENT_UID";
const AUTHORITY_SOCKET_ENV: &str = "TURSO_MYSQL_CROSS_UID_SOCKET";
const ACCOUNT_STORE_ROOT_ENV: &str = "TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT";
const RUNTIME_BINARY_ENV: &str = "TURSO_MYSQL_CROSS_UID_RUNTIME_BINARY";
const CHILD_WAIT: Duration = Duration::from_secs(5);
const PASSWORD: &str = "cross-uid-gate-password";

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
            authority_socket: PathBuf::from(required(AUTHORITY_SOCKET_ENV)),
            authority: required(AUTHORITY_ENV),
            service_uid: required(SERVICE_UID_ENV)
                .parse()
                .expect("fixture service UID is valid"),
            account_root: PathBuf::from(required(ACCOUNT_STORE_ROOT_ENV)),
        }
    }
}

struct TestRoots {
    _parent: TempDir,
    data_root: PathBuf,
    ca: PathBuf,
    server_chain: PathBuf,
    private_key: PathBuf,
}

impl TestRoots {
    fn new(account_root: &Path) -> Self {
        let runtime_root = PathBuf::from(required("TURSO_MYSQL_RUNTIME_TEST_ROOT"));
        assert!(!runtime_root.starts_with(account_root));
        let parent = tempfile::Builder::new()
            .prefix("runtime-tcp-")
            .tempdir_in(runtime_root)
            .expect("fixture runtime root accepts a private TCP directory");
        set_private_mode(parent.path());

        let data_root = private_child(parent.path(), "data");
        let tls_root = private_child(parent.path(), "tls");
        let ca = tls_root.join("ca.pem");
        let server_chain = tls_root.join("server-chain.pem");
        let private_key = tls_root.join("server-key.pem");
        fs::write(&ca, include_bytes!("fixtures/ca.pem")).expect("CA fixture is written");
        fs::write(&server_chain, include_bytes!("fixtures/server-chain.pem"))
            .expect("server certificate fixture is written");
        fs::write(&private_key, include_bytes!("fixtures/server-key.pem"))
            .expect("server key fixture is written");
        fs::set_permissions(&ca, fs::Permissions::from_mode(0o644))
            .expect("CA fixture permissions");
        fs::set_permissions(&server_chain, fs::Permissions::from_mode(0o644))
            .expect("server certificate permissions");
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600))
            .expect("server key permissions");

        let roots = Self {
            _parent: parent,
            data_root,
            ca,
            server_chain,
            private_key,
        };
        roots.assert_tls_permissions();
        roots
    }

    fn assert_tls_permissions(&self) {
        assert_material(&self.ca, 0o644);
        assert_material(&self.server_chain, 0o644);
        assert_material(&self.private_key, 0o600);
    }
}

struct RuntimeProcess {
    child: Child,
    endpoint: SocketAddr,
    stopped: bool,
}

impl RuntimeProcess {
    fn start(fixture: &Fixture, roots: &TestRoots) -> Self {
        let endpoint = reserve_local_endpoint();
        let child = Command::new(required(RUNTIME_BINARY_ENV))
            .args(["--data-root"])
            .arg(&roots.data_root)
            .args(["--account-store-root"])
            .arg(&fixture.account_root)
            .args(["--listen"])
            .arg(endpoint.to_string())
            .args(["--tls-cert"])
            .arg(&roots.server_chain)
            .args(["--tls-key"])
            .arg(&roots.private_key)
            .args(["--authority-id", &fixture.authority, "--authority-socket"])
            .arg(&fixture.authority_socket)
            .args(["--authority-service-uid"])
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
                "64",
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
            .expect("TCP runtime executable starts");
        let mut runtime = Self {
            child,
            endpoint,
            stopped: false,
        };
        runtime.wait_for_endpoint();
        runtime
    }

    fn wait_for_endpoint(&mut self) {
        let deadline = Instant::now() + CHILD_WAIT;
        loop {
            if TcpStream::connect_timeout(&self.endpoint, Duration::from_millis(100)).is_ok() {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("runtime child can be polled") {
                panic!("runtime exited before binding TCP endpoint: {status}");
            }
            if Instant::now() >= deadline {
                self.kill_and_wait();
                panic!("runtime did not bind TCP endpoint");
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
        wait_for_port_release(self.endpoint);
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
async fn mysql_async_0_37_1_over_tls_tcp_validates_localhost_and_releases_port() {
    let fixture = Fixture::from_environment();
    let roots = TestRoots::new(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(&roots.data_root).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, &roots);
    assert_plaintext_rejected(runtime.endpoint);

    let valid_ssl = SslOpts::default()
        .with_root_certs(vec![roots.ca.clone().into()])
        .with_disable_built_in_roots(true);
    assert!(!valid_ssl.accept_invalid_certs());
    assert!(!valid_ssl.skip_domain_validation());
    let mut connection = Conn::new(tcp_options(runtime.endpoint, valid_ssl))
        .await
        .expect("mysql_async validates the localhost certificate and authenticates");
    connection
        .query_drop("USE reports")
        .await
        .expect("TLS connection can select its granted database");
    let value: Option<i64> = connection
        .query_first("SELECT 1")
        .await
        .expect("TLS connection executes a query");
    assert_eq!(value, Some(1));
    connection
        .disconnect()
        .await
        .expect("TLS connection closes cleanly");

    let wrong_hostname_ssl = SslOpts::default()
        .with_root_certs(vec![roots.ca.clone().into()])
        .with_disable_built_in_roots(true);
    assert!(!wrong_hostname_ssl.accept_invalid_certs());
    assert!(!wrong_hostname_ssl.skip_domain_validation());
    assert!(
        tokio::time::timeout(
            CHILD_WAIT,
            Conn::new(tcp_options_for_host(
                runtime.endpoint,
                "wrong-hostname",
                wrong_hostname_ssl,
            )),
        )
        .await
        .expect("wrong-hostname TLS connection does not hang")
        .is_err(),
        "mysql_async must reject a trusted certificate for the wrong hostname"
    );

    let untrusted_ssl = SslOpts::default().with_disable_built_in_roots(true);
    assert!(
        tokio::time::timeout(
            CHILD_WAIT,
            Conn::new(tcp_options(runtime.endpoint, untrusted_ssl)),
        )
        .await
        .expect("untrusted TLS connection does not hang")
        .is_err(),
        "mysql_async must reject the server without its configured CA"
    );

    runtime.stop_after_sigterm();
}

#[test]
#[ignore = "requires the privileged Linux cross-UID fixture with MySQL 8.0.46 CLI"]
fn mysql_cli_8_0_46_over_tls_tcp_exercises_schema_data_transactions_and_reconnect() {
    let fixture = Fixture::from_environment();
    let roots = TestRoots::new(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(&roots.data_root).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, &roots);
    let first = run_mysql_cli(
        runtime.endpoint,
        &roots.ca,
        "CREATE TABLE cli_records (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) NOT NULL);\n\
         INSERT INTO cli_records (id, name) VALUES (1, 'alpha');\n\
         SELECT id, name FROM cli_records ORDER BY id;\n\
         START TRANSACTION;\n\
         UPDATE cli_records SET name = 'beta' WHERE id = 1;\n\
         SELECT id, name FROM cli_records ORDER BY id;\n\
         ROLLBACK;\n\
         SELECT id, name FROM cli_records ORDER BY id;\n\
         SHOW COLUMNS FROM cli_records;\n",
    );
    let lines: Vec<_> = first.lines().collect();
    assert_eq!(lines.len(), 5, "unexpected MySQL CLI output: {first}");
    assert_eq!(&lines[..3], ["1\talpha", "1\tbeta", "1\talpha"]);
    assert!(lines
        .iter()
        .any(|line| line.starts_with("id\tint\tNO\tPRI\t")));
    assert!(lines
        .iter()
        .any(|line| line.starts_with("name\tvarchar(20)\tNO\t")));

    let second = run_mysql_cli(
        runtime.endpoint,
        &roots.ca,
        "SELECT id, name FROM cli_records ORDER BY id;\nDROP TABLE cli_records;\n",
    );
    assert_eq!(second.trim(), "1\talpha");

    runtime.stop_after_sigterm();
}

#[test]
#[ignore = "requires the privileged Linux cross-UID fixture with go-sql-driver/mysql 1.9.3"]
fn go_sql_driver_1_9_3_over_tls_tcp_exercises_prepared_crud_and_migration() {
    let fixture = Fixture::from_environment();
    let roots = TestRoots::new(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(&roots.data_root).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, &roots);
    run_external_driver(
        &["/usr/local/bin/mysql-go-driver-e2e"],
        runtime.endpoint,
        &roots.ca,
    );
    runtime.stop_after_sigterm();
}

#[test]
#[ignore = "requires the privileged Linux cross-UID fixture with Connector/J 9.6.0"]
fn connector_j_9_6_0_over_tls_tcp_exercises_prepared_crud_and_migration() {
    let fixture = Fixture::from_environment();
    let roots = TestRoots::new(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(&roots.data_root).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, &roots);
    run_external_driver(
        &[
            "java",
            "-cp",
            "/opt/mysql-drivers:/opt/mysql-drivers/mysql-connector-j-9.6.0.jar",
            "JdbcDriver",
        ],
        runtime.endpoint,
        &roots.ca,
    );
    runtime.stop_after_sigterm();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the privileged Linux cross-UID fixture"]
async fn mysql_value_regressions_over_tls_tcp() {
    let fixture = Fixture::from_environment();
    let roots = TestRoots::new(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(&roots.data_root).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, &roots);
    let ssl = SslOpts::default()
        .with_root_certs(vec![roots.ca.clone().into()])
        .with_disable_built_in_roots(true);
    let mut connection = Conn::new(tcp_options(runtime.endpoint, ssl))
        .await
        .expect("connect over verified TLS");
    connection
        .query_drop("USE reports")
        .await
        .expect("select reports");

    connection
        .query_drop("CREATE TABLE values_test (id INT NOT NULL PRIMARY KEY, amount FLOAT, rights SET('read','write','exec'))")
        .await
        .expect("create value test table");
    connection
        .query_drop("INSERT INTO values_test VALUES (1,0.1,'read,write'),(2,0.1,'read'),(3,0.1,'exec'),(4,NULL,'write'),(5,NULL,''),(6,NULL,NULL)")
        .await
        .expect("insert numeric and set values");
    let amount: Option<f32> = connection
        .query_first("SELECT amount FROM values_test WHERE id = 1")
        .await
        .expect("read rounded float");
    assert_eq!(amount, Some(0.1_f32));
    let rows: Vec<Row> = connection
        .query("SELECT id FROM values_test ORDER BY rights")
        .await
        .expect("sort set by declared member bits");
    assert_eq!(
        rows.iter()
            .map(|row| row.get::<u64, _>(0).expect("id"))
            .collect::<Vec<_>>(),
        vec![6, 5, 2, 4, 1, 3]
    );

    connection
        .query_drop("CREATE TABLE replace_test (id INT NOT NULL PRIMARY KEY, value INT)")
        .await
        .expect("create replace table");
    connection
        .query_drop("INSERT INTO replace_test VALUES (1,10)")
        .await
        .expect("insert old row");
    connection
        .query_drop("REPLACE INTO replace_test VALUES (1,20)")
        .await
        .expect("replace old row");
    assert_eq!(connection.affected_rows(), 2);

    connection
        .query_drop("CREATE TABLE moment_test (stamp TIMESTAMP NULL)")
        .await
        .expect("create timestamp table");
    connection
        .query_drop("INSERT INTO moment_test VALUES ('2038-01-19 03:14:07')")
        .await
        .expect("largest accepted timestamp");
    assert!(connection
        .query_drop("INSERT INTO moment_test VALUES ('2038-01-19 03:14:08')")
        .await
        .is_err());

    connection
        .query_drop(
            "CREATE TABLE decimal_test (id INT NOT NULL PRIMARY KEY, amount DECIMAL(65,30))",
        )
        .await
        .expect("create exact decimal table");
    connection
        .query_drop(
            "INSERT INTO decimal_test (id, amount) VALUES (1,'1.234567890123456789012345678901')",
        )
        .await
        .expect("insert exact decimal");
    let exact: Option<String> = connection
        .query_first("SELECT amount FROM decimal_test WHERE id = 1")
        .await
        .expect("read decimal over text protocol");
    assert_eq!(exact.as_deref(), Some("1.234567890123456789012345678901"));
    let prepared_exact: Option<String> = connection
        .exec_first("SELECT amount FROM decimal_test WHERE id = ?", (1,))
        .await
        .expect("read decimal over prepared binary protocol");
    assert_eq!(prepared_exact, exact);

    connection.disconnect().await.expect("disconnect");
    runtime.stop_after_sigterm();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the privileged Linux cross-UID fixture"]
async fn sql_account_administration_persists_and_reauthorizes_over_tls_tcp() {
    let fixture = Fixture::from_environment();
    let roots = TestRoots::new(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(&roots.data_root).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, &roots);
    let ssl = SslOpts::default()
        .with_root_certs(vec![roots.ca.clone().into()])
        .with_disable_built_in_roots(true);
    let mut admin = Conn::new(tcp_options(runtime.endpoint, ssl.clone()))
        .await
        .expect("admin connects");
    admin
        .query_drop("USE reports")
        .await
        .expect("select reports");
    admin
        .query_drop(
            "CREATE TABLE admin_records (id INT NOT NULL PRIMARY KEY, name VARCHAR(20) NOT NULL)",
        )
        .await
        .expect("create table");
    admin
        .query_drop("INSERT INTO admin_records (id, name) VALUES (1, 'alpha')")
        .await
        .expect("insert row");
    assert!(admin
        .query_drop("GRANT SELECT ON reports.admin_records TO 'unknown'@'%'")
        .await
        .is_err());
    admin
        .query_drop("CREATE USER 'sqlreader'@'%' IDENTIFIED BY 'sql-secret'")
        .await
        .expect("create account through durable checkpoint");
    admin
        .query_drop("GRANT SELECT ON reports.admin_records TO 'sqlreader'@'%'")
        .await
        .expect("grant table select");

    let reader_options = OptsBuilder::default()
        .ip_or_hostname("localhost")
        .resolved_ips(Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]))
        .tcp_port(runtime.endpoint.port())
        .user(Some("sqlreader"))
        .pass(Some("sql-secret"))
        .prefer_socket(false)
        .ssl_opts(ssl);
    let mut reader = Conn::new(reader_options)
        .await
        .expect("new account authenticates");
    reader
        .query_drop("USE reports")
        .await
        .expect("granted database");
    let name: Option<String> = reader
        .query_first("SELECT name FROM admin_records WHERE id = 1")
        .await
        .expect("granted table read");
    assert_eq!(name.as_deref(), Some("alpha"));
    let columns: Vec<Row> = reader
        .query("SHOW FULL COLUMNS FROM admin_records")
        .await
        .expect("table grant permits column metadata");
    assert_eq!(columns.len(), 2);
    assert!(columns
        .iter()
        .all(|column| column.get::<String, _>(7).as_deref() == Some("select")));
    assert!(reader
        .query_drop("CREATE USER 'denied'@'%' IDENTIFIED BY 'secret'")
        .await
        .is_err());

    admin
        .query_drop("REVOKE SELECT ON reports.admin_records FROM 'sqlreader'@'%'")
        .await
        .expect("revoke table select");
    assert!(reader
        .query_first::<String, _>("SELECT name FROM admin_records WHERE id = 1")
        .await
        .is_err());
    reader.disconnect().await.expect("reader disconnects");
    admin.disconnect().await.expect("admin disconnects");
    runtime.stop_after_sigterm();
}

fn run_external_driver(command: &[&str], endpoint: SocketAddr, ca: &Path) {
    let output = Command::new("timeout")
        .args(["--signal=KILL", "15s"])
        .args(command)
        .env(
            "TURSO_MYSQL_DRIVER_ENDPOINT",
            format!("localhost:{}", endpoint.port()),
        )
        .env("TURSO_MYSQL_DRIVER_CA", ca)
        .env("TURSO_MYSQL_DRIVER_PASSWORD", PASSWORD)
        .output()
        .expect("external driver starts");
    assert!(
        output.status.success(),
        "external driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_mysql_cli(endpoint: SocketAddr, ca: &Path, sql: &str) -> String {
    let mut child = Command::new("timeout")
        .args(["--signal=KILL", "15s", "mysql"])
        .args([
            "--no-defaults",
            "--protocol=tcp",
            "--host=localhost",
            "--user=gateadmin",
            "--ssl-mode=VERIFY_IDENTITY",
            "--default-character-set=utf8mb4",
            "--connect-timeout=3",
            "--batch",
            "--raw",
            "--skip-column-names",
        ])
        .arg(format!("--port={}", endpoint.port()))
        .arg(format!("--ssl-ca={}", ca.display()))
        .arg("reports")
        .env("HOME", ca.parent().expect("TLS fixture has a directory"))
        .env("MYSQL_PWD", PASSWORD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("MySQL CLI starts");
    child
        .stdin
        .take()
        .expect("MySQL CLI stdin is piped")
        .write_all(sql.as_bytes())
        .expect("MySQL CLI accepts the SQL script");
    let output = child.wait_with_output().expect("MySQL CLI exits");
    assert!(
        output.status.success(),
        "MySQL CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("MySQL CLI returns UTF-8 output")
}

fn tcp_options(endpoint: SocketAddr, ssl_opts: SslOpts) -> OptsBuilder {
    tcp_options_for_host(endpoint, "localhost", ssl_opts)
}

fn tcp_options_for_host(endpoint: SocketAddr, hostname: &str, ssl_opts: SslOpts) -> OptsBuilder {
    OptsBuilder::default()
        .ip_or_hostname(hostname)
        .resolved_ips(Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]))
        .tcp_port(endpoint.port())
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .prefer_socket(false)
        .ssl_opts(ssl_opts)
}

fn assert_plaintext_rejected(endpoint: SocketAddr) {
    let mut stream = TcpStream::connect_timeout(&endpoint, CHILD_WAIT)
        .expect("plaintext probe connects to the TCP listener");
    stream
        .set_read_timeout(Some(CHILD_WAIT))
        .expect("plaintext probe read timeout");
    read_greeting(&mut stream).expect("TCP listener sends its initial greeting");
    let response = ClientHandshakeResponseConfig::new(
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        0,
        DEFAULT_UTF8MB4_COLLATION,
        "gateadmin",
        vec![0; 32],
        None::<String>,
        Some(CACHING_SHA2_PASSWORD_PLUGIN.to_owned()),
        None,
    )
    .encode(
        PacketCodec::new(MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH).expect("handshake response codec"),
        CLIENT_HANDSHAKE_SEQUENCE_ID,
    )
    .expect("plaintext handshake response is structurally valid");
    match stream.write_all(&response) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::UnexpectedEof
            ) =>
        {
            return;
        }
        Err(error) => panic!("plaintext probe response write failed: {error}"),
    }

    let mut byte = [0; 1];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Ok(read) => panic!("plaintext probe received {read} unexpected bytes"),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::UnexpectedEof
            ) => {}
        Err(error) => panic!("plaintext probe was not rejected: {error}"),
    }
}

fn read_greeting(stream: &mut TcpStream) -> io::Result<()> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let payload_length =
        usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
    assert!(
        payload_length <= 4096,
        "initial greeting exceeds the configured protocol bound"
    );
    let mut payload = vec![0; payload_length];
    stream.read_exact(&mut payload)
}

fn reserve_local_endpoint() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("local TCP port is free");
    listener
        .local_addr()
        .expect("local TCP endpoint is available")
}

fn wait_for_port_release(endpoint: SocketAddr) {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        match TcpListener::bind(endpoint) {
            Ok(listener) => {
                drop(listener);
                return;
            }
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("TCP port was not released after shutdown: {error}"),
        }
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

fn assert_material(path: &Path, mode: u32) {
    let metadata = fs::metadata(path).expect("TLS fixture metadata is available");
    assert!(metadata.is_file());
    assert_eq!(metadata.uid(), effective_uid());
    assert_eq!(metadata.mode() & 0o7777, mode);
}

fn assert_running_as(uid_environment: &str) {
    let expected = required(uid_environment)
        .parse::<u32>()
        .expect("fixture UID is valid");
    // SAFETY: geteuid has no arguments and only reads process credentials.
    assert_eq!(unsafe { libc::geteuid() }, expected);
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not access Rust memory.
    unsafe { libc::geteuid() }
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("fixture environment {name} is missing"))
}
