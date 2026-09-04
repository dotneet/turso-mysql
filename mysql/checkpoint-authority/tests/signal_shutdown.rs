// Copyright 2026 the Turso authors. All rights reserved. MIT license.

#![cfg(all(unix, any(target_os = "linux", target_os = "macos")))]

use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

const AUTHORITY_ID: &str = "signal-shutdown-test";
const SOCKET_NAME: &str = "authority.sock";
const CHILD_WAIT: Duration = Duration::from_secs(5);
const MAX_CHILD_STDERR_BYTES: u64 = 16 * 1024;

struct TestRoots {
    _parent: TempDir,
    state: PathBuf,
    socket: PathBuf,
}

struct AuthorityProcess {
    child: Child,
    stderr: fs::File,
}

impl AuthorityProcess {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn stderr(&mut self) -> String {
        read_bounded_stderr(&mut self.stderr)
    }
}

impl Drop for AuthorityProcess {
    fn drop(&mut self) {
        let running = self.child.try_wait().ok().flatten().is_none();
        if running {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl TestRoots {
    fn new() -> Self {
        let parent = tempfile::Builder::new()
            .prefix("ca-signal-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let state = parent.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = parent.path().join("socket");
        fs::create_dir(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o710)).unwrap();
        Self {
            _parent: parent,
            state,
            socket,
        }
    }

    fn endpoint(&self) -> PathBuf {
        self.socket.join(SOCKET_NAME)
    }
}

#[test]
fn termination_signals_exit_cleanly_and_remove_the_owned_socket() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        let roots = TestRoots::new();
        let endpoint = roots.endpoint();
        let mut child = spawn_authority(&roots);
        wait_for_socket(&mut child, &endpoint);

        send_signal(&child, signal);
        let status = wait_for_exit(&mut child);
        assert!(
            status.success(),
            "authority did not exit successfully after signal {signal}: {status}"
        );
        assert_endpoint_absent(&endpoint);
    }
}

fn spawn_authority(roots: &TestRoots) -> AuthorityProcess {
    let service_uid = effective_uid();
    let client_uid = if service_uid == u32::MAX {
        0
    } else {
        service_uid + 1
    };
    assert_ne!(service_uid, client_uid);
    let socket_gid = effective_gid().to_string();
    let client_uid = client_uid.to_string();
    let binary = env!("CARGO_BIN_EXE_turso-mysql-checkpoint-authority");
    let stderr = tempfile::tempfile().expect("authority stderr file can be created");
    let child = Command::new(binary)
        .args([
            "--authority-id",
            AUTHORITY_ID,
            "--state-root",
            roots.state.to_str().unwrap(),
            "--socket-directory",
            roots.socket.to_str().unwrap(),
            "--socket-name",
            SOCKET_NAME,
            "--socket-gid",
        ])
        .arg(socket_gid)
        .arg("--client-uid")
        .arg(client_uid)
        .args(["--io-timeout-ms", "100"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .expect("authority stderr file can be cloned"),
        ))
        .spawn()
        .expect("authority process can be spawned");
    AuthorityProcess { child, stderr }
}

fn read_bounded_stderr(stderr: &mut fs::File) -> String {
    stderr
        .seek(SeekFrom::Start(0))
        .expect("authority stderr file can seek");
    let mut bytes = Vec::new();
    Read::by_ref(stderr)
        .take(MAX_CHILD_STDERR_BYTES + 1)
        .read_to_end(&mut bytes)
        .expect("authority stderr file can be read");
    let truncated = bytes.len() > MAX_CHILD_STDERR_BYTES as usize;
    bytes.truncate(MAX_CHILD_STDERR_BYTES as usize);
    let output = String::from_utf8_lossy(&bytes);
    if output.is_empty() {
        "<empty>".to_owned()
    } else if truncated {
        format!("{output}\n<stderr truncated at {MAX_CHILD_STDERR_BYTES} bytes>")
    } else {
        output.into_owned()
    }
}

#[test]
fn child_stderr_diagnostics_are_empty_or_bounded() {
    let mut stderr = tempfile::tempfile().expect("authority stderr file can be created");
    assert_eq!(read_bounded_stderr(&mut stderr), "<empty>");

    let output = "a".repeat(MAX_CHILD_STDERR_BYTES as usize + 1);
    stderr
        .write_all(output.as_bytes())
        .expect("authority stderr fixture can be written");
    let diagnostic = read_bounded_stderr(&mut stderr);
    assert!(diagnostic.starts_with(&"a".repeat(MAX_CHILD_STDERR_BYTES as usize)));
    assert!(diagnostic.ends_with("<stderr truncated at 16384 bytes>"));
    assert!(!diagnostic.contains("a".repeat(MAX_CHILD_STDERR_BYTES as usize + 1).as_str()));
}

#[test]
fn early_child_exit_diagnostics_include_status_and_stderr() {
    let stderr = tempfile::tempfile().expect("authority stderr file can be created");
    let child = Command::new(env!("CARGO_BIN_EXE_turso-mysql-checkpoint-authority"))
        .arg("--definitely-invalid-option")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .expect("authority stderr file can be cloned"),
        ))
        .spawn()
        .expect("authority process can be spawned");
    let mut process = AuthorityProcess { child, stderr };
    let status = process
        .child
        .wait()
        .expect("invalid authority process can be reaped");
    assert!(!status.success());

    let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
        wait_for_socket(&mut process, Path::new("authority-socket-does-not-exist"));
    }))
    .expect_err("an authority that exits before binding must include diagnostics");
    let message = panic
        .downcast_ref::<String>()
        .expect("diagnostic panic uses an owned string");
    assert!(message.contains("authority exited before binding"));
    assert!(message.contains(&status.to_string()));
    assert!(message.contains("checkpoint authority configuration is invalid"));
}

fn wait_for_socket(child: &mut AuthorityProcess, endpoint: &Path) {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if let Ok(metadata) = fs::symlink_metadata(endpoint) {
            assert!(metadata.file_type().is_socket());
            return;
        }
        if let Some(status) = child.try_wait().expect("authority child can be polled") {
            let stderr = child.stderr();
            panic!("authority exited before binding: {status}; stderr:\n{stderr}");
        }
        if Instant::now() >= deadline {
            let status = terminate_with_kill(child);
            let stderr = child.stderr();
            let message = format!(
                concat!(
                    "authority did not bind its socket within {wait:?}; child status after ",
                    "kill: {status:?}; stderr:\n{stderr}"
                ),
                wait = CHILD_WAIT,
                status = status,
                stderr = stderr
            );
            panic!("{message}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn send_signal(child: &AuthorityProcess, signal: libc::c_int) {
    let pid = libc::pid_t::try_from(child.child.id()).expect("child PID fits pid_t");
    // SAFETY: `pid` identifies the child process owned by this test.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
}

fn wait_for_exit(child: &mut AuthorityProcess) -> ExitStatus {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("authority child can be polled") {
            return status;
        }
        if Instant::now() >= deadline {
            let status = terminate_with_kill(child);
            let stderr = child.stderr();
            let message = format!(
                concat!(
                    "authority did not exit after its termination signal within {wait:?}; ",
                    "child status after kill: {status:?}; stderr:\n{stderr}"
                ),
                wait = CHILD_WAIT,
                status = status,
                stderr = stderr
            );
            panic!("{message}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn terminate_with_kill(child: &mut AuthorityProcess) -> Option<ExitStatus> {
    let _ = child.child.kill();
    child.child.wait().ok()
}

fn assert_endpoint_absent(endpoint: &Path) {
    let error = fs::symlink_metadata(endpoint).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid reads process credentials without accessing Rust memory.
    unsafe { libc::geteuid() }
}

fn effective_gid() -> u32 {
    // SAFETY: getegid reads process credentials without accessing Rust memory.
    unsafe { libc::getegid() }
}
