// Copyright 2026 the Turso authors. All rights reserved. MIT license.

#![cfg(all(unix, any(target_os = "linux", target_os = "macos")))]

use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

const AUTHORITY_ID: &str = "signal-shutdown-test";
const SOCKET_NAME: &str = "authority.sock";
const CHILD_WAIT: Duration = Duration::from_secs(5);

struct TestRoots {
    _parent: TempDir,
    state: PathBuf,
    socket: PathBuf,
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

fn spawn_authority(roots: &TestRoots) -> Child {
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
    Command::new(binary)
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
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_socket(child: &mut Child, endpoint: &Path) {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if let Ok(metadata) = fs::symlink_metadata(endpoint) {
            assert!(metadata.file_type().is_socket());
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("authority exited before binding: {status}");
        }
        if Instant::now() >= deadline {
            terminate_with_kill(child);
            panic!("authority did not bind its socket");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn send_signal(child: &Child, signal: libc::c_int) {
    let pid = libc::pid_t::try_from(child.id()).expect("child PID fits pid_t");
    // SAFETY: `pid` identifies the child process owned by this test.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
}

fn wait_for_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            terminate_with_kill(child);
            panic!("authority did not exit after its termination signal");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn terminate_with_kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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
