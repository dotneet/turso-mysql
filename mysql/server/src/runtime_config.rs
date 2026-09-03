//! Unix-only, side-effect-free validation for the MySQL server runtime.
//!
//! This module deliberately does not open listeners, inspect permissions, read
//! certificates, or talk to a checkpoint service. Those operations belong to a
//! runtime owner which must enforce the contracts represented here.

use std::{
    error::Error,
    fmt,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use std::os::unix::ffi::OsStrExt;

use crate::{AccountStoreCheckpoint, MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH, PACKET_HEADER_LEN};

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

/// Reads the exact account checkpoint authorized by the external control plane.
///
/// The runtime must successfully read this value and open the matching account
/// generation before binding a listener. It must read again before each reload;
/// the redacted identifier alone grants no authority.
pub trait AccountStoreCheckpointReader: Send + Sync {
    /// Reads one exact checkpoint without exposing backend-specific failures.
    fn read_checkpoint(
        &self,
        authority: &CheckpointAuthorityId,
    ) -> Result<AccountStoreCheckpoint, CheckpointReadError>;
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
}

impl fmt::Display for CheckpointReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("account checkpoint authority unavailable"),
            Self::Missing => f.write_str("account checkpoint is missing"),
            Self::Invalid => f.write_str("account checkpoint is invalid"),
        }
    }
}

impl Error for CheckpointReadError {}

/// The three bounded resource controls used by the runtime owner.
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
    tls: Duration,
    authentication: Duration,
    idle: Duration,
    write: Duration,
    shutdown: Duration,
}

impl RuntimeTimeouts {
    /// Creates the required non-zero lifecycle timeouts.
    pub fn new(
        tls: Duration,
        authentication: Duration,
        idle: Duration,
        write: Duration,
        shutdown: Duration,
    ) -> Result<Self, RuntimeConfigError> {
        check_timeout(RuntimeTimeoutKind::Tls, tls)?;
        check_timeout(RuntimeTimeoutKind::Authentication, authentication)?;
        check_timeout(RuntimeTimeoutKind::Idle, idle)?;
        check_timeout(RuntimeTimeoutKind::Write, write)?;
        check_timeout(RuntimeTimeoutKind::Shutdown, shutdown)?;
        Ok(Self {
            tls,
            authentication,
            idle,
            write,
            shutdown,
        })
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

    pub const fn write(self) -> Duration {
        self.write
    }

    pub const fn shutdown(self) -> Duration {
        self.shutdown
    }
}

/// Side-effect-free configuration for a future Unix MySQL server runtime.
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
        };
        config.validate()?;
        Ok(config)
    }

    /// Rechecks cross-field invariants without performing runtime I/O.
    pub fn validate(&self) -> Result<(), RuntimeConfigError> {
        if self.tcp.is_none() && self.unix_socket.is_none() {
            return Err(RuntimeConfigError::NoListener);
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
    Tls,
    Authentication,
    Idle,
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
    TlsCertificatePathNotAbsolute,
    TlsCertificatePathContainsNul,
    TlsPrivateKeyPathNotAbsolute,
    TlsPrivateKeyPathContainsNul,
    TlsCertificateAndKeyPathsEqual,
    UnixSocketDirectoryNotAbsolute,
    UnixSocketDirectoryContainsNul,
    UnixSocketFilenameEmpty,
    UnixSocketFilenameNotSimple,
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
            Self::NoListener => f.write_str("at least one MySQL listener is required"),
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

fn check_timeout(kind: RuntimeTimeoutKind, timeout: Duration) -> Result<(), RuntimeConfigError> {
    if timeout.is_zero() {
        return Err(RuntimeConfigError::ZeroTimeout { kind });
    }
    if timeout > MAX_RUNTIME_TIMEOUT {
        return Err(RuntimeConfigError::TimeoutTooLarge { kind });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_tls() -> TlsConfig {
        TlsConfig::new("/etc/turso/server.crt", "/etc/turso/server.key").unwrap()
    }

    fn valid_config() -> RuntimeConfig {
        RuntimeConfig::new(
            Some(TcpConfig::new(
                SocketAddr::from(([127, 0, 0, 1], 3306)),
                valid_tls(),
            )),
            Some(UnixSocketConfig::new("/run/turso", "mysql.sock").unwrap()),
            "/var/lib/turso/data",
            "/var/lib/turso/accounts",
            CheckpointAuthorityId::new("control-plane:accounts").unwrap(),
            Duration::from_secs(5),
            RuntimeLimits::new(100, 100, 1024 * 1024, 64).unwrap(),
            RuntimeTimeouts::new(
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
            config.tcp().unwrap().tls().certificate_path(),
            Path::new("/etc/turso/server.crt")
        );
        assert_eq!(
            config.unix_socket().unwrap().policy(),
            UnixSocketPolicy::SameEffectiveUid
        );
        assert_eq!(
            config.checkpoint_authority().as_str(),
            "control-plane:accounts"
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
                    Duration::from_secs(1)
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
                    Duration::from_secs(1)
                )
                .unwrap()
            ),
            Err(RuntimeConfigError::DataAndAccountRootsEqual)
        );
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
                    Duration::from_secs(1)
                )
                .unwrap()
            ),
            Err(RuntimeConfigError::ReloadIntervalOutOfRange)
        );
    }

    #[test]
    fn limits_and_timeouts_are_nonzero_and_bounded() {
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
                    Duration::from_secs(1)
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
                Duration::from_secs(1)
            ),
            Err(RuntimeConfigError::ZeroTimeout {
                kind: RuntimeTimeoutKind::Tls
            })
        ));
        assert!(matches!(
            RuntimeTimeouts::new(
                MAX_RUNTIME_TIMEOUT + Duration::from_nanos(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1)
            ),
            Err(RuntimeConfigError::TimeoutTooLarge {
                kind: RuntimeTimeoutKind::Tls
            })
        ));
    }
}
