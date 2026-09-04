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

use mysql_async::{Conn, OptsBuilder, SslOpts, prelude::Queryable};
use tempfile::TempDir;
use turso_mysql::MySqlDatabaseCatalog;
use turso_mysql_server::{
    CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_HANDSHAKE_SEQUENCE_ID, ClientHandshakeResponseConfig,
    DEFAULT_UTF8MB4_COLLATION, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH, PacketCodec,
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
