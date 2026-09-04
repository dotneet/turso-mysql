//! Side-effect-free validation for the MySQL server runtime.
//!
//! This module deliberately does not open listeners, inspect permissions, read
//! certificates, or talk to a checkpoint service. Those operations belong to a
//! runtime owner which must enforce the contracts represented here.

use std::{
    error::Error,
    fmt,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use std::os::unix::ffi::OsStrExt;

use crate::{AccountStoreCheckpoint, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH, PACKET_HEADER_LEN};

pub use turso_mysql::{DEFAULT_MAX_PREPARED_STMT_COUNT, MAX_PREPARED_STMT_COUNT};

/// The smallest and largest accepted account-generation reload intervals.
pub const MIN_RELOAD_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_RELOAD_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum values accepted for the three runtime resource limits.
pub const MAX_CONNECTION_LIMIT: usize = 65_536;
pub const MAX_ADMISSION_LIMIT: usize = 65_536;
pub const MAX_WRITE_LIMIT: usize = 64 * 1024 * 1024;
/// The smallest write queue that can retain one maximum-size initial handshake.
pub const MIN_WRITE_LIMIT: usize = MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH + PACKET_HEADER_LEN;
/// The largest number of response frames retained by one connection.
pub const MAX_WRITE_FRAME_LIMIT: usize = 4_096;
/// The largest accepted transport lifecycle timeout.
pub const MAX_RUNTIME_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// Default deadline for one checked query when no override is configured.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest Unix socket path accepted by both Linux and macOS.
///
/// macOS reserves one byte of its 104-byte `sun_path` buffer for the
/// terminating NUL, so the shared limit is 103 raw path bytes.
pub const MAX_UNIX_SOCKET_PATH_BYTES: usize = 103;

const MAX_CHECKPOINT_AUTHORITY_ID_BYTES: usize = 256;
const MAX_SOCKET_FILENAME_BYTES: usize = 255;

/// A TCP endpoint with mandatory TLS certificate and private-key references.
#[derive(Clone, PartialEq, Eq)]
pub struct TcpConfig {
    bind: SocketAddr,
    tls: TlsConfig,
}

impl TcpConfig {
    /// Creates a TCP endpoint. TLS cannot be omitted from this type.
    pub const fn new(bind: SocketAddr, tls: TlsConfig) -> Self {
        Self { bind, tls }
    }

    /// Returns the address the runtime should bind.
    pub const fn bind(&self) -> SocketAddr {
        self.bind
    }

    /// Returns the validated TLS references.
    pub const fn tls(&self) -> &TlsConfig {
        &self.tls
    }
}

impl fmt::Debug for TcpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpConfig")
            .field("bind", &self.bind)
            .field("tls", &self.tls)
            .finish()
    }
}

/// Certificate and private-key references for a TCP endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct TlsConfig {
    certificate_path: PathBuf,
    private_key_path: PathBuf,
}

impl TlsConfig {
    /// Creates TLS references without reading either file.
    pub fn new(
        certificate_path: impl AsRef<Path>,
        private_key_path: impl AsRef<Path>,
    ) -> Result<Self, RuntimeConfigError> {
        let certificate_path = absolute_path(certificate_path.as_ref(), PathField::TlsCertificate)?;
        let private_key_path = absolute_path(private_key_path.as_ref(), PathField::TlsPrivateKey)?;
        if same_path(&certificate_path, &private_key_path) {
            return Err(RuntimeConfigError::TlsCertificateAndKeyPathsEqual);
        }
        Ok(Self {
            certificate_path,
            private_key_path,
        })
    }

    /// Returns the certificate reference.
    pub fn certificate_path(&self) -> &Path {
        &self.certificate_path
    }

    /// Returns the private-key reference.
    pub fn private_key_path(&self) -> &Path {
        &self.private_key_path
    }
}

impl fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConfig")
            .field("certificate_path", &"<redacted>")
            .field("private_key_path", &"<redacted>")
            .finish()
    }
}

/// The only Unix-socket access policy currently accepted by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSocketPolicy {
    /// The runtime must restrict clients to the server's effective UID.
    SameEffectiveUid,
}

/// A local Unix socket endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct UnixSocketConfig {
    directory: PathBuf,
    filename: String,
    policy: UnixSocketPolicy,
}

impl fmt::Debug for UnixSocketConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnixSocketConfig")
            .field("directory", &"<redacted>")
            .field("filename", &"<redacted>")
            .field("policy", &self.policy)
            .finish()
    }
}

impl UnixSocketConfig {
    /// Creates a socket using the mandatory same-effective-UID policy.
    pub fn new(
        directory: impl AsRef<Path>,
        filename: impl Into<String>,
    ) -> Result<Self, RuntimeConfigError> {
        Self::new_with_policy(directory, filename, UnixSocketPolicy::SameEffectiveUid)
    }

    /// Creates a socket while keeping the access policy explicit at the call site.
    pub fn new_with_policy(
        directory: impl AsRef<Path>,
        filename: impl Into<String>,
        policy: UnixSocketPolicy,
    ) -> Result<Self, RuntimeConfigError> {
        let directory = absolute_path(directory.as_ref(), PathField::UnixSocketDirectory)?;
        let filename = filename.into();
        if !simple_filename(&filename) {
            return Err(if filename.is_empty() {
                RuntimeConfigError::UnixSocketFilenameEmpty
            } else {
                RuntimeConfigError::UnixSocketFilenameNotSimple
            });
        }
        if !unix_socket_path_within_limit(&directory, &filename) {
            return Err(RuntimeConfigError::UnixSocketPathTooLong);
        }
        match policy {
            UnixSocketPolicy::SameEffectiveUid => {}
        }
        Ok(Self {
            directory,
            filename,
            policy,
        })
    }

    /// Returns the directory whose private permissions the runtime must check.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the one-component socket filename.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Returns the required client-access policy.
    pub const fn policy(&self) -> UnixSocketPolicy {
        self.policy
    }

    /// Returns the lexical endpoint path; no filesystem operation is performed.
    pub fn socket_path(&self) -> PathBuf {
        self.directory.join(&self.filename)
    }
}

/// An opaque identifier owned by an external rollback-resistant authority.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CheckpointAuthorityId(String);

impl CheckpointAuthorityId {
    /// Creates an identifier without treating it as a local filename.
    pub fn new(identifier: impl Into<String>) -> Result<Self, RuntimeConfigError> {
        let identifier = identifier.into();
        let bytes = identifier.as_bytes();
        if bytes.is_empty() {
            return Err(RuntimeConfigError::CheckpointAuthorityIdEmpty);
        }
        if bytes.len() > MAX_CHECKPOINT_AUTHORITY_ID_BYTES {
            return Err(RuntimeConfigError::CheckpointAuthorityIdTooLong);
        }
        if bytes.contains(&0) {
            return Err(RuntimeConfigError::CheckpointAuthorityIdContainsNul);
        }
        if identifier == "."
            || identifier == ".."
            || identifier.starts_with('/')
            || identifier.contains('/')
            || identifier.contains('\\')
        {
            return Err(RuntimeConfigError::CheckpointAuthorityIdLooksLikePath);
        }
        Ok(Self(identifier))
    }

    /// Returns the opaque identifier for the external authority.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CheckpointAuthorityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointAuthorityId")
            .field("identifier", &"<redacted>")
            .finish()
    }
}

/// Starts bounded reads of exact account checkpoints from the external control plane.
///
/// [`Self::request_checkpoint`] must return without performing blocking I/O.
/// The returned request gives the runtime ownership of the timeout. After
/// cancellation, a backend must stop its external work and send or drop the
/// response to acknowledge completion. It must also serialize startup retries
/// for one authority until that acknowledgement is complete.
pub trait AccountStoreCheckpointReader: Send + Sync {
    /// Starts one read without exposing backend-specific failures.
    fn request_checkpoint(
        &self,
        authority: &CheckpointAuthorityId,
    ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError>;
}

/// The runtime-owned receiving half of one checkpoint read.
pub struct AccountStoreCheckpointRequest {
    receiver: Receiver<Result<AccountStoreCheckpoint, CheckpointReadError>>,
    cancelled: Arc<AtomicBool>,
    wake: AccountStoreCheckpointWake,
    finished: bool,
}

impl AccountStoreCheckpointRequest {
    /// Creates a one-shot response pair for an asynchronous checkpoint backend.
    pub fn channel() -> (AccountStoreCheckpointResponse, Self) {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let wake = AccountStoreCheckpointWake::new();
        (
            AccountStoreCheckpointResponse {
                sender,
                cancelled: Arc::clone(&cancelled),
                wake: wake.clone(),
            },
            Self {
                receiver,
                cancelled,
                wake,
                finished: false,
            },
        )
    }

    /// Creates an already-completed request for in-process authorities.
    pub fn completed(result: Result<AccountStoreCheckpoint, CheckpointReadError>) -> Self {
        let (response, request) = Self::channel();
        let _ = response.complete(result);
        request
    }

    pub(crate) fn wait(self, timeout: Duration) -> AccountStoreCheckpointWait {
        self.wait_until(timeout, || false)
    }

    /// Waits for a checkpoint while allowing a runtime shutdown to cancel it.
    pub(crate) fn wait_until_shutdown(
        self,
        timeout: Duration,
        shutting_down: &AtomicBool,
    ) -> AccountStoreCheckpointWait {
        self.wait_until(timeout, || shutting_down.load(Ordering::Acquire))
    }

    pub(crate) fn wake_handle(&self) -> AccountStoreCheckpointWake {
        self.wake.clone()
    }

    fn wait_until(
        mut self,
        timeout: Duration,
        should_stop: impl Fn() -> bool,
    ) -> AccountStoreCheckpointWait {
        let deadline = Instant::now() + timeout;
        loop {
            if should_stop() {
                return AccountStoreCheckpointWait::Stopped(self.cancel());
            }
            match self.receiver.try_recv() {
                Ok(result) => {
                    self.finished = true;
                    return AccountStoreCheckpointWait::Completed(result);
                }
                Err(TryRecvError::Disconnected) => {
                    self.finished = true;
                    return AccountStoreCheckpointWait::Completed(Err(
                        CheckpointReadError::Unavailable,
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return AccountStoreCheckpointWait::TimedOut(self.cancel());
            }

            let guard = self
                .wake
                .lock
                .lock()
                .expect("checkpoint wake state must not be poisoned");
            if should_stop() {
                drop(guard);
                return AccountStoreCheckpointWait::Stopped(self.cancel());
            }
            match self.receiver.try_recv() {
                Ok(result) => {
                    self.finished = true;
                    return AccountStoreCheckpointWait::Completed(result);
                }
                Err(TryRecvError::Disconnected) => {
                    self.finished = true;
                    return AccountStoreCheckpointWait::Completed(Err(
                        CheckpointReadError::Unavailable,
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
            drop(
                self.wake
                    .changed
                    .wait_timeout(guard, remaining)
                    .expect("checkpoint wake state must not be poisoned"),
            );
        }
    }

    pub(crate) fn cancellation_finished(&mut self) -> bool {
        match self.receiver.try_recv() {
            Ok(_) | Err(TryRecvError::Disconnected) => {
                self.finished = true;
                true
            }
            Err(TryRecvError::Empty) => false,
        }
    }

    fn cancel(self) -> Self {
        self.cancelled.store(true, Ordering::Release);
        self
    }
}

impl Drop for AccountStoreCheckpointRequest {
    fn drop(&mut self) {
        if !self.finished {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

pub(crate) enum AccountStoreCheckpointWait {
    Completed(Result<AccountStoreCheckpoint, CheckpointReadError>),
    TimedOut(AccountStoreCheckpointRequest),
    Stopped(AccountStoreCheckpointRequest),
}

#[derive(Clone)]
pub(crate) struct AccountStoreCheckpointWake {
    lock: Arc<Mutex<()>>,
    changed: Arc<Condvar>,
}

impl AccountStoreCheckpointWake {
    fn new() -> Self {
        Self {
            lock: Arc::new(Mutex::new(())),
            changed: Arc::new(Condvar::new()),
        }
    }

    pub(crate) fn notify(&self) {
        let _guard = self
            .lock
            .lock()
            .expect("checkpoint wake state must not be poisoned");
        self.changed.notify_all();
    }
}

impl fmt::Debug for AccountStoreCheckpointRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountStoreCheckpointRequest { <redacted> }")
    }
}

/// The backend-owned sending half of one checkpoint read.
pub struct AccountStoreCheckpointResponse {
    sender: Sender<Result<AccountStoreCheckpoint, CheckpointReadError>>,
    cancelled: Arc<AtomicBool>,
    wake: AccountStoreCheckpointWake,
}

impl AccountStoreCheckpointResponse {
    /// Returns whether the runtime timed out or otherwise abandoned this read.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Sends the read result if the receiving channel remains open.
    ///
    /// A `true` result means only that delivery succeeded. The runtime still
    /// discards a result when cancellation won the checkpoint deadline.
    pub fn complete(self, result: Result<AccountStoreCheckpoint, CheckpointReadError>) -> bool {
        let delivered = self.sender.send(result).is_ok();
        self.wake.notify();
        delivered
    }
}

impl Drop for AccountStoreCheckpointResponse {
    fn drop(&mut self) {
        self.wake.notify();
    }
}

impl fmt::Debug for AccountStoreCheckpointResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountStoreCheckpointResponse { <redacted> }")
    }
}

/// A safe category for an external checkpoint read failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointReadError {
    /// The external authority could not be reached.
    Unavailable,
    /// The named checkpoint has not been provisioned.
    Missing,
    /// The authority returned malformed checkpoint bytes.
    Invalid,
    /// The authority did not complete the read before the configured deadline.
    TimedOut,
}

impl fmt::Display for CheckpointReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("account checkpoint authority unavailable"),
            Self::Missing => f.write_str("account checkpoint is missing"),
            Self::Invalid => f.write_str("account checkpoint is invalid"),
            Self::TimedOut => f.write_str("account checkpoint read timed out"),
        }
    }
}

impl Error for CheckpointReadError {}

/// The bounded resource controls used by the runtime owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLimits {
    max_connections: usize,
    max_admissions: usize,
    max_write_bytes: usize,
    max_write_frames: usize,
}

impl RuntimeLimits {
    /// Creates non-zero limits within the fixed server bounds.
    pub fn new(
        max_connections: usize,
        max_admissions: usize,
        max_write_bytes: usize,
        max_write_frames: usize,
    ) -> Result<Self, RuntimeConfigError> {
        check_limit(
            RuntimeLimitKind::Connections,
            max_connections,
            MAX_CONNECTION_LIMIT,
        )?;
        check_limit(
            RuntimeLimitKind::Admissions,
            max_admissions,
            MAX_ADMISSION_LIMIT,
        )?;
        check_limit(
            RuntimeLimitKind::WriteBytes,
            max_write_bytes,
            MAX_WRITE_LIMIT,
        )?;
        if max_write_bytes < MIN_WRITE_LIMIT {
            return Err(RuntimeConfigError::WriteLimitTooSmall);
        }
        check_limit(
            RuntimeLimitKind::WriteFrames,
            max_write_frames,
            MAX_WRITE_FRAME_LIMIT,
        )?;
        if max_admissions > max_connections {
            return Err(RuntimeConfigError::AdmissionsExceedConnections);
        }
        Ok(Self {
            max_connections,
            max_admissions,
            max_write_bytes,
            max_write_frames,
        })
    }

    pub const fn max_connections(self) -> usize {
        self.max_connections
    }

    pub const fn max_admissions(self) -> usize {
        self.max_admissions
    }

    pub const fn max_write_bytes(self) -> usize {
        self.max_write_bytes
    }

    pub const fn max_write_frames(self) -> usize {
        self.max_write_frames
    }
}

/// Timeouts owned by the runtime transport and connection lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeTimeouts {
    checkpoint: Duration,
    tls: Duration,
    authentication: Duration,
    idle: Duration,
    query: Duration,
    write: Duration,
    shutdown: Duration,
}

impl RuntimeTimeouts {
    /// Creates the required non-zero lifecycle timeouts with the default query
    /// timeout. The stored idle timeout is rounded up to whole seconds because
    /// MySQL exposes `wait_timeout` as an integer and the runtime uses this
    /// stored value for its actual idle deadline.
    pub fn new(
        checkpoint: Duration,
        tls: Duration,
        authentication: Duration,
        idle: Duration,
        write: Duration,
        shutdown: Duration,
    ) -> Result<Self, RuntimeConfigError> {
        check_timeout(RuntimeTimeoutKind::Checkpoint, checkpoint)?;
        check_timeout(RuntimeTimeoutKind::Tls, tls)?;
        check_timeout(RuntimeTimeoutKind::Authentication, authentication)?;
        check_timeout(RuntimeTimeoutKind::Idle, idle)?;
        check_timeout(RuntimeTimeoutKind::Write, write)?;
        check_timeout(RuntimeTimeoutKind::Shutdown, shutdown)?;
        Ok(Self {
            checkpoint,
            tls,
            authentication,
            idle: whole_second_timeout(idle),
            query: DEFAULT_QUERY_TIMEOUT,
            write,
            shutdown,
        })
    }

    /// Replaces the default deadline for each checked query.
    pub fn with_query_timeout(mut self, query: Duration) -> Result<Self, RuntimeConfigError> {
        check_timeout(RuntimeTimeoutKind::Query, query)?;
        self.query = query;
        Ok(self)
    }

    pub const fn checkpoint(self) -> Duration {
        self.checkpoint
    }

    pub const fn tls(self) -> Duration {
        self.tls
    }

    pub const fn authentication(self) -> Duration {
        self.authentication
    }

    pub const fn idle(self) -> Duration {
        self.idle
    }

    pub const fn query(self) -> Duration {
        self.query
    }

    pub const fn write(self) -> Duration {
        self.write
    }

    pub const fn shutdown(self) -> Duration {
        self.shutdown
    }
}

/// Side-effect-free configuration for a MySQL server runtime.
///
/// Validation proves only lexical and numeric rules. The runtime must still
/// inject an [`AccountStoreCheckpointReader`], open and inspect every path,
/// validate TLS material, verify Unix peers, and bind listeners in that order.
#[derive(Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    tcp: Option<TcpConfig>,
    unix_socket: Option<UnixSocketConfig>,
    data_root: PathBuf,
    account_root: PathBuf,
    checkpoint_authority: CheckpointAuthorityId,
    reload_interval: Duration,
    limits: RuntimeLimits,
    timeouts: RuntimeTimeouts,
    max_prepared_statement_count: usize,
}

impl RuntimeConfig {
    /// Validates configuration without opening files or starting a listener.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tcp: Option<TcpConfig>,
        unix_socket: Option<UnixSocketConfig>,
        data_root: impl AsRef<Path>,
        account_root: impl AsRef<Path>,
        checkpoint_authority: CheckpointAuthorityId,
        reload_interval: Duration,
        limits: RuntimeLimits,
        timeouts: RuntimeTimeouts,
    ) -> Result<Self, RuntimeConfigError> {
        let config = Self {
            tcp,
            unix_socket,
            data_root: absolute_path(data_root.as_ref(), PathField::DataRoot)?,
            account_root: absolute_path(account_root.as_ref(), PathField::AccountRoot)?,
            checkpoint_authority,
            reload_interval,
            limits,
            timeouts,
            max_prepared_statement_count: DEFAULT_MAX_PREPARED_STMT_COUNT,
        };
        config.validate()?;
        Ok(config)
    }

    /// Sets the maximum number of prepared statements retained by this runtime.
    ///
    /// Zero disables prepared statements. The value is shared by all sessions
    /// created by the runtime owner.
    pub fn with_max_prepared_statement_count(
        mut self,
        maximum: usize,
    ) -> Result<Self, RuntimeConfigError> {
        validate_prepared_statement_count(maximum)?;
        self.max_prepared_statement_count = maximum;
        Ok(self)
    }

    /// Rechecks cross-field invariants without performing runtime I/O.
    pub fn validate(&self) -> Result<(), RuntimeConfigError> {
        if self.tcp.is_none() && self.unix_socket.is_none() {
            return Err(RuntimeConfigError::NoListener);
        }
        if self.tcp.is_some() && self.unix_socket.is_some() {
            return Err(RuntimeConfigError::ListenersMutuallyExclusive);
        }
        if same_path(&self.data_root, &self.account_root) {
            return Err(RuntimeConfigError::DataAndAccountRootsEqual);
        }
        if let Some(socket) = &self.unix_socket {
            let socket_path = socket.socket_path();
            if same_or_descendant(socket.directory(), &self.data_root)
                || same_or_descendant(&socket_path, &self.data_root)
            {
                return Err(RuntimeConfigError::UnixSocketCollidesWithDataRoot);
            }
            if same_or_descendant(socket.directory(), &self.account_root)
                || same_or_descendant(&socket_path, &self.account_root)
            {
                return Err(RuntimeConfigError::UnixSocketCollidesWithAccountRoot);
            }
        }
        if !(MIN_RELOAD_INTERVAL..=MAX_RELOAD_INTERVAL).contains(&self.reload_interval) {
            return Err(RuntimeConfigError::ReloadIntervalOutOfRange);
        }
        validate_prepared_statement_count(self.max_prepared_statement_count)?;
        Ok(())
    }

    pub fn tcp(&self) -> Option<&TcpConfig> {
        self.tcp.as_ref()
    }

    pub fn unix_socket(&self) -> Option<&UnixSocketConfig> {
        self.unix_socket.as_ref()
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn account_root(&self) -> &Path {
        &self.account_root
    }

    pub fn checkpoint_authority(&self) -> &CheckpointAuthorityId {
        &self.checkpoint_authority
    }

    pub const fn reload_interval(&self) -> Duration {
        self.reload_interval
    }

    pub const fn limits(&self) -> RuntimeLimits {
        self.limits
    }

    pub const fn timeouts(&self) -> RuntimeTimeouts {
        self.timeouts
    }

    /// Returns the maximum number of prepared statements retained by this runtime.
    pub const fn max_prepared_statement_count(&self) -> usize {
        self.max_prepared_statement_count
    }
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("tcp", &self.tcp)
            .field("unix_socket", &self.unix_socket)
            .field("data_root", &"<redacted>")
            .field("account_root", &"<redacted>")
            .field("checkpoint_authority", &self.checkpoint_authority)
            .field("reload_interval", &self.reload_interval)
            .field("limits", &self.limits)
            .field("timeouts", &self.timeouts)
            .field(
                "max_prepared_statement_count",
                &self.max_prepared_statement_count,
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLimitKind {
    Connections,
    Admissions,
    WriteBytes,
    WriteFrames,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTimeoutKind {
    Checkpoint,
    Tls,
    Authentication,
    Idle,
    Query,
    Write,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathField {
    TlsCertificate,
    TlsPrivateKey,
    UnixSocketDirectory,
    DataRoot,
    AccountRoot,
}

/// A rejected runtime configuration. Sensitive identifiers are never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeConfigError {
    NoListener,
    ListenersMutuallyExclusive,
    TlsCertificatePathNotAbsolute,
    TlsCertificatePathContainsNul,
    TlsPrivateKeyPathNotAbsolute,
    TlsPrivateKeyPathContainsNul,
    TlsCertificateAndKeyPathsEqual,
    UnixSocketDirectoryNotAbsolute,
    UnixSocketDirectoryContainsNul,
    UnixSocketFilenameEmpty,
    UnixSocketFilenameNotSimple,
    UnixSocketPathTooLong,
    DataRootNotAbsolute,
    DataRootContainsNul,
    AccountRootNotAbsolute,
    AccountRootContainsNul,
    DataAndAccountRootsEqual,
    UnixSocketCollidesWithDataRoot,
    UnixSocketCollidesWithAccountRoot,
    CheckpointAuthorityIdEmpty,
    CheckpointAuthorityIdTooLong,
    CheckpointAuthorityIdContainsNul,
    CheckpointAuthorityIdLooksLikePath,
    ReloadIntervalOutOfRange,
    PreparedStatementCountTooLarge {
        value: usize,
        maximum: usize,
    },
    ZeroLimit {
        kind: RuntimeLimitKind,
    },
    LimitTooLarge {
        kind: RuntimeLimitKind,
        value: usize,
        maximum: usize,
    },
    AdmissionsExceedConnections,
    WriteLimitTooSmall,
    ZeroTimeout {
        kind: RuntimeTimeoutKind,
    },
    TimeoutTooLarge {
        kind: RuntimeTimeoutKind,
    },
}

impl fmt::Display for RuntimeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoListener => f.write_str("one MySQL listener is required"),
            Self::ListenersMutuallyExclusive => {
                f.write_str("TCP and Unix MySQL listeners are mutually exclusive")
            }
            Self::TlsCertificatePathNotAbsolute => {
                f.write_str("TLS certificate path must be absolute")
            }
            Self::TlsCertificatePathContainsNul => f.write_str("TLS certificate path contains NUL"),
            Self::TlsPrivateKeyPathNotAbsolute => {
                f.write_str("TLS private-key path must be absolute")
            }
            Self::TlsPrivateKeyPathContainsNul => f.write_str("TLS private-key path contains NUL"),
            Self::TlsCertificateAndKeyPathsEqual => {
                f.write_str("TLS certificate and private-key paths must differ")
            }
            Self::UnixSocketDirectoryNotAbsolute => {
                f.write_str("Unix socket directory must be absolute")
            }
            Self::UnixSocketDirectoryContainsNul => {
                f.write_str("Unix socket directory contains NUL")
            }
            Self::UnixSocketFilenameEmpty => f.write_str("Unix socket filename must not be empty"),
            Self::UnixSocketFilenameNotSimple => {
                f.write_str("Unix socket filename must be a simple filename")
            }
            Self::UnixSocketPathTooLong => write!(
                f,
                "Unix socket path exceeds the {MAX_UNIX_SOCKET_PATH_BYTES}-byte platform-safe maximum"
            ),
            Self::DataRootNotAbsolute => f.write_str("data root must be absolute"),
            Self::DataRootContainsNul => f.write_str("data root contains NUL"),
            Self::AccountRootNotAbsolute => f.write_str("account root must be absolute"),
            Self::AccountRootContainsNul => f.write_str("account root contains NUL"),
            Self::DataAndAccountRootsEqual => f.write_str("data root and account root must differ"),
            Self::UnixSocketCollidesWithDataRoot => {
                f.write_str("Unix socket path collides with data root")
            }
            Self::UnixSocketCollidesWithAccountRoot => {
                f.write_str("Unix socket path collides with account root")
            }
            Self::CheckpointAuthorityIdEmpty => {
                f.write_str("checkpoint authority identifier must not be empty")
            }
            Self::CheckpointAuthorityIdTooLong => {
                f.write_str("checkpoint authority identifier is too long")
            }
            Self::CheckpointAuthorityIdContainsNul => {
                f.write_str("checkpoint authority identifier contains NUL")
            }
            Self::CheckpointAuthorityIdLooksLikePath => {
                f.write_str("checkpoint authority identifier must not be a local path")
            }
            Self::ReloadIntervalOutOfRange => {
                f.write_str("reload interval must be between 1 and 60 seconds")
            }
            Self::PreparedStatementCountTooLarge { value, maximum } => write!(
                f,
                "prepared statement count {value} exceeds maximum {maximum}"
            ),
            Self::ZeroLimit { kind } => write!(f, "{kind:?} limit must be non-zero"),
            Self::LimitTooLarge {
                kind,
                value,
                maximum,
            } => {
                write!(f, "{kind:?} limit {value} exceeds maximum {maximum}")
            }
            Self::AdmissionsExceedConnections => {
                f.write_str("admission limit cannot exceed connection limit")
            }
            Self::WriteLimitTooSmall => {
                f.write_str("write-byte limit cannot retain one maximum initial handshake")
            }
            Self::ZeroTimeout { kind } => write!(f, "{kind:?} timeout must be non-zero"),
            Self::TimeoutTooLarge { kind } => {
                write!(f, "{kind:?} timeout exceeds the 24 hour maximum")
            }
        }
    }
}

impl Error for RuntimeConfigError {}

fn absolute_path(path: &Path, field: PathField) -> Result<PathBuf, RuntimeConfigError> {
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(match field {
            PathField::TlsCertificate => RuntimeConfigError::TlsCertificatePathContainsNul,
            PathField::TlsPrivateKey => RuntimeConfigError::TlsPrivateKeyPathContainsNul,
            PathField::UnixSocketDirectory => RuntimeConfigError::UnixSocketDirectoryContainsNul,
            PathField::DataRoot => RuntimeConfigError::DataRootContainsNul,
            PathField::AccountRoot => RuntimeConfigError::AccountRootContainsNul,
        });
    }
    if !path.is_absolute() {
        return Err(match field {
            PathField::TlsCertificate => RuntimeConfigError::TlsCertificatePathNotAbsolute,
            PathField::TlsPrivateKey => RuntimeConfigError::TlsPrivateKeyPathNotAbsolute,
            PathField::UnixSocketDirectory => RuntimeConfigError::UnixSocketDirectoryNotAbsolute,
            PathField::DataRoot => RuntimeConfigError::DataRootNotAbsolute,
            PathField::AccountRoot => RuntimeConfigError::AccountRootNotAbsolute,
        });
    }
    Ok(path.to_owned())
}

fn simple_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= MAX_SOCKET_FILENAME_BYTES
        && filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && filename != "."
        && filename != ".."
}

fn unix_socket_path_within_limit(directory: &Path, filename: &str) -> bool {
    directory.join(filename).as_os_str().as_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES
}

fn same_path(left: &Path, right: &Path) -> bool {
    normalize_path(left) == normalize_path(right)
}

fn same_or_descendant(path: &Path, root: &Path) -> bool {
    normalize_path(path).starts_with(normalize_path(root))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            Component::Normal(component) => components.push(component.to_owned()),
            Component::Prefix(component) => components.push(component.as_os_str().to_owned()),
        }
    }
    let mut normalized = PathBuf::from("/");
    for component in components {
        normalized.push(component);
    }
    normalized
}

fn check_limit(
    kind: RuntimeLimitKind,
    value: usize,
    maximum: usize,
) -> Result<(), RuntimeConfigError> {
    if value == 0 {
        return Err(RuntimeConfigError::ZeroLimit { kind });
    }
    if value > maximum {
        return Err(RuntimeConfigError::LimitTooLarge {
            kind,
            value,
            maximum,
        });
    }
    Ok(())
}

fn validate_prepared_statement_count(value: usize) -> Result<(), RuntimeConfigError> {
    if value > MAX_PREPARED_STMT_COUNT {
        return Err(RuntimeConfigError::PreparedStatementCountTooLarge {
            value,
            maximum: MAX_PREPARED_STMT_COUNT,
        });
    }
    Ok(())
}

fn check_timeout(kind: RuntimeTimeoutKind, timeout: Duration) -> Result<(), RuntimeConfigError> {
    if timeout.is_zero() {
        return Err(RuntimeConfigError::ZeroTimeout { kind });
    }
    if timeout > MAX_RUNTIME_TIMEOUT {
        return Err(RuntimeConfigError::TimeoutTooLarge { kind });
    }
    Ok(())
}

fn whole_second_timeout(timeout: Duration) -> Duration {
    let seconds = timeout
        .as_secs()
        .checked_add(u64::from(timeout.subsec_nanos() != 0))
        .expect("runtime timeout seconds must fit in u64");
    Duration::from_secs(seconds.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_tls() -> TlsConfig {
        TlsConfig::new("/etc/turso/server.crt", "/etc/turso/server.key").unwrap()
    }

    fn valid_config() -> RuntimeConfig {
        RuntimeConfig::new(
            None,
            Some(UnixSocketConfig::new("/run/turso", "mysql.sock").unwrap()),
            "/var/lib/turso/data",
            "/var/lib/turso/accounts",
            CheckpointAuthorityId::new("control-plane:accounts").unwrap(),
            Duration::from_secs(5),
            RuntimeLimits::new(100, 100, 1024 * 1024, 64).unwrap(),
            RuntimeTimeouts::new(
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(60),
                Duration::from_secs(5),
                Duration::from_secs(5),
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn valid_config_exposes_only_validated_values() {
        let config = valid_config();
        assert_eq!(
            config.unix_socket().unwrap().policy(),
            UnixSocketPolicy::SameEffectiveUid
        );
        assert_eq!(
            config.checkpoint_authority().as_str(),
            "control-plane:accounts"
        );
        assert_eq!(config.timeouts().query(), DEFAULT_QUERY_TIMEOUT);
        assert_eq!(
            config.max_prepared_statement_count(),
            DEFAULT_MAX_PREPARED_STMT_COUNT
        );
        let debug = format!("{config:?}");
        for private in [
            "/var/lib/turso/data",
            "/var/lib/turso/accounts",
            "/run/turso",
            "mysql.sock",
            "control-plane:accounts",
        ] {
            assert!(!debug.contains(private));
        }
    }

    #[test]
    fn tls_paths_are_absolute_distinct_and_redacted() {
        assert_eq!(
            TlsConfig::new("server.crt", "/server.key"),
            Err(RuntimeConfigError::TlsCertificatePathNotAbsolute)
        );
        assert_eq!(
            TlsConfig::new("/server.crt", "/server.crt"),
            Err(RuntimeConfigError::TlsCertificateAndKeyPathsEqual)
        );
        let tls = valid_tls();
        let debug = format!("{tls:?}");
        assert!(!debug.contains("server.crt"));
        assert!(!debug.contains("server.key"));
    }

    #[test]
    fn unix_socket_path_uses_the_platform_safe_byte_limit() {
        let filename_at_limit = "a".repeat(100);
        let filename_over_limit = "a".repeat(101);

        assert_eq!(
            "/d".len() + 1 + filename_at_limit.len(),
            MAX_UNIX_SOCKET_PATH_BYTES
        );
        assert!(UnixSocketConfig::new("/d", filename_at_limit).is_ok());
        assert_eq!(
            UnixSocketConfig::new("/d", filename_over_limit),
            Err(RuntimeConfigError::UnixSocketPathTooLong)
        );
    }

    #[test]
    fn roots_and_socket_path_cannot_collide() {
        let socket = UnixSocketConfig::new("/srv", "data").unwrap();
        let error = RuntimeConfig::new(
            None,
            Some(socket),
            "/srv/data",
            "/srv/accounts",
            CheckpointAuthorityId::new("authority-1").unwrap(),
            Duration::from_secs(1),
            RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT, 1).unwrap(),
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap(),
        );
        assert_eq!(
            error,
            Err(RuntimeConfigError::UnixSocketCollidesWithDataRoot)
        );
        let nested_socket = UnixSocketConfig::new("/srv/accounts/run", "mysql.sock").unwrap();
        assert_eq!(
            RuntimeConfig::new(
                None,
                Some(nested_socket),
                "/srv/data",
                "/srv/accounts",
                CheckpointAuthorityId::new("authority-1").unwrap(),
                Duration::from_secs(1),
                RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT, 1).unwrap(),
                RuntimeTimeouts::new(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .unwrap()
            ),
            Err(RuntimeConfigError::UnixSocketCollidesWithAccountRoot)
        );
        assert_eq!(
            RuntimeConfig::new(
                Some(TcpConfig::new(
                    SocketAddr::from(([127, 0, 0, 1], 3306)),
                    valid_tls(),
                )),
                None,
                "/srv/root",
                "/srv/./root",
                CheckpointAuthorityId::new("authority-1").unwrap(),
                Duration::from_secs(1),
                RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT, 1).unwrap(),
                RuntimeTimeouts::new(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .unwrap()
            ),
            Err(RuntimeConfigError::DataAndAccountRootsEqual)
        );
    }

    #[test]
    fn tcp_and_unix_listeners_cannot_be_enabled_together() {
        let error = RuntimeConfig::new(
            Some(TcpConfig::new(
                SocketAddr::from(([127, 0, 0, 1], 3306)),
                valid_tls(),
            )),
            Some(UnixSocketConfig::new("/run/turso", "mysql.sock").unwrap()),
            "/var/lib/turso/data",
            "/var/lib/turso/accounts",
            CheckpointAuthorityId::new("authority-1").unwrap(),
            Duration::from_secs(1),
            RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT, 1).unwrap(),
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap(),
        );
        assert_eq!(error, Err(RuntimeConfigError::ListenersMutuallyExclusive));
    }

    #[test]
    fn checkpoint_and_reload_rules_do_not_leak_identifiers() {
        assert_eq!(
            CheckpointAuthorityId::new("/var/lib/checkpoint"),
            Err(RuntimeConfigError::CheckpointAuthorityIdLooksLikePath)
        );
        let identifier = CheckpointAuthorityId::new("authority-secret").unwrap();
        assert!(!format!("{identifier:?}").contains("authority-secret"));
        assert_eq!(valid_config().validate(), Ok(()));
        assert_eq!(
            RuntimeConfig::new(
                Some(TcpConfig::new(
                    SocketAddr::from(([127, 0, 0, 1], 3306)),
                    valid_tls(),
                )),
                None,
                "/data",
                "/accounts",
                identifier,
                Duration::from_millis(999),
                RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT, 1).unwrap(),
                RuntimeTimeouts::new(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .unwrap()
            ),
            Err(RuntimeConfigError::ReloadIntervalOutOfRange)
        );
    }

    #[test]
    fn checkpoint_request_cancellation_is_explicit_and_redacted() {
        let (response, request) = AccountStoreCheckpointRequest::channel();
        assert!(!response.is_cancelled());
        assert!(!format!("{request:?}").contains("Receiver"));
        assert!(!format!("{response:?}").contains("Sender"));

        drop(request);

        assert!(response.is_cancelled());
        assert!(!response.complete(Err(CheckpointReadError::Unavailable)));
    }

    #[test]
    fn limits_and_timeouts_are_nonzero_and_bounded() {
        let rounded_idle = RuntimeTimeouts::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(1500),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(rounded_idle.idle(), Duration::from_secs(2));

        assert_eq!(
            RuntimeConfig::new(
                None,
                None,
                "/data",
                "/accounts",
                CheckpointAuthorityId::new("authority-1").unwrap(),
                Duration::from_secs(1),
                RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT, 1).unwrap(),
                RuntimeTimeouts::new(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .unwrap()
            ),
            Err(RuntimeConfigError::NoListener)
        );
        assert!(matches!(
            RuntimeLimits::new(0, 1, MIN_WRITE_LIMIT, 1),
            Err(RuntimeConfigError::ZeroLimit {
                kind: RuntimeLimitKind::Connections
            })
        ));
        assert!(matches!(
            RuntimeLimits::new(MAX_CONNECTION_LIMIT + 1, 1, MIN_WRITE_LIMIT, 1),
            Err(RuntimeConfigError::LimitTooLarge {
                kind: RuntimeLimitKind::Connections,
                ..
            })
        ));
        assert_eq!(
            RuntimeLimits::new(1, 2, MIN_WRITE_LIMIT, 1),
            Err(RuntimeConfigError::AdmissionsExceedConnections)
        );
        assert_eq!(
            RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT - 1, 1),
            Err(RuntimeConfigError::WriteLimitTooSmall)
        );
        assert!(matches!(
            RuntimeLimits::new(1, 1, MIN_WRITE_LIMIT, MAX_WRITE_FRAME_LIMIT + 1),
            Err(RuntimeConfigError::LimitTooLarge {
                kind: RuntimeLimitKind::WriteFrames,
                ..
            })
        ));
        assert!(matches!(
            RuntimeTimeouts::new(
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(RuntimeConfigError::ZeroTimeout {
                kind: RuntimeTimeoutKind::Checkpoint
            })
        ));
        assert!(matches!(
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(RuntimeConfigError::ZeroTimeout {
                kind: RuntimeTimeoutKind::Tls
            })
        ));
        assert!(matches!(
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                MAX_RUNTIME_TIMEOUT + Duration::from_nanos(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(RuntimeConfigError::TimeoutTooLarge {
                kind: RuntimeTimeoutKind::Tls
            })
        ));
        assert!(matches!(
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap()
            .with_query_timeout(Duration::ZERO),
            Err(RuntimeConfigError::ZeroTimeout {
                kind: RuntimeTimeoutKind::Query
            })
        ));
        assert!(matches!(
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap()
            .with_query_timeout(MAX_RUNTIME_TIMEOUT + Duration::from_nanos(1)),
            Err(RuntimeConfigError::TimeoutTooLarge {
                kind: RuntimeTimeoutKind::Query
            })
        ));
    }

    #[test]
    fn prepared_statement_count_accepts_mysql_boundaries_and_rejects_overflow() {
        for maximum in [0, DEFAULT_MAX_PREPARED_STMT_COUNT, MAX_PREPARED_STMT_COUNT] {
            let config = valid_config()
                .with_max_prepared_statement_count(maximum)
                .expect("MySQL prepared statement boundary should be valid");
            assert_eq!(config.max_prepared_statement_count(), maximum);
            assert_eq!(config.validate(), Ok(()));
        }

        assert_eq!(
            valid_config().with_max_prepared_statement_count(MAX_PREPARED_STMT_COUNT + 1),
            Err(RuntimeConfigError::PreparedStatementCountTooLarge {
                value: MAX_PREPARED_STMT_COUNT + 1,
                maximum: MAX_PREPARED_STMT_COUNT,
            })
        );
    }
}
