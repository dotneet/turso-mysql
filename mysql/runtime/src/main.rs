// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Foreground entry point for the MySQL runtime.

#[cfg(unix)]
use std::{
    fmt,
    net::SocketAddr,
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
    AuthorityId, UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
};
#[cfg(unix)]
use turso_mysql_server::{
    AccountStoreCheckpointReader, CheckpointAuthorityId, RuntimeConfig, RuntimeLimits,
    RuntimeTcpServer, RuntimeTcpServerRunError, RuntimeTimeouts, RuntimeUnixServer,
    RuntimeUnixServerRunError, TcpConfig, TlsConfig, UnixSocketConfig,
};

#[cfg(unix)]
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(name = "turso-mysql-server")]
#[command(about = "Run the local Unix or mandatory-TLS TCP Turso MySQL server")]
struct Arguments {
    /// Existing private directory holding MySQL database data.
    #[arg(long)]
    data_root: PathBuf,

    /// Existing private directory holding the account-store snapshot.
    #[arg(long)]
    account_store_root: PathBuf,

    /// TCP address to listen on. Requires TLS certificate and private-key paths.
    #[arg(
        long,
        value_name = "IP:PORT",
        conflicts_with_all = ["socket_directory", "socket_name"],
        requires_all = ["tls_cert", "tls_key"]
    )]
    listen: Option<SocketAddr>,

    /// Absolute server certificate-chain path for the TCP listener.
    #[arg(long, value_name = "PATH", requires = "listen")]
    tls_cert: Option<PathBuf>,

    /// Absolute server private-key path for the TCP listener.
    #[arg(long, value_name = "PATH", requires = "listen")]
    tls_key: Option<PathBuf>,

    /// Existing private directory that will hold the MySQL Unix socket.
    #[arg(long, requires = "socket_name", conflicts_with = "listen")]
    socket_directory: Option<PathBuf>,

    /// One-component MySQL Unix socket filename.
    #[arg(long, requires = "socket_directory", conflicts_with = "listen")]
    socket_name: Option<String>,

    /// Opaque identifier for the external checkpoint authority.
    #[arg(long)]
    authority_id: String,

    /// Absolute Unix socket path for the checkpoint authority.
    #[arg(long)]
    authority_socket: PathBuf,

    /// Effective UID expected for the checkpoint-authority service.
    #[arg(long)]
    authority_service_uid: u32,

    /// Bound for one checkpoint-authority RPC in milliseconds.
    #[arg(long)]
    authority_rpc_timeout_ms: u64,

    /// Interval between account-store reload attempts in milliseconds.
    #[arg(long)]
    reload_interval_ms: u64,

    /// Maximum active protocol connections.
    #[arg(long)]
    max_connections: usize,

    /// Maximum connections authenticating at once.
    #[arg(long)]
    max_admissions: usize,

    /// Maximum queued response bytes per connection.
    #[arg(long)]
    max_write_bytes: usize,

    /// Maximum queued response frames per connection.
    #[arg(long)]
    max_write_frames: usize,

    /// Account checkpoint read deadline in milliseconds.
    #[arg(long)]
    checkpoint_timeout_ms: u64,

    /// Reserved TLS lifecycle deadline in milliseconds.
    #[arg(long)]
    tls_timeout_ms: u64,

    /// Client authentication deadline in milliseconds.
    #[arg(long)]
    authentication_timeout_ms: u64,

    /// Idle connection deadline in milliseconds.
    #[arg(long)]
    idle_timeout_ms: u64,

    /// Checked query deadline in milliseconds.
    #[arg(long)]
    query_timeout_ms: u64,

    /// Response write deadline in milliseconds.
    #[arg(long)]
    write_timeout_ms: u64,

    /// Deadline for draining on shutdown in milliseconds.
    #[arg(long)]
    shutdown_timeout_ms: u64,
}

#[cfg(unix)]
struct Configuration {
    runtime: RuntimeConfig,
    authority_client: UnixCheckpointAuthorityClientConfig,
}

#[cfg(unix)]
impl Configuration {
    fn from_arguments(arguments: Arguments) -> Result<Self, DaemonError> {
        let tcp_tls = match (arguments.tls_cert, arguments.tls_key) {
            (Some(certificate), Some(private_key)) => Some(
                TlsConfig::new(certificate, private_key).map_err(|_| DaemonError::Configuration)?,
            ),
            (None, None) => None,
            _ => return Err(DaemonError::Configuration),
        };
        let tcp = match (arguments.listen, tcp_tls) {
            (Some(bind), Some(tls)) => Some(TcpConfig::new(bind, tls)),
            (None, None) => None,
            _ => return Err(DaemonError::Configuration),
        };
        let unix_socket = match (arguments.socket_directory, arguments.socket_name) {
            (Some(directory), Some(name)) => Some(
                UnixSocketConfig::new(directory, name).map_err(|_| DaemonError::Configuration)?,
            ),
            (None, None) => None,
            _ => return Err(DaemonError::Configuration),
        };
        let authority_id = AuthorityId::new(arguments.authority_id.clone())
            .map_err(|_| DaemonError::Configuration)?;
        let checkpoint_authority = CheckpointAuthorityId::new(arguments.authority_id)
            .map_err(|_| DaemonError::Configuration)?;
        let authority_rpc_timeout = duration_from_millis(arguments.authority_rpc_timeout_ms)?;
        let authority_client = UnixCheckpointAuthorityClientConfig::new(
            arguments.authority_socket,
            authority_id,
            arguments.authority_service_uid,
            authority_rpc_timeout,
        )
        .map_err(|_| DaemonError::Configuration)?;
        let limits = RuntimeLimits::new(
            arguments.max_connections,
            arguments.max_admissions,
            arguments.max_write_bytes,
            arguments.max_write_frames,
        )
        .map_err(|_| DaemonError::Configuration)?;
        let timeouts = RuntimeTimeouts::new(
            duration_from_millis(arguments.checkpoint_timeout_ms)?,
            duration_from_millis(arguments.tls_timeout_ms)?,
            duration_from_millis(arguments.authentication_timeout_ms)?,
            duration_from_millis(arguments.idle_timeout_ms)?,
            duration_from_millis(arguments.write_timeout_ms)?,
            duration_from_millis(arguments.shutdown_timeout_ms)?,
        )
        .map_err(|_| DaemonError::Configuration)?
        .with_query_timeout(duration_from_millis(arguments.query_timeout_ms)?)
        .map_err(|_| DaemonError::Configuration)?;
        let runtime = RuntimeConfig::new(
            tcp,
            unix_socket,
            arguments.data_root,
            arguments.account_store_root,
            checkpoint_authority,
            duration_from_millis(arguments.reload_interval_ms)?,
            limits,
            timeouts,
        )
        .map_err(|_| DaemonError::Configuration)?;
        Ok(Self {
            runtime,
            authority_client,
        })
    }
}

#[cfg(unix)]
fn main() -> ExitCode {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        eprintln!("turso-mysql-server is unsupported on this platform");
        return ExitCode::FAILURE;
    }
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
            eprintln!("MySQL server configuration is invalid");
            return ExitCode::FAILURE;
        }
    };
    match run(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("MySQL server failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("turso-mysql-server is unsupported on this platform");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
#[derive(Clone, Debug)]
enum RuntimeShutdownHandle {
    Unix(turso_mysql_server::RuntimeUnixServerShutdown),
    Tcp(turso_mysql_server::RuntimeTcpServerShutdown),
}

#[cfg(unix)]
impl RuntimeShutdownHandle {
    fn request_shutdown(&self) {
        match self {
            Self::Unix(handle) => handle.request_shutdown(),
            Self::Tcp(handle) => handle.request_shutdown(),
        }
    }
}

#[cfg(unix)]
fn run(arguments: Arguments) -> Result<(), DaemonError> {
    let configuration = Configuration::from_arguments(arguments)?;
    let stop_requested = Arc::new(AtomicBool::new(false));
    let shutdown: Arc<OnceLock<RuntimeShutdownHandle>> = Arc::new(OnceLock::new());
    install_shutdown_handler(Arc::clone(&stop_requested), Arc::clone(&shutdown))?;

    let Configuration {
        runtime,
        authority_client,
    } = configuration;
    let authority: Arc<dyn AccountStoreCheckpointReader> = Arc::new(
        UnixCheckpointAuthorityClient::new(authority_client).map_err(|_| DaemonError::Authority)?,
    );
    if runtime.tcp().is_some() {
        run_tcp(runtime, authority, stop_requested, shutdown)
    } else {
        run_unix(runtime, authority, stop_requested, shutdown)
    }
}

#[cfg(unix)]
fn run_unix(
    runtime: RuntimeConfig,
    authority: Arc<dyn AccountStoreCheckpointReader>,
    stop_requested: Arc<AtomicBool>,
    shutdown: Arc<OnceLock<RuntimeShutdownHandle>>,
) -> Result<(), DaemonError> {
    let server = RuntimeUnixServer::bind(&runtime, authority).map_err(|_| DaemonError::Bind)?;
    let shutdown_handle = server.shutdown_handle();
    shutdown
        .set(RuntimeShutdownHandle::Unix(shutdown_handle.clone()))
        .expect("server shutdown handle is installed once");
    if stop_requested.load(Ordering::Acquire) {
        shutdown_handle.request_shutdown();
    }

    let run_result = server.run();
    let shutdown_status = shutdown_result(server.shutdown().drained());
    if let Err(error) = shutdown_status {
        retain_server_until_process_exit(server);
        return Err(error);
    }
    match run_result {
        Ok(()) => {}
        Err(RuntimeUnixServerRunError::ShuttingDown) if stop_requested.load(Ordering::Acquire) => {}
        Err(_) => return Err(DaemonError::Run),
    }
    Ok(())
}

#[cfg(unix)]
fn run_tcp(
    runtime: RuntimeConfig,
    authority: Arc<dyn AccountStoreCheckpointReader>,
    stop_requested: Arc<AtomicBool>,
    shutdown: Arc<OnceLock<RuntimeShutdownHandle>>,
) -> Result<(), DaemonError> {
    let server = RuntimeTcpServer::bind(&runtime, authority).map_err(|_| DaemonError::Bind)?;
    let shutdown_handle = server.shutdown_handle();
    shutdown
        .set(RuntimeShutdownHandle::Tcp(shutdown_handle.clone()))
        .expect("server shutdown handle is installed once");
    if stop_requested.load(Ordering::Acquire) {
        shutdown_handle.request_shutdown();
    }

    let run_result = server.run();
    let shutdown_status = shutdown_result(server.shutdown().drained());
    if let Err(error) = shutdown_status {
        retain_tcp_server_until_process_exit(server);
        return Err(error);
    }
    match run_result {
        Ok(()) => {}
        Err(RuntimeTcpServerRunError::ShuttingDown) if stop_requested.load(Ordering::Acquire) => {}
        Err(_) => return Err(DaemonError::Run),
    }
    Ok(())
}

#[cfg(unix)]
fn shutdown_result(drained: bool) -> Result<(), DaemonError> {
    if drained {
        Ok(())
    } else {
        Err(DaemonError::Shutdown)
    }
}

#[cfg(unix)]
fn retain_server_until_process_exit(server: RuntimeUnixServer) {
    // The bounded shutdown already attempted socket cleanup; dropping now can wait forever.
    std::mem::forget(server);
}

#[cfg(unix)]
fn retain_tcp_server_until_process_exit(server: RuntimeTcpServer) {
    // The bounded shutdown already attempted to stop the listener; dropping now can wait forever.
    std::mem::forget(server);
}

#[cfg(unix)]
fn install_shutdown_handler(
    stop_requested: Arc<AtomicBool>,
    shutdown: Arc<OnceLock<RuntimeShutdownHandle>>,
) -> Result<(), DaemonError> {
    ctrlc::set_handler(move || {
        stop_requested.store(true, Ordering::Release);
        if let Some(shutdown) = shutdown.get() {
            shutdown.request_shutdown();
        }
    })
    .map_err(|_| DaemonError::Signal)
}

#[cfg(unix)]
fn duration_from_millis(milliseconds: u64) -> Result<Duration, DaemonError> {
    let duration = Duration::from_millis(milliseconds);
    if duration.is_zero() || duration > MAX_TIMEOUT {
        return Err(DaemonError::Configuration);
    }
    Ok(duration)
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonError {
    Configuration,
    Authority,
    Bind,
    Signal,
    Run,
    Shutdown,
}

#[cfg(unix)]
impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration => f.write_str("configuration is invalid"),
            Self::Authority => f.write_str("checkpoint authority client is unavailable"),
            Self::Bind => f.write_str("server could not be started"),
            Self::Signal => f.write_str("shutdown signal handler is unavailable"),
            Self::Run => f.write_str("server stopped with an error"),
            Self::Shutdown => f.write_str("server did not finish shutdown in time"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{path::Path, time::Duration};

    use clap::Parser;

    use super::{duration_from_millis, shutdown_result, Arguments, Configuration, DaemonError};

    const ARGUMENTS: [&str; 41] = [
        "turso-mysql-server",
        "--data-root",
        "/var/lib/turso-mysql/data",
        "--account-store-root",
        "/var/lib/turso-mysql/accounts",
        "--socket-directory",
        "/run/turso-mysql",
        "--socket-name",
        "mysql.sock",
        "--authority-id",
        "account-store",
        "--authority-socket",
        "/run/turso-mysql-checkpoint/authority.sock",
        "--authority-service-uid",
        "1002",
        "--authority-rpc-timeout-ms",
        "100",
        "--reload-interval-ms",
        "1000",
        "--max-connections",
        "8",
        "--max-admissions",
        "4",
        "--max-write-bytes",
        "8192",
        "--max-write-frames",
        "8",
        "--checkpoint-timeout-ms",
        "100",
        "--tls-timeout-ms",
        "100",
        "--authentication-timeout-ms",
        "100",
        "--idle-timeout-ms",
        "100",
        "--query-timeout-ms",
        "100",
        "--write-timeout-ms",
        "100",
        "--shutdown-timeout-ms",
        "100",
    ];

    fn tcp_arguments() -> Vec<&'static str> {
        vec![
            "turso-mysql-server",
            "--data-root",
            "/var/lib/turso-mysql/data",
            "--account-store-root",
            "/var/lib/turso-mysql/accounts",
            "--listen",
            "127.0.0.1:3306",
            "--tls-cert",
            "/etc/turso/server.crt",
            "--tls-key",
            "/etc/turso/server.key",
            "--authority-id",
            "account-store",
            "--authority-socket",
            "/run/turso-mysql-checkpoint/authority.sock",
            "--authority-service-uid",
            "1002",
            "--authority-rpc-timeout-ms",
            "100",
            "--reload-interval-ms",
            "1000",
            "--max-connections",
            "8",
            "--max-admissions",
            "4",
            "--max-write-bytes",
            "8192",
            "--max-write-frames",
            "8",
            "--checkpoint-timeout-ms",
            "100",
            "--tls-timeout-ms",
            "100",
            "--authentication-timeout-ms",
            "100",
            "--idle-timeout-ms",
            "100",
            "--query-timeout-ms",
            "100",
            "--write-timeout-ms",
            "100",
            "--shutdown-timeout-ms",
            "100",
        ]
    }

    #[test]
    fn parses_an_explicit_runtime_configuration() {
        let configuration = Configuration::from_arguments(
            Arguments::try_parse_from(ARGUMENTS).expect("arguments should parse"),
        )
        .expect("configuration should validate");

        assert_eq!(
            configuration.runtime.data_root(),
            Path::new("/var/lib/turso-mysql/data")
        );
        assert_eq!(
            configuration.runtime.account_root(),
            Path::new("/var/lib/turso-mysql/accounts")
        );
        assert_eq!(
            configuration.runtime.unix_socket().unwrap().socket_path(),
            Path::new("/run/turso-mysql/mysql.sock")
        );
        assert_eq!(
            configuration.runtime.reload_interval(),
            Duration::from_secs(1)
        );
        assert_eq!(
            configuration.runtime.timeouts().query(),
            Duration::from_millis(100)
        );
        assert_eq!(
            configuration.authority_client.rpc_timeout(),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn parses_a_tcp_configuration_with_mandatory_tls() {
        let configuration = Configuration::from_arguments(
            Arguments::try_parse_from(tcp_arguments()).expect("arguments should parse"),
        )
        .expect("configuration should validate");

        let tcp = configuration.runtime.tcp().expect("TCP is configured");
        assert_eq!(tcp.bind(), "127.0.0.1:3306".parse().unwrap());
        assert_eq!(
            tcp.tls().certificate_path(),
            Path::new("/etc/turso/server.crt")
        );
        assert_eq!(
            tcp.tls().private_key_path(),
            Path::new("/etc/turso/server.key")
        );
        assert!(configuration.runtime.unix_socket().is_none());
    }

    #[test]
    fn rejects_tcp_without_both_tls_paths() {
        let mut arguments = tcp_arguments();
        arguments
            .retain(|argument| *argument != "--tls-key" && *argument != "/etc/turso/server.key");
        assert!(Arguments::try_parse_from(arguments).is_err());
    }

    #[test]
    fn rejects_mixing_tcp_and_unix_listener_arguments() {
        let mut arguments = ARGUMENTS.to_vec();
        arguments.splice(
            5..5,
            [
                "--listen",
                "127.0.0.1:3306",
                "--tls-cert",
                "/etc/turso/server.crt",
                "--tls-key",
                "/etc/turso/server.key",
            ],
        );
        assert!(Arguments::try_parse_from(arguments).is_err());
    }

    #[test]
    fn rejects_a_configuration_without_a_listener() {
        let mut arguments = ARGUMENTS.to_vec();
        arguments.drain(5..9);
        let parsed = Arguments::try_parse_from(arguments).expect("arguments should parse");
        assert!(matches!(
            Configuration::from_arguments(parsed),
            Err(DaemonError::Configuration)
        ));
    }

    #[test]
    fn requires_every_explicit_option() {
        assert!(Arguments::try_parse_from(["turso-mysql-server"]).is_err());
    }

    #[test]
    fn rejects_invalid_authority_without_exposing_it() {
        let mut arguments = ARGUMENTS;
        arguments[10] = "../account-store";
        let parsed = Arguments::try_parse_from(arguments).expect("arguments should parse");

        assert!(matches!(
            Configuration::from_arguments(parsed),
            Err(DaemonError::Configuration)
        ));
    }

    #[test]
    fn rejects_zero_and_unbounded_timeouts() {
        assert_eq!(duration_from_millis(0), Err(DaemonError::Configuration));
        assert_eq!(
            duration_from_millis(24 * 60 * 60 * 1000 + 1),
            Err(DaemonError::Configuration)
        );
    }

    #[test]
    fn reports_an_incomplete_shutdown_for_process_exit() {
        assert_eq!(shutdown_result(true), Ok(()));
        assert_eq!(shutdown_result(false), Err(DaemonError::Shutdown));
    }
}
