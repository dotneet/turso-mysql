// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Foreground entry point for the local checkpoint authority.

#[cfg(unix)]
use std::{
    fmt,
    path::PathBuf,
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

#[cfg(unix)]
use clap::{error::ErrorKind, Parser};
#[cfg(unix)]
use turso_mysql_checkpoint_authority::{
    AuthorityId, CheckpointAuthority, CheckpointAuthorityConfig,
};

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(name = "turso-mysql-checkpoint-authority")]
#[command(about = "Run a local Unix checkpoint authority")]
struct Arguments {
    /// Opaque authority identity.
    #[arg(long)]
    authority_id: String,

    /// Existing private directory holding durable authority state.
    #[arg(long)]
    state_root: PathBuf,

    /// Existing authority-owned 0710 directory that will hold the Unix socket.
    #[arg(long)]
    socket_directory: PathBuf,

    /// One-component Unix socket filename.
    #[arg(long)]
    socket_name: String,

    /// Effective GID of the dedicated group shared with the client account.
    #[arg(long)]
    socket_gid: u32,

    /// Effective UID of the only permitted client process.
    #[arg(long)]
    client_uid: u32,

    /// Per-connection read/write deadline in milliseconds.
    #[arg(long)]
    io_timeout_ms: u64,
}

#[cfg(unix)]
fn main() -> ExitCode {
    let arguments = match Arguments::try_parse() {
        Ok(arguments) => arguments,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(_) => {
            eprintln!("checkpoint authority configuration is invalid");
            return ExitCode::FAILURE;
        }
    };

    match run(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("checkpoint authority failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("turso-mysql-checkpoint-authority is unsupported on this platform");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
fn run(arguments: Arguments) -> Result<(), DaemonError> {
    let config = configuration_from(arguments)?;
    let stop_requested = Arc::new(AtomicBool::new(false));
    let shutdown: Arc<OnceLock<turso_mysql_checkpoint_authority::CheckpointAuthorityShutdown>> =
        Arc::new(OnceLock::new());
    let handler_stop_requested = Arc::clone(&stop_requested);
    let handler_shutdown = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        handler_stop_requested.store(true, Ordering::Release);
        if let Some(shutdown) = handler_shutdown.get() {
            shutdown.shutdown();
        }
    })
    .map_err(|_| DaemonError::Signal)?;
    let authority = CheckpointAuthority::bind(config).map_err(|_| DaemonError::Bind)?;
    let shutdown_handle = authority.shutdown_handle();
    shutdown
        .set(shutdown_handle.clone())
        .expect("checkpoint authority shutdown handle is installed once");
    if stop_requested.load(Ordering::Acquire) {
        shutdown_handle.shutdown();
    }
    authority.run().map_err(|_| DaemonError::Run)?;
    Ok(())
}

#[cfg(unix)]
fn configuration_from(arguments: Arguments) -> Result<CheckpointAuthorityConfig, DaemonError> {
    let authority =
        AuthorityId::new(arguments.authority_id).map_err(|_| DaemonError::Configuration)?;
    CheckpointAuthorityConfig::new(
        authority,
        arguments.state_root,
        arguments.socket_directory,
        arguments.socket_name,
        arguments.socket_gid,
        arguments.client_uid,
        Duration::from_millis(arguments.io_timeout_ms),
    )
    .map_err(|_| DaemonError::Configuration)
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonError {
    Configuration,
    Bind,
    Signal,
    Run,
}

#[cfg(unix)]
impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration => f.write_str("configuration is invalid"),
            Self::Bind => f.write_str("service could not be started"),
            Self::Signal => f.write_str("shutdown signal handler is unavailable"),
            Self::Run => f.write_str("service stopped with an error"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{path::Path, time::Duration};

    use clap::Parser;

    use super::{configuration_from, Arguments, DaemonError};

    #[test]
    fn parses_an_explicit_authority_configuration() {
        let arguments = Arguments::try_parse_from([
            "turso-mysql-checkpoint-authority",
            "--authority-id",
            "account-store",
            "--state-root",
            "/var/lib/turso/checkpoints",
            "--socket-directory",
            "/run/turso",
            "--socket-name",
            "checkpoint.sock",
            "--socket-gid",
            "1002",
            "--client-uid",
            "1001",
            "--io-timeout-ms",
            "250",
        ])
        .expect("arguments should parse");

        let configuration = configuration_from(arguments).expect("configuration should validate");
        assert_eq!(configuration.authority().as_str(), "account-store");
        assert_eq!(
            configuration.state_root(),
            Path::new("/var/lib/turso/checkpoints")
        );
        assert_eq!(configuration.socket_directory(), Path::new("/run/turso"));
        assert_eq!(configuration.socket_name(), "checkpoint.sock");
        assert_eq!(configuration.socket_gid(), 1002);
        assert_eq!(configuration.client_uid(), 1001);
        assert_eq!(configuration.io_timeout(), Duration::from_millis(250));
    }

    #[test]
    fn requires_each_explicit_option() {
        assert!(Arguments::try_parse_from(["turso-mysql-checkpoint-authority"]).is_err());
    }

    #[test]
    fn rejects_an_invalid_authority_before_binding() {
        let arguments = Arguments::try_parse_from([
            "turso-mysql-checkpoint-authority",
            "--authority-id",
            "../account-store",
            "--state-root",
            "/var/lib/turso/checkpoints",
            "--socket-directory",
            "/run/turso",
            "--socket-name",
            "checkpoint.sock",
            "--socket-gid",
            "1002",
            "--client-uid",
            "1001",
            "--io-timeout-ms",
            "250",
        ])
        .expect("arguments should parse before domain validation");

        assert_eq!(
            configuration_from(arguments),
            Err(DaemonError::Configuration)
        );
    }
}
