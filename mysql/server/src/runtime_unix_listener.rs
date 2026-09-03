//! Blocking Unix listener ownership for the local MySQL runtime.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    io::{Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use turso_mysql::MySqlDatabaseCatalog;

use crate::unix_peer::{UnixPeerError, UnixPeerVerifier};
use crate::unix_socket_fs::{
    SocketEndpointIdentity, SocketOwnerLock, UnixSocketDirectory, UnixSocketFsError,
};
use crate::{
    AccountStoreCheckpointReader, RuntimeAccountReload, RuntimeAccountStore,
    RuntimeAccountStoreError, RuntimeConfig, RuntimeLimits, RuntimeTimeouts, UnixSocketPolicy,
};

/// A blocking local listener that owns its private socket directory lease.
///
/// This is a transport boundary only. Protocol handling and connection work
/// remain with the caller after [`Self::accept`] returns a stream.
pub struct RuntimeUnixListener {
    control: Arc<RuntimeUnixListenerControl>,
    wake_reader: UnixStream,
    directory: UnixSocketDirectory,
    _data_directory: UnixSocketDirectory,
    _account_directory: UnixSocketDirectory,
    _owner_lock: SocketOwnerLock,
    endpoint_identity: SocketEndpointIdentity,
    peer_verifier: UnixPeerVerifier,
    accounts: Arc<RuntimeAccountStore>,
    catalog: Arc<MySqlDatabaseCatalog>,
    endpoint_name: String,
    limits: RuntimeLimits,
    timeouts: RuntimeTimeouts,
}

impl RuntimeUnixListener {
    /// Opens the local runtime state and binds one Unix listener.
    pub fn bind(
        config: &RuntimeConfig,
        checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
    ) -> Result<Self, RuntimeUnixListenerError> {
        if config.tcp().is_some() {
            return Err(RuntimeUnixListenerError::TcpListenerUnsupported);
        }
        let socket = config
            .unix_socket()
            .ok_or(RuntimeUnixListenerError::UnixSocketRequired)?;
        match socket.policy() {
            UnixSocketPolicy::SameEffectiveUid => {}
        }

        let peer_verifier = UnixPeerVerifier::capture_for_startup().map_err(map_peer_error)?;
        let directory =
            UnixSocketDirectory::open(socket.directory()).map_err(map_socket_filesystem_error)?;
        let owner_lock = directory
            .acquire_owner_lock()
            .map_err(map_socket_filesystem_error)?;
        directory
            .ensure_endpoint_absent(socket.filename())
            .map_err(map_socket_filesystem_error)?;
        let data_directory = UnixSocketDirectory::open(config.data_root())
            .map_err(|_| RuntimeUnixListenerError::DataRootUnavailable)?;
        let account_directory = UnixSocketDirectory::open(config.account_root())
            .map_err(|_| RuntimeUnixListenerError::AccountRootUnavailable)?;
        if directory
            .same_directory(&data_directory)
            .map_err(map_socket_filesystem_error)?
            || directory
                .same_directory(&account_directory)
                .map_err(map_socket_filesystem_error)?
            || data_directory
                .same_directory(&account_directory)
                .map_err(map_socket_filesystem_error)?
        {
            return Err(RuntimeUnixListenerError::ProtectedRootsCollide);
        }
        let accounts = Arc::new(
            RuntimeAccountStore::open(config, checkpoint_reader)
                .map_err(RuntimeUnixListenerError::AccountStore)?,
        );
        let catalog = MySqlDatabaseCatalog::open(config.data_root())
            .map_err(|_| RuntimeUnixListenerError::CatalogUnavailable)?;

        directory
            .revalidate()
            .map_err(map_socket_filesystem_error)?;
        directory
            .path_still_resolves_to_self(socket.directory())
            .map_err(map_socket_filesystem_error)?;
        data_directory
            .path_still_resolves_to_self(config.data_root())
            .map_err(|_| RuntimeUnixListenerError::DataRootUnavailable)?;
        account_directory
            .path_still_resolves_to_self(config.account_root())
            .map_err(|_| RuntimeUnixListenerError::AccountRootUnavailable)?;
        directory
            .ensure_endpoint_absent(socket.filename())
            .map_err(map_socket_filesystem_error)?;
        match accounts.reload_once() {
            RuntimeAccountReload::Healthy(_) => {}
            RuntimeAccountReload::Degraded(error) => {
                return Err(RuntimeUnixListenerError::AccountStore(error));
            }
        }

        let (wake_reader, wake_writer) =
            UnixStream::pair().map_err(|_| RuntimeUnixListenerError::WakeUnavailable)?;
        let listener = UnixListener::bind(socket.socket_path())
            .map_err(|_| RuntimeUnixListenerError::BindUnavailable)?;
        let endpoint_identity = match directory.endpoint_identity(socket.filename()) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                return Err(recover_unpublished_endpoint(
                    &directory,
                    socket.filename(),
                    RuntimeUnixListenerError::SocketInvalidEntry,
                ));
            }
            Err(error) => {
                return Err(recover_unpublished_endpoint(
                    &directory,
                    socket.filename(),
                    map_socket_filesystem_error(error),
                ));
            }
        };
        let mut cleanup = EndpointCleanup::new(&directory, socket.filename(), endpoint_identity);
        if listener.set_nonblocking(true).is_err() {
            return Err(cleanup.recover(RuntimeUnixListenerError::BindUnavailable));
        }
        if let Err(error) = directory.path_still_resolves_to_self(socket.directory()) {
            return Err(cleanup.recover(map_socket_filesystem_error(error)));
        }
        let configured_identity =
            match directory.set_endpoint_private_mode_and_capture_identity(socket.filename()) {
                Ok(identity) => identity,
                Err(error) => {
                    return Err(cleanup.recover(map_socket_filesystem_error(error)));
                }
            };
        if configured_identity != endpoint_identity {
            return Err(cleanup.recover(RuntimeUnixListenerError::SocketInvalidEntry));
        }
        cleanup.disarm();
        drop(cleanup);

        Ok(Self {
            control: Arc::new(RuntimeUnixListenerControl::new(
                listener,
                wake_writer,
                config.limits(),
            )),
            wake_reader,
            directory,
            _data_directory: data_directory,
            _account_directory: account_directory,
            _owner_lock: owner_lock,
            endpoint_identity,
            peer_verifier,
            accounts,
            catalog,
            endpoint_name: socket.filename().to_owned(),
            limits: config.limits(),
            timeouts: config.timeouts(),
        })
    }

    /// Performs one explicit account-store reload tick.
    pub fn reload_accounts_once(&self) -> RuntimeAccountReload {
        self.accounts.reload_once()
    }

    /// Returns whether the transport may admit a new authentication attempt.
    pub fn is_ready_for_new_connections(&self) -> bool {
        !self.is_shutting_down() && self.accounts.is_ready_for_new_connections()
    }

    /// Returns whether shutdown has begun and no new stream can be returned.
    pub fn is_shutting_down(&self) -> bool {
        self.control.is_shutting_down()
    }

    /// Stops accepting, wakes waiters, drains accepted streams, and reports cleanup.
    pub fn shutdown(&self) -> RuntimeUnixShutdownReport {
        let deadline = Instant::now() + self.timeouts.shutdown();
        let start = match self.control.begin_shutdown() {
            ShutdownStart::Owner(start) => start,
            ShutdownStart::Wait => return self.control.wait_for_shutdown(),
            ShutdownStart::Finished(report) => return report,
        };
        drop(start.listener);
        drop(start.wake_writer);

        let report = self.control.wait_for_drain(
            deadline,
            start.connections_at_start,
            start.admissions_at_start,
            start.streams_signalled,
        );
        let report = RuntimeUnixShutdownReport {
            endpoint_cleanup: self.cleanup_endpoint(),
            ..report
        };
        self.control.finish_shutdown(report.clone());
        report
    }

    /// Blocks until one same-effective-UID client is accepted or rejected.
    pub(crate) fn accept(&self) -> Result<AcceptedUnixStream, RuntimeUnixListenerError> {
        let accept_waiter = self.control.start_accept()?;
        if self.is_shutting_down() {
            return Err(RuntimeUnixListenerError::ShuttingDown);
        }
        if !self.accounts.is_ready_for_new_connections() {
            return Err(RuntimeUnixListenerError::AccountNotReady);
        }
        let stream = loop {
            match wait_for_listener_or_shutdown(
                &accept_waiter.listener,
                &self.wake_reader,
                &self.control,
            )? {
                ListenerWait::ShuttingDown => return Err(RuntimeUnixListenerError::ShuttingDown),
                ListenerWait::ListenerReady => {}
            }
            match accept_waiter.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => return Err(RuntimeUnixListenerError::AcceptUnavailable),
            }
        };
        if self.is_shutting_down() {
            return Err(RuntimeUnixListenerError::ShuttingDown);
        }
        self.peer_verifier.verify(&stream).map_err(map_peer_error)?;
        // A reload can mark the store unready while the blocking accept waits.
        if !self.is_ready_for_new_connections() {
            return Err(if self.is_shutting_down() {
                RuntimeUnixListenerError::ShuttingDown
            } else {
                RuntimeUnixListenerError::AccountNotReady
            });
        }
        let permits = ConnectionPermits::acquire(&self.control.permits)
            .map_err(RuntimeUnixListenerError::ConnectionLimit)?;
        stream
            .set_nonblocking(false)
            .map_err(|_| RuntimeUnixListenerError::TransportConfiguration)?;
        stream
            .set_read_timeout(Some(self.timeouts.authentication()))
            .map_err(|_| RuntimeUnixListenerError::TransportConfiguration)?;
        stream
            .set_write_timeout(Some(self.timeouts.write()))
            .map_err(|_| RuntimeUnixListenerError::TransportConfiguration)?;

        let registration = self
            .accounts
            .while_ready_for_new_connection(|| self.control.register_connection(&stream))
            .ok_or(RuntimeUnixListenerError::AccountNotReady)??;
        let authentication_deadline = Instant::now() + self.timeouts.authentication();
        drop(accept_waiter);
        Ok(AcceptedUnixStream {
            stream,
            lease: ConnectionLease {
                permits,
                registration,
            },
            authentication_deadline,
            accounts: Arc::clone(&self.accounts),
            catalog: Arc::clone(&self.catalog),
            limits: self.limits,
            timeouts: self.timeouts,
        })
    }
}

fn recover_unpublished_endpoint(
    directory: &UnixSocketDirectory,
    filename: &str,
    original_error: RuntimeUnixListenerError,
) -> RuntimeUnixListenerError {
    match directory.remove_unpublished_socket(filename) {
        Ok(true) => original_error,
        Ok(false) | Err(_) => RuntimeUnixListenerError::SocketCleanupRequired,
    }
}

impl fmt::Debug for RuntimeUnixListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeUnixListener")
            .field("control", &self.control)
            .field("directory", &self.directory)
            .field("data_directory", &"<retained>")
            .field("account_directory", &"<retained>")
            .field("endpoint_identity", &self.endpoint_identity)
            .field("peer_verifier", &self.peer_verifier)
            .field("accounts", &"<retained>")
            .field("catalog", &"<retained>")
            .finish()
    }
}

impl Drop for RuntimeUnixListener {
    fn drop(&mut self) {
        if let ShutdownStart::Owner(start) = self.control.begin_shutdown() {
            drop(start.listener);
            drop(start.wake_writer);
        }
        let _ = self.cleanup_endpoint();
    }
}

impl RuntimeUnixListener {
    fn cleanup_endpoint(&self) -> RuntimeUnixEndpointCleanup {
        match self
            .directory
            .unlink_endpoint_if_matches(&self.endpoint_name, self.endpoint_identity)
        {
            Ok(true) => RuntimeUnixEndpointCleanup::Removed,
            Ok(false) => RuntimeUnixEndpointCleanup::AlreadyMissingOrReplaced,
            Err(_) => RuntimeUnixEndpointCleanup::Failed,
        }
    }
}

struct EndpointCleanup<'a> {
    directory: &'a UnixSocketDirectory,
    filename: &'a str,
    identity: SocketEndpointIdentity,
    active: bool,
}

impl<'a> EndpointCleanup<'a> {
    fn new(
        directory: &'a UnixSocketDirectory,
        filename: &'a str,
        identity: SocketEndpointIdentity,
    ) -> Self {
        Self {
            directory,
            filename,
            identity,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }

    fn recover(&mut self, original_error: RuntimeUnixListenerError) -> RuntimeUnixListenerError {
        self.active = false;
        match self
            .directory
            .unlink_endpoint_if_matches(self.filename, self.identity)
        {
            Ok(true) => original_error,
            Ok(false) | Err(_) => RuntimeUnixListenerError::SocketCleanupRequired,
        }
    }
}

impl Drop for EndpointCleanup<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self
                .directory
                .unlink_endpoint_if_matches(self.filename, self.identity);
        }
    }
}

/// One accepted local stream with active connection and admission permits.
pub(crate) struct AcceptedUnixStream {
    stream: UnixStream,
    lease: ConnectionLease,
    authentication_deadline: Instant,
    accounts: Arc<RuntimeAccountStore>,
    catalog: Arc<MySqlDatabaseCatalog>,
    limits: RuntimeLimits,
    timeouts: RuntimeTimeouts,
}

impl AcceptedUnixStream {
    /// Marks authentication complete and switches future reads to the idle timeout.
    pub(crate) fn complete_admission(&mut self) -> Result<(), RuntimeUnixListenerError> {
        self.set_read_timeout(self.timeouts.idle())?;
        self.lease.complete_admission()?;
        Ok(())
    }

    /// Returns the nonzero protocol connection ID reserved for this live stream.
    pub(crate) fn connection_id(&self) -> u32 {
        self.lease.connection_id()
    }

    /// Starts one protocol action if shutdown has not started.
    pub(crate) fn begin_protocol_work(&self) -> Result<(), RuntimeUnixListenerError> {
        self.lease.begin_protocol_work()
    }

    /// Clones the account store retained for the protocol owner.
    pub(crate) fn account_store(&self) -> Arc<RuntimeAccountStore> {
        Arc::clone(&self.accounts)
    }

    /// Clones the catalog retained for the protocol owner.
    pub(crate) fn catalog(&self) -> Arc<MySqlDatabaseCatalog> {
        Arc::clone(&self.catalog)
    }

    /// Returns the runtime limits selected when this stream was accepted.
    pub(crate) const fn limits(&self) -> RuntimeLimits {
        self.limits
    }

    /// Returns the runtime timeouts selected when this stream was accepted.
    pub(crate) const fn timeouts(&self) -> RuntimeTimeouts {
        self.timeouts
    }

    /// Returns the fixed deadline for completing authentication.
    pub(crate) const fn authentication_deadline(&self) -> Instant {
        self.authentication_deadline
    }

    /// Applies one blocking read deadline for the protocol owner's current phase.
    pub(crate) fn set_read_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), RuntimeUnixListenerError> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| RuntimeUnixListenerError::TransportConfiguration)
    }

    /// Applies one blocking write deadline for the protocol owner's current phase.
    pub(crate) fn set_write_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), RuntimeUnixListenerError> {
        self.stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| RuntimeUnixListenerError::TransportConfiguration)
    }
}

impl fmt::Debug for AcceptedUnixStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptedUnixStream")
            .field("stream", &"<redacted>")
            .field("admission_complete", &self.lease.admission_complete())
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl Read for AcceptedUnixStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buffer)
    }
}

impl Write for AcceptedUnixStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// The outcome of identity-safe endpoint cleanup during listener shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeUnixEndpointCleanup {
    /// The listener removed the endpoint it originally created.
    Removed,
    /// The endpoint was already gone or no longer had the retained identity.
    AlreadyMissingOrReplaced,
    /// The identity-safe cleanup check or unlink failed.
    Failed,
}

/// The bounded result of one Unix-listener shutdown attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeUnixShutdownReport {
    connections_at_start: usize,
    admissions_at_start: usize,
    streams_signalled: usize,
    remaining_connections: usize,
    remaining_admissions: usize,
    remaining_accept_waiters: usize,
    endpoint_cleanup: RuntimeUnixEndpointCleanup,
}

impl RuntimeUnixShutdownReport {
    /// Returns the number of accepted streams present when shutdown started.
    pub const fn connections_at_start(&self) -> usize {
        self.connections_at_start
    }

    /// Returns the number of streams still authenticating when shutdown started.
    pub const fn admissions_at_start(&self) -> usize {
        self.admissions_at_start
    }

    /// Returns the number of accepted streams signalled with `Shutdown::Both`.
    pub const fn streams_signalled(&self) -> usize {
        self.streams_signalled
    }

    /// Returns the number of streams that outlived the shutdown deadline.
    pub const fn remaining_connections(&self) -> usize {
        self.remaining_connections
    }

    /// Returns the number of admissions that outlived the shutdown deadline.
    pub const fn remaining_admissions(&self) -> usize {
        self.remaining_admissions
    }

    /// Returns the number of accept calls that outlived the shutdown deadline.
    pub const fn remaining_accept_waiters(&self) -> usize {
        self.remaining_accept_waiters
    }

    /// Returns the result of endpoint cleanup.
    pub const fn endpoint_cleanup(&self) -> RuntimeUnixEndpointCleanup {
        self.endpoint_cleanup
    }

    /// Returns whether every accepted stream and accept waiter drained in time.
    pub const fn drained(&self) -> bool {
        self.remaining_connections == 0 && self.remaining_accept_waiters == 0
    }
}

/// A listener or accepted-stream operation failed without exposing paths,
/// checkpoint contents, endpoint identities, or peer credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeUnixListenerError {
    /// TCP support is not part of this Unix-only runtime slice.
    TcpListenerUnsupported,
    /// This runtime requires one Unix socket endpoint.
    UnixSocketRequired,
    /// The operating system did not provide peer credentials.
    PeerCredentialsUnavailable,
    /// The peer did not have the startup effective UID.
    PeerUidMismatch,
    /// The target has no reviewed peer credential implementation.
    PeerCredentialsUnsupported,
    /// The configured socket path was invalid.
    SocketInvalidPath,
    /// The configured socket directory was unsafe.
    SocketInvalidDirectory,
    /// The endpoint or owner-lock entry was unsafe.
    SocketInvalidEntry,
    /// The endpoint already existed.
    SocketEndpointExists,
    /// Another process owns the endpoint lock.
    SocketLockHeld,
    /// The socket filesystem was unavailable.
    SocketFilesystemUnavailable,
    /// Bind failed after creating an endpoint whose cleanup could not be confirmed.
    SocketCleanupRequired,
    /// The data root could not be retained without following symlinks.
    DataRootUnavailable,
    /// The account root could not be retained without following symlinks.
    AccountRootUnavailable,
    /// Two protected runtime roots resolve to the same directory.
    ProtectedRootsCollide,
    /// The externally checkpointed account store was not ready.
    AccountStore(RuntimeAccountStoreError),
    /// The last account-store reload failed, so new authentication is blocked.
    AccountNotReady,
    /// The trusted database catalog could not be opened.
    CatalogUnavailable,
    /// The Unix listener could not be bound.
    BindUnavailable,
    /// The listener could not accept a connection.
    AcceptUnavailable,
    /// Shutdown began before this call could return an accepted stream.
    ShuttingDown,
    /// The listener could not create its private accept wake stream.
    WakeUnavailable,
    /// The runtime could not reserve connection capacity.
    ConnectionLimit(ConnectionLimitError),
    /// A stream timeout could not be configured.
    TransportConfiguration,
}

impl fmt::Display for RuntimeUnixListenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TcpListenerUnsupported => {
                f.write_str("TCP listener is unsupported by this runtime")
            }
            Self::UnixSocketRequired => f.write_str("Unix socket listener is required"),
            Self::PeerCredentialsUnavailable => {
                f.write_str("Unix peer credentials are unavailable")
            }
            Self::PeerUidMismatch => f.write_str("Unix peer effective UID is not allowed"),
            Self::PeerCredentialsUnsupported => {
                f.write_str("Unix peer credentials are unsupported on this platform")
            }
            Self::SocketInvalidPath => f.write_str("Unix socket path is invalid"),
            Self::SocketInvalidDirectory => f.write_str("Unix socket directory is invalid"),
            Self::SocketInvalidEntry => f.write_str("Unix socket entry is invalid"),
            Self::SocketEndpointExists => f.write_str("Unix socket endpoint already exists"),
            Self::SocketLockHeld => f.write_str("Unix socket lock is already held"),
            Self::SocketFilesystemUnavailable => {
                f.write_str("Unix socket filesystem is unavailable")
            }
            Self::SocketCleanupRequired => {
                f.write_str("Unix socket bind cleanup requires operator inspection")
            }
            Self::DataRootUnavailable => f.write_str("MySQL data root is unavailable"),
            Self::AccountRootUnavailable => f.write_str("MySQL account root is unavailable"),
            Self::ProtectedRootsCollide => f.write_str("MySQL runtime roots collide"),
            Self::AccountStore(error) => write!(f, "runtime account store failed: {error}"),
            Self::AccountNotReady => {
                f.write_str("runtime account store is not ready for new connections")
            }
            Self::CatalogUnavailable => f.write_str("MySQL database catalog is unavailable"),
            Self::BindUnavailable => f.write_str("Unix listener bind failed"),
            Self::AcceptUnavailable => f.write_str("Unix listener accept failed"),
            Self::ShuttingDown => f.write_str("Unix listener is shutting down"),
            Self::WakeUnavailable => f.write_str("Unix listener wake stream is unavailable"),
            Self::ConnectionLimit(error) => write!(f, "Unix connection rejected: {error}"),
            Self::TransportConfiguration => f.write_str("Unix stream timeout configuration failed"),
        }
    }
}

impl Error for RuntimeUnixListenerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AccountStore(error) => Some(error),
            Self::ConnectionLimit(error) => Some(error),
            Self::TcpListenerUnsupported
            | Self::UnixSocketRequired
            | Self::PeerCredentialsUnavailable
            | Self::PeerUidMismatch
            | Self::PeerCredentialsUnsupported
            | Self::SocketInvalidPath
            | Self::SocketInvalidDirectory
            | Self::SocketInvalidEntry
            | Self::SocketEndpointExists
            | Self::SocketLockHeld
            | Self::SocketFilesystemUnavailable
            | Self::SocketCleanupRequired
            | Self::DataRootUnavailable
            | Self::AccountRootUnavailable
            | Self::ProtectedRootsCollide
            | Self::AccountNotReady
            | Self::CatalogUnavailable
            | Self::BindUnavailable
            | Self::AcceptUnavailable
            | Self::ShuttingDown
            | Self::WakeUnavailable
            | Self::TransportConfiguration => None,
        }
    }
}

fn map_peer_error(error: UnixPeerError) -> RuntimeUnixListenerError {
    match error {
        UnixPeerError::CredentialsUnavailable => {
            RuntimeUnixListenerError::PeerCredentialsUnavailable
        }
        UnixPeerError::EffectiveUidMismatch => RuntimeUnixListenerError::PeerUidMismatch,
        UnixPeerError::UnsupportedPlatform => RuntimeUnixListenerError::PeerCredentialsUnsupported,
    }
}

fn map_socket_filesystem_error(error: UnixSocketFsError) -> RuntimeUnixListenerError {
    match error {
        UnixSocketFsError::InvalidPath => RuntimeUnixListenerError::SocketInvalidPath,
        UnixSocketFsError::InvalidDirectory => RuntimeUnixListenerError::SocketInvalidDirectory,
        UnixSocketFsError::InvalidEntry => RuntimeUnixListenerError::SocketInvalidEntry,
        UnixSocketFsError::EndpointExists => RuntimeUnixListenerError::SocketEndpointExists,
        UnixSocketFsError::LockHeld => RuntimeUnixListenerError::SocketLockHeld,
        UnixSocketFsError::Backend => RuntimeUnixListenerError::SocketFilesystemUnavailable,
    }
}

struct RuntimeUnixListenerControl {
    state: Mutex<RuntimeUnixListenerState>,
    changed: Condvar,
    permits: Arc<Mutex<PermitState>>,
}

impl RuntimeUnixListenerControl {
    fn new(listener: UnixListener, wake_writer: UnixStream, limits: RuntimeLimits) -> Self {
        Self {
            state: Mutex::new(RuntimeUnixListenerState {
                lifecycle: RuntimeUnixListenerLifecycle::Accepting,
                listener: Some(listener),
                wake_writer: Some(wake_writer),
                accept_waiters: 0,
                next_connection_id: 1,
                connections: BTreeMap::new(),
            }),
            changed: Condvar::new(),
            permits: Arc::new(Mutex::new(PermitState::new(limits))),
        }
    }

    fn is_shutting_down(&self) -> bool {
        !matches!(
            self.lock().lifecycle,
            RuntimeUnixListenerLifecycle::Accepting
        )
    }

    fn start_accept(self: &Arc<Self>) -> Result<AcceptWaiter, RuntimeUnixListenerError> {
        let mut state = self.lock();
        if !matches!(state.lifecycle, RuntimeUnixListenerLifecycle::Accepting) {
            return Err(RuntimeUnixListenerError::ShuttingDown);
        }
        let listener = state
            .listener
            .as_ref()
            .ok_or(RuntimeUnixListenerError::ShuttingDown)?
            .try_clone()
            .map_err(|_| RuntimeUnixListenerError::AcceptUnavailable)?;
        state.accept_waiters += 1;
        self.changed.notify_all();
        Ok(AcceptWaiter {
            control: Arc::clone(self),
            listener,
        })
    }

    fn register_connection(
        self: &Arc<Self>,
        stream: &UnixStream,
    ) -> Result<ConnectionRegistration, RuntimeUnixListenerError> {
        let duplicate = stream
            .try_clone()
            .map_err(|_| RuntimeUnixListenerError::TransportConfiguration)?;
        let mut state = self.lock();
        if !matches!(state.lifecycle, RuntimeUnixListenerLifecycle::Accepting) {
            return Err(RuntimeUnixListenerError::ShuttingDown);
        }
        let id = allocate_protocol_connection_id(&mut state);
        let previous = state.connections.insert(
            id,
            RegisteredConnection {
                stream: duplicate,
                admission_active: true,
            },
        );
        assert!(
            previous.is_none(),
            "fresh Unix connection ID must be unused"
        );
        Ok(ConnectionRegistration {
            control: Arc::clone(self),
            id,
            admission_active: true,
        })
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeUnixListenerError> {
        if matches!(
            self.lock().lifecycle,
            RuntimeUnixListenerLifecycle::Accepting
        ) {
            Ok(())
        } else {
            Err(RuntimeUnixListenerError::ShuttingDown)
        }
    }

    fn begin_shutdown(&self) -> ShutdownStart {
        let mut state = self.lock();
        match &state.lifecycle {
            RuntimeUnixListenerLifecycle::Stopped(report) => {
                ShutdownStart::Finished(report.clone())
            }
            RuntimeUnixListenerLifecycle::Draining => ShutdownStart::Wait,
            RuntimeUnixListenerLifecycle::Accepting => {
                state.lifecycle = RuntimeUnixListenerLifecycle::Draining;
                let connections_at_start = state.connections.len();
                let admissions_at_start = state
                    .connections
                    .values()
                    .filter(|connection| connection.admission_active)
                    .count();
                let mut streams_signalled = 0;
                for connection in state.connections.values() {
                    if connection.stream.shutdown(Shutdown::Both).is_ok() {
                        streams_signalled += 1;
                    }
                }
                self.changed.notify_all();
                ShutdownStart::Owner(ShutdownOwner {
                    listener: state.listener.take(),
                    wake_writer: state.wake_writer.take(),
                    connections_at_start,
                    admissions_at_start,
                    streams_signalled,
                })
            }
        }
    }

    fn wait_for_drain(
        &self,
        deadline: Instant,
        connections_at_start: usize,
        admissions_at_start: usize,
        streams_signalled: usize,
    ) -> RuntimeUnixShutdownReport {
        let mut state = self.lock();
        while state.accept_waiters != 0 || !state.connections.is_empty() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next_state, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("Unix listener shutdown state must not be poisoned");
            state = next_state;
            if timeout.timed_out() {
                break;
            }
        }
        RuntimeUnixShutdownReport {
            connections_at_start,
            admissions_at_start,
            streams_signalled,
            remaining_connections: state.connections.len(),
            remaining_admissions: state
                .connections
                .values()
                .filter(|connection| connection.admission_active)
                .count(),
            remaining_accept_waiters: state.accept_waiters,
            endpoint_cleanup: RuntimeUnixEndpointCleanup::Failed,
        }
    }

    fn finish_shutdown(&self, report: RuntimeUnixShutdownReport) {
        let mut state = self.lock();
        assert!(
            matches!(state.lifecycle, RuntimeUnixListenerLifecycle::Draining),
            "only the shutdown owner may publish a report"
        );
        state.lifecycle = RuntimeUnixListenerLifecycle::Stopped(report);
        self.changed.notify_all();
    }

    fn wait_for_shutdown(&self) -> RuntimeUnixShutdownReport {
        let mut state = self.lock();
        loop {
            match &state.lifecycle {
                RuntimeUnixListenerLifecycle::Stopped(report) => return report.clone(),
                RuntimeUnixListenerLifecycle::Draining => {
                    state = self
                        .changed
                        .wait(state)
                        .expect("Unix listener shutdown state must not be poisoned");
                }
                RuntimeUnixListenerLifecycle::Accepting => {
                    unreachable!("only a shutdown caller can wait for shutdown")
                }
            }
        }
    }

    fn complete_admission(&self, id: u32) {
        let mut state = self.lock();
        let connection = state
            .connections
            .get_mut(&id)
            .expect("live Unix connection registration must be present");
        connection.admission_active = false;
        self.changed.notify_all();
    }

    fn remove_connection(&self, id: u32) {
        let mut state = self.lock();
        let removed = state.connections.remove(&id);
        assert!(
            removed.is_some(),
            "live Unix connection registration must be present"
        );
        self.changed.notify_all();
    }

    fn remove_accept_waiter(&self) {
        let mut state = self.lock();
        assert!(
            state.accept_waiters > 0,
            "live accept waiter must be counted"
        );
        state.accept_waiters -= 1;
        self.changed.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeUnixListenerState> {
        self.state
            .lock()
            .expect("Unix listener shutdown state must not be poisoned")
    }
}

impl fmt::Debug for RuntimeUnixListenerControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("RuntimeUnixListenerControl")
            .field("lifecycle", &state.lifecycle)
            .field("accept_waiters", &state.accept_waiters)
            .field("connections", &state.connections.len())
            .finish()
    }
}

struct RuntimeUnixListenerState {
    lifecycle: RuntimeUnixListenerLifecycle,
    listener: Option<UnixListener>,
    wake_writer: Option<UnixStream>,
    accept_waiters: usize,
    next_connection_id: u32,
    connections: BTreeMap<u32, RegisteredConnection>,
}

fn allocate_protocol_connection_id(state: &mut RuntimeUnixListenerState) -> u32 {
    assert_ne!(
        state.next_connection_id, 0,
        "the next protocol connection ID must be nonzero"
    );
    let attempts = state
        .connections
        .len()
        .checked_add(1)
        .expect("active connection count must fit in usize");
    for _ in 0..attempts {
        let id = state.next_connection_id;
        state.next_connection_id = if id == u32::MAX { 1 } else { id + 1 };
        if !state.connections.contains_key(&id) {
            return id;
        }
    }
    unreachable!("an active connection permit guarantees an unused protocol connection ID")
}

#[derive(Debug, Clone)]
enum RuntimeUnixListenerLifecycle {
    Accepting,
    Draining,
    Stopped(RuntimeUnixShutdownReport),
}

struct RegisteredConnection {
    stream: UnixStream,
    admission_active: bool,
}

enum ShutdownStart {
    Owner(ShutdownOwner),
    Wait,
    Finished(RuntimeUnixShutdownReport),
}

struct ShutdownOwner {
    listener: Option<UnixListener>,
    wake_writer: Option<UnixStream>,
    connections_at_start: usize,
    admissions_at_start: usize,
    streams_signalled: usize,
}

struct AcceptWaiter {
    control: Arc<RuntimeUnixListenerControl>,
    listener: UnixListener,
}

impl Drop for AcceptWaiter {
    fn drop(&mut self) {
        self.control.remove_accept_waiter();
    }
}

struct ConnectionRegistration {
    control: Arc<RuntimeUnixListenerControl>,
    id: u32,
    admission_active: bool,
}

impl ConnectionRegistration {
    fn connection_id(&self) -> u32 {
        self.id
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeUnixListenerError> {
        self.control.begin_protocol_work()
    }

    fn complete_admission(&mut self) {
        if self.admission_active {
            self.control.complete_admission(self.id);
            self.admission_active = false;
        }
    }
}

impl Drop for ConnectionRegistration {
    fn drop(&mut self) {
        self.control.remove_connection(self.id);
    }
}

struct ConnectionLease {
    permits: ConnectionPermits,
    registration: ConnectionRegistration,
}

impl ConnectionLease {
    fn connection_id(&self) -> u32 {
        self.registration.connection_id()
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeUnixListenerError> {
        self.registration.begin_protocol_work()
    }

    fn complete_admission(&mut self) -> Result<(), RuntimeUnixListenerError> {
        self.permits.complete_admission()?;
        self.registration.complete_admission();
        Ok(())
    }

    fn admission_complete(&self) -> bool {
        self.permits.admission_complete()
    }
}

enum ListenerWait {
    ListenerReady,
    ShuttingDown,
}

fn wait_for_listener_or_shutdown(
    listener: &UnixListener,
    wake_reader: &UnixStream,
    control: &RuntimeUnixListenerControl,
) -> Result<ListenerWait, RuntimeUnixListenerError> {
    loop {
        if control.is_shutting_down() {
            return Ok(ListenerWait::ShuttingDown);
        }
        let mut descriptors = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both descriptors remain owned by the listener for the call,
        // and `descriptors` points to exactly two writable pollfd values.
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if result < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(RuntimeUnixListenerError::AcceptUnavailable);
        }
        if descriptors[1].revents != 0 || control.is_shutting_down() {
            return Ok(ListenerWait::ShuttingDown);
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            return Ok(ListenerWait::ListenerReady);
        }
        return Err(RuntimeUnixListenerError::AcceptUnavailable);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionLimitError {
    /// The active connection cap has been reached.
    ConnectionsExhausted,
    /// The in-progress authentication cap has been reached.
    AdmissionsExhausted,
    /// The permit state cannot be inspected safely.
    Unavailable,
}

impl fmt::Display for ConnectionLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionsExhausted => f.write_str("connection limit reached"),
            Self::AdmissionsExhausted => f.write_str("admission limit reached"),
            Self::Unavailable => f.write_str("connection limit state is unavailable"),
        }
    }
}

impl Error for ConnectionLimitError {}

struct PermitState {
    limits: RuntimeLimits,
    connections: usize,
    admissions: usize,
}

impl PermitState {
    fn new(limits: RuntimeLimits) -> Self {
        Self {
            limits,
            connections: 0,
            admissions: 0,
        }
    }
}

struct ConnectionPermits {
    state: Arc<Mutex<PermitState>>,
    admission_active: bool,
}

impl ConnectionPermits {
    fn acquire(state: &Arc<Mutex<PermitState>>) -> Result<Self, ConnectionLimitError> {
        let mut counts = state
            .lock()
            .map_err(|_| ConnectionLimitError::Unavailable)?;
        if counts.connections == counts.limits.max_connections() {
            return Err(ConnectionLimitError::ConnectionsExhausted);
        }
        if counts.admissions == counts.limits.max_admissions() {
            return Err(ConnectionLimitError::AdmissionsExhausted);
        }
        counts.connections += 1;
        counts.admissions += 1;
        Ok(Self {
            state: Arc::clone(state),
            admission_active: true,
        })
    }

    fn complete_admission(&mut self) -> Result<(), RuntimeUnixListenerError> {
        if !self.admission_active {
            return Ok(());
        }
        let mut counts = self.state.lock().map_err(|_| {
            RuntimeUnixListenerError::ConnectionLimit(ConnectionLimitError::Unavailable)
        })?;
        assert!(
            counts.admissions > 0,
            "live admission permit must be counted"
        );
        counts.admissions -= 1;
        self.admission_active = false;
        Ok(())
    }

    fn admission_complete(&self) -> bool {
        !self.admission_active
    }
}

impl Drop for ConnectionPermits {
    fn drop(&mut self) {
        let Ok(mut counts) = self.state.lock() else {
            return;
        };
        assert!(
            counts.connections > 0,
            "live connection permit must be counted"
        );
        counts.connections -= 1;
        if self.admission_active {
            assert!(
                counts.admissions > 0,
                "live admission permit must be counted"
            );
            counts.admissions -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        os::fd::AsRawFd,
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier, Mutex},
        thread,
    };

    use super::*;
    use crate::{
        AccountDefinition, AccountGenerationBuilder, AccountId, AccountStoreCheckpoint,
        AccountStoreCheckpointAuthority, AccountStoreCheckpointRequest, CheckpointAuthorityId,
        CheckpointPersistence, CheckpointReadError, GlobalPrivileges, OfflineAccountProvisioner,
        RuntimeTimeouts, UnixSocketConfig, MIN_WRITE_LIMIT,
    };

    struct FakeCheckpointReader {
        results: Mutex<VecDeque<Result<AccountStoreCheckpoint, CheckpointReadError>>>,
    }

    impl FakeCheckpointReader {
        fn new(
            results: impl IntoIterator<Item = Result<AccountStoreCheckpoint, CheckpointReadError>>,
        ) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
            }
        }

        fn push(&self, result: Result<AccountStoreCheckpoint, CheckpointReadError>) {
            self.results.lock().unwrap().push_back(result);
        }
    }

    impl AccountStoreCheckpointReader for FakeCheckpointReader {
        fn request_checkpoint(
            &self,
            _authority: &CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
            let result = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(CheckpointReadError::Missing));
            Ok(AccountStoreCheckpointRequest::completed(result))
        }
    }

    #[derive(Default)]
    struct MemoryAuthority {
        checkpoint: Option<AccountStoreCheckpoint>,
    }

    impl AccountStoreCheckpointAuthority for MemoryAuthority {
        fn compare_and_persist(
            &mut self,
            expected: Option<&AccountStoreCheckpoint>,
            replacement: &AccountStoreCheckpoint,
        ) -> CheckpointPersistence {
            if self.checkpoint.as_ref() == Some(replacement) {
                return CheckpointPersistence::Durable;
            }
            if self.checkpoint.as_ref() != expected {
                return CheckpointPersistence::Conflict;
            }
            self.checkpoint = Some(*replacement);
            CheckpointPersistence::Durable
        }
    }

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn checkpoint(account_root: &std::path::Path) -> AccountStoreCheckpoint {
        let mut authority = MemoryAuthority::default();
        let account =
            AccountDefinition::new("alice", AccountId::from_bytes([7; 32]), true, [0x11; 32])
                .with_global_privileges(GlobalPrivileges::new(true, false));
        let provisioner = OfflineAccountProvisioner::initialize(
            account_root,
            AccountGenerationBuilder::new().with_account(account),
            &mut authority,
        )
        .unwrap();
        provisioner.checkpoint().unwrap()
    }

    fn config(
        data_root: &std::path::Path,
        account_root: &std::path::Path,
        socket_directory: &std::path::Path,
        limits: RuntimeLimits,
        authentication: Duration,
        idle: Duration,
    ) -> RuntimeConfig {
        let data_root = data_root.canonicalize().unwrap();
        let account_root = account_root.canonicalize().unwrap();
        let socket_directory = socket_directory.canonicalize().unwrap();
        RuntimeConfig::new(
            None,
            Some(UnixSocketConfig::new(&socket_directory, "mysql.sock").unwrap()),
            &data_root,
            &account_root,
            CheckpointAuthorityId::new("runtime-checkpoints").unwrap(),
            Duration::from_secs(1),
            limits,
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                authentication,
                idle,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn runtime(
        limits: RuntimeLimits,
        authentication: Duration,
        idle: Duration,
    ) -> (
        RuntimeUnixListener,
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        std::path::PathBuf,
    ) {
        let data_root = private_directory();
        let account_root = private_directory();
        let socket_directory = private_directory();
        let checkpoint = checkpoint(account_root.path());
        let config = config(
            data_root.path(),
            account_root.path(),
            socket_directory.path(),
            limits,
            authentication,
            idle,
        );
        let endpoint = config.unix_socket().unwrap().socket_path();
        let reader = Arc::new(FakeCheckpointReader::new([Ok(checkpoint), Ok(checkpoint)]));
        let listener = RuntimeUnixListener::bind(&config, reader).unwrap();
        (
            listener,
            data_root,
            account_root,
            socket_directory,
            endpoint,
        )
    }

    fn limits(connections: usize, admissions: usize) -> RuntimeLimits {
        RuntimeLimits::new(connections, admissions, MIN_WRITE_LIMIT, 1).unwrap()
    }

    fn wait_for_accept_waiter(listener: &RuntimeUnixListener) {
        let mut state = listener.control.lock();
        while state.accept_waiters == 0 {
            state = listener
                .control
                .changed
                .wait(state)
                .expect("test listener state must not be poisoned");
        }
    }

    fn shutdown_timeout(timeout: Duration) -> RuntimeTimeouts {
        RuntimeTimeouts::new(timeout, timeout, timeout, timeout, timeout, timeout).unwrap()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn binding_sets_private_mode_and_drop_removes_the_owned_endpoint() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = runtime(
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        let metadata = fs::symlink_metadata(&endpoint).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);

        drop(listener);

        assert!(!endpoint.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn shutdown_wakes_a_blocking_accept_without_returning_a_stream() {
        let (listener, _data_root, _account_root, _socket_directory, _endpoint) = runtime(
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        let listener = Arc::new(listener);
        let accepting = Arc::clone(&listener);
        let accept_thread = thread::spawn(move || accepting.accept());
        wait_for_accept_waiter(&listener);

        let report = listener.shutdown();
        assert!(report.drained());
        assert_eq!(report.remaining_accept_waiters(), 0);
        assert_eq!(
            report.endpoint_cleanup(),
            RuntimeUnixEndpointCleanup::Removed
        );
        assert!(matches!(
            accept_thread.join().unwrap(),
            Err(RuntimeUnixListenerError::ShuttingDown)
        ));
        assert!(matches!(
            listener.accept(),
            Err(RuntimeUnixListenerError::ShuttingDown)
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn shutdown_signals_active_and_pending_streams_then_releases_their_permits() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = runtime(
            limits(2, 2),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        let _first_client = UnixStream::connect(&endpoint).unwrap();
        let mut first = listener.accept().unwrap();
        first.complete_admission().unwrap();
        let _second_client = UnixStream::connect(&endpoint).unwrap();
        let second = listener.accept().unwrap();

        let first_thread = thread::spawn(move || {
            let mut stream = first;
            let mut byte = [0; 1];
            stream.read(&mut byte)
        });
        let second_thread = thread::spawn(move || {
            let mut stream = second;
            let mut byte = [0; 1];
            stream.read(&mut byte)
        });

        let report = listener.shutdown();
        assert!(report.drained());
        assert_eq!(report.connections_at_start(), 2);
        assert_eq!(report.admissions_at_start(), 1);
        assert_eq!(report.streams_signalled(), 2);
        assert!(matches!(first_thread.join().unwrap(), Ok(0) | Err(_)));
        assert!(matches!(second_thread.join().unwrap(), Ok(0) | Err(_)));
        let counts = listener.control.permits.lock().unwrap();
        assert_eq!(counts.connections, 0);
        assert_eq!(counts.admissions, 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn shutdown_reports_streams_that_outlive_its_single_deadline() {
        let (mut listener, _data_root, _account_root, _socket_directory, endpoint) =
            runtime(limits(1, 1), Duration::from_secs(1), Duration::from_secs(1));
        listener.timeouts = shutdown_timeout(Duration::from_millis(20));
        let _client = UnixStream::connect(&endpoint).unwrap();
        let accepted = listener.accept().unwrap();

        let report = listener.shutdown();
        assert!(!report.drained());
        assert_eq!(report.remaining_connections(), 1);
        assert_eq!(report.remaining_admissions(), 1);
        assert_eq!(
            report.endpoint_cleanup(),
            RuntimeUnixEndpointCleanup::Removed
        );

        drop(accepted);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn concurrent_shutdown_returns_one_identity_safe_cleanup_result() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = runtime(
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        fs::remove_file(&endpoint).unwrap();
        let replacement = UnixListener::bind(&endpoint).unwrap();
        let listener = Arc::new(listener);
        let barrier = Arc::new(Barrier::new(2));
        let other_listener = Arc::clone(&listener);
        let other_barrier = Arc::clone(&barrier);
        let other = thread::spawn(move || {
            other_barrier.wait();
            other_listener.shutdown()
        });

        barrier.wait();
        let report = listener.shutdown();
        assert_eq!(report, other.join().unwrap());
        assert_eq!(
            report.endpoint_cleanup(),
            RuntimeUnixEndpointCleanup::AlreadyMissingOrReplaced
        );
        assert!(endpoint.exists());
        drop(replacement);
        fs::remove_file(endpoint).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn final_checkpoint_failure_never_creates_an_endpoint() {
        let data_root = private_directory();
        let account_root = private_directory();
        let socket_directory = private_directory();
        let checkpoint = checkpoint(account_root.path());
        let config = config(
            data_root.path(),
            account_root.path(),
            socket_directory.path(),
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        let endpoint = config.unix_socket().unwrap().socket_path();
        let reader = Arc::new(FakeCheckpointReader::new([
            Ok(checkpoint),
            Err(CheckpointReadError::Unavailable),
        ]));

        assert!(matches!(
            RuntimeUnixListener::bind(&config, reader),
            Err(RuntimeUnixListenerError::AccountStore(
                RuntimeAccountStoreError::CheckpointRead(CheckpointReadError::Unavailable)
            ))
        ));
        assert!(!endpoint.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn writable_socket_ancestor_is_rejected_before_binding() {
        let data_root = private_directory();
        let account_root = private_directory();
        let socket_root = private_directory();
        let socket_directory = socket_root.path().join("writable");
        fs::create_dir(&socket_directory).unwrap();
        fs::set_permissions(&socket_directory, fs::Permissions::from_mode(0o770)).unwrap();
        let config = config(
            data_root.path(),
            account_root.path(),
            &socket_directory,
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        let endpoint = config.unix_socket().unwrap().socket_path();

        assert!(matches!(
            RuntimeUnixListener::bind(
                &config,
                Arc::new(FakeCheckpointReader::new([Err(
                    CheckpointReadError::Missing
                )])),
            ),
            Err(RuntimeUnixListenerError::SocketInvalidDirectory)
        ));
        assert!(!endpoint.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn same_uid_accept_uses_authentication_then_idle_timeouts() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = runtime(
            limits(1, 1),
            Duration::from_millis(100),
            Duration::from_millis(100),
        );
        let _client = UnixStream::connect(endpoint).unwrap();
        let mut accepted = listener.accept().unwrap();

        // SAFETY: the accepted stream owns this live descriptor for the call.
        let flags = unsafe { libc::fcntl(accepted.stream.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1, "accepted stream flags must be readable");
        assert_eq!(flags & libc::O_NONBLOCK, 0);
        assert_eq!(
            accepted.timeouts().authentication(),
            Duration::from_millis(100)
        );
        let authentication_deadline = accepted.authentication_deadline();
        thread::sleep(Duration::from_millis(150));
        assert!(Instant::now() >= authentication_deadline);
        let mut byte = [0; 1];
        let error = accepted.read(&mut byte).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));

        accepted.complete_admission().unwrap();
        assert_eq!(accepted.timeouts().idle(), Duration::from_millis(100));
        let error = accepted.read(&mut byte).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn protocol_work_linearizes_before_shutdown() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) =
            runtime(limits(1, 1), Duration::from_secs(1), Duration::from_secs(1));
        let listener = Arc::new(listener);
        let _client = UnixStream::connect(endpoint).unwrap();
        let accepted = listener.accept().unwrap();

        accepted.begin_protocol_work().unwrap();
        let shutting_down = Arc::clone(&listener);
        let shutdown = thread::spawn(move || shutting_down.shutdown());
        while !listener.is_shutting_down() {
            thread::yield_now();
        }
        assert!(matches!(
            accepted.begin_protocol_work(),
            Err(RuntimeUnixListenerError::ShuttingDown)
        ));

        drop(accepted);
        assert!(shutdown.join().unwrap().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn protocol_connection_ids_are_live_unique_and_reused_after_wrap() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = runtime(
            limits(4, 4),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );

        let _first_client = UnixStream::connect(&endpoint).unwrap();
        let first = listener.accept().unwrap();
        assert_eq!(first.connection_id(), 1);

        listener.control.lock().next_connection_id = u32::MAX;
        let _second_client = UnixStream::connect(&endpoint).unwrap();
        let second = listener.accept().unwrap();
        assert_eq!(second.connection_id(), u32::MAX);

        let _third_client = UnixStream::connect(&endpoint).unwrap();
        let third = listener.accept().unwrap();
        assert_eq!(third.connection_id(), 2);
        assert_ne!(first.connection_id(), second.connection_id());
        assert_ne!(first.connection_id(), third.connection_id());
        assert_ne!(second.connection_id(), third.connection_id());

        let accounts = second.account_store();
        let catalog = second.catalog();
        assert!(Arc::ptr_eq(&accounts, &listener.accounts));
        assert!(Arc::ptr_eq(&catalog, &listener.catalog));
        assert_eq!(second.limits(), listener.limits);
        assert_eq!(second.timeouts(), listener.timeouts);
        second.set_read_timeout(Duration::from_millis(20)).unwrap();
        second.set_write_timeout(Duration::from_millis(20)).unwrap();

        drop(first);
        listener.control.lock().next_connection_id = 1;
        let _fourth_client = UnixStream::connect(&endpoint).unwrap();
        let fourth = listener.accept().unwrap();
        assert_eq!(fourth.connection_id(), 1);

        drop(fourth);
        drop(third);
        drop(second);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn connection_quota_releases_when_the_owner_drops() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = runtime(
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        let first_client = UnixStream::connect(&endpoint).unwrap();
        let first = listener.accept().unwrap();

        let second_client = UnixStream::connect(&endpoint).unwrap();
        assert!(matches!(
            listener.accept(),
            Err(RuntimeUnixListenerError::ConnectionLimit(
                ConnectionLimitError::ConnectionsExhausted
            ))
        ));
        drop(second_client);
        drop(first);
        drop(first_client);

        let third_client = UnixStream::connect(endpoint).unwrap();
        let third = listener.accept().unwrap();
        drop(third);
        drop(third_client);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn drop_never_removes_a_replacement_endpoint() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = runtime(
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        fs::remove_file(&endpoint).unwrap();
        let replacement = UnixListener::bind(&endpoint).unwrap();

        drop(listener);

        assert!(endpoint.exists());
        drop(replacement);
        fs::remove_file(endpoint).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn post_bind_failure_reports_when_identity_safe_cleanup_cannot_remove_the_endpoint() {
        let socket_directory = private_directory();
        let directory =
            UnixSocketDirectory::open(&socket_directory.path().canonicalize().unwrap()).unwrap();
        let endpoint = socket_directory.path().join("mysql.sock");
        let original = UnixListener::bind(&endpoint).unwrap();
        let identity = directory.endpoint_identity("mysql.sock").unwrap().unwrap();
        let mut cleanup = EndpointCleanup::new(&directory, "mysql.sock", identity);
        fs::remove_file(&endpoint).unwrap();
        let replacement = UnixListener::bind(&endpoint).unwrap();

        assert_eq!(
            cleanup.recover(RuntimeUnixListenerError::BindUnavailable),
            RuntimeUnixListenerError::SocketCleanupRequired
        );
        assert!(endpoint.exists());

        drop(cleanup);
        drop(original);
        drop(replacement);
        fs::remove_file(endpoint).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn degraded_account_store_rejects_accept_before_waiting_for_a_client() {
        let data_root = private_directory();
        let account_root = private_directory();
        let socket_directory = private_directory();
        let checkpoint = checkpoint(account_root.path());
        let config = config(
            data_root.path(),
            account_root.path(),
            socket_directory.path(),
            limits(1, 1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        let reader = Arc::new(FakeCheckpointReader::new([Ok(checkpoint), Ok(checkpoint)]));
        let checkpoint_reader: Arc<dyn AccountStoreCheckpointReader> = reader.clone();
        let listener = RuntimeUnixListener::bind(&config, checkpoint_reader).unwrap();
        reader.push(Err(CheckpointReadError::Unavailable));

        assert!(matches!(
            listener.reload_accounts_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                CheckpointReadError::Unavailable
            ))
        ));
        assert!(!listener.is_ready_for_new_connections());
        assert!(matches!(
            listener.accept(),
            Err(RuntimeUnixListenerError::AccountNotReady)
        ));
    }

    #[test]
    fn connection_and_admission_permits_release_at_their_required_boundaries() {
        let state = Arc::new(Mutex::new(PermitState::new(limits(1, 1))));
        let mut first = ConnectionPermits::acquire(&state).unwrap();
        assert!(matches!(
            ConnectionPermits::acquire(&state),
            Err(ConnectionLimitError::ConnectionsExhausted)
        ));

        first.complete_admission().unwrap();
        assert!(first.admission_complete());
        drop(first);

        let second = ConnectionPermits::acquire(&state).unwrap();
        drop(second);
    }

    #[test]
    fn admission_cap_rejects_before_connection_cap_when_it_is_smaller() {
        let state = Arc::new(Mutex::new(PermitState::new(limits(2, 1))));
        let first = ConnectionPermits::acquire(&state).unwrap();

        assert!(matches!(
            ConnectionPermits::acquire(&state),
            Err(ConnectionLimitError::AdmissionsExhausted)
        ));
        drop(first);
    }
}
