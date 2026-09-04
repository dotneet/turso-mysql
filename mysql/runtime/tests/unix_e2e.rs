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

use mysql_async::{Conn, OptsBuilder, prelude::Queryable};
use tempfile::TempDir;
use turso_mysql::MySqlDatabaseCatalog;

const SOCKET_ENV: &str = "TURSO_MYSQL_CROSS_UID_SOCKET";
const AUTHORITY_ENV: &str = "TURSO_MYSQL_CROSS_UID_AUTHORITY";
const SERVICE_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_SERVICE_UID";
const CLIENT_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_CLIENT_UID";
const ACCOUNT_STORE_ROOT_ENV: &str = "TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT";
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
        let binary = env!("CARGO_BIN_EXE_turso-mysql-server");
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
async fn runtime_binary_authenticates_and_serves_prepared_queries_over_a_unix_socket() {
    let fixture = Fixture::from_environment();
    let roots = private_roots(&fixture.account_root);
    let catalog = MySqlDatabaseCatalog::open(roots.data_root()).expect("catalog opens");
    assert_eq!(catalog.create("reports"), Ok("reports".to_owned()));
    drop(catalog);

    let mut runtime = RuntimeProcess::start(&fixture, roots.data_root(), roots.socket_directory());
    let options = OptsBuilder::default()
        .user(Some("gateadmin"))
        .pass(Some(PASSWORD))
        .socket(Some(path_argument(&runtime.endpoint)))
        .max_allowed_packet(Some(64 * 1024))
        .wait_timeout(Some(1));
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
    connection
        .disconnect()
        .await
        .expect("external driver closes cleanly");

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
