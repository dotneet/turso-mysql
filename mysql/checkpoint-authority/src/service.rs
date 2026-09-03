// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! A bounded, single-owner Unix service for checkpoint reads and CAS writes.

use std::{
    error::Error,
    fmt,
    io::{ErrorKind, Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use turso_mysql_server::UnixPeerVerifier;

use crate::unix_fs::{
    checked_socket_path, SocketEndpointIdentity, SocketOwnerLock, UnixSocketDirectory,
    UnixSocketFsError,
};
use crate::{
    decode_request, encode_response, AuthorityId, CasResponse, CheckpointStore, CheckpointStoreCas,
    GetResponse, ProtocolError, Request, Response, MAX_FRAME_PAYLOAD_BYTES,
};

/// The shortest useful per-connection I/O deadline.
pub const MIN_CONNECTION_IO_TIMEOUT: Duration = Duration::from_millis(1);
/// The largest per-connection I/O deadline accepted by the service.
pub const MAX_CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

const MAX_CONNECTION_WORKERS: usize = 16;
const WORKER_POLL_WAIT: Duration = Duration::from_millis(10);

/// Validated configuration for one authority service.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckpointAuthorityConfig {
    authority: AuthorityId,
    state_root: PathBuf,
    socket_directory: PathBuf,
    socket_name: String,
    socket_gid: u32,
    client_uid: u32,
    io_timeout: Duration,
}

impl CheckpointAuthorityConfig {
    /// Creates a side-effect-free configuration for one local authority.
    pub fn new(
        authority: AuthorityId,
        state_root: impl AsRef<Path>,
        socket_directory: impl AsRef<Path>,
        socket_name: impl Into<String>,
        socket_gid: u32,
        client_uid: u32,
        io_timeout: Duration,
    ) -> Result<Self, CheckpointAuthorityConfigError> {
        let state_root = state_root.as_ref().to_owned();
        let socket_directory = socket_directory.as_ref().to_owned();
        let socket_name = socket_name.into();
        validate_absolute_path(&state_root)?;
        validate_absolute_path(&socket_directory)?;
        checked_socket_path(&socket_directory, &socket_name)
            .map_err(|_| CheckpointAuthorityConfigError::InvalidSocketPath)?;
        if !(MIN_CONNECTION_IO_TIMEOUT..=MAX_CONNECTION_IO_TIMEOUT).contains(&io_timeout) {
            return Err(CheckpointAuthorityConfigError::IoTimeoutOutOfRange);
        }
        Ok(Self {
            authority,
            state_root,
            socket_directory,
            socket_name,
            socket_gid,
            client_uid,
            io_timeout,
        })
    }

    /// Returns the opaque authority identifier.
    pub fn authority(&self) -> &AuthorityId {
        &self.authority
    }

    /// Returns the configured authority state root.
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Returns the configured socket directory.
    pub fn socket_directory(&self) -> &Path {
        &self.socket_directory
    }

    /// Returns the configured one-component socket name.
    pub fn socket_name(&self) -> &str {
        &self.socket_name
    }

    /// Returns the dedicated group allowed to traverse and connect to the socket.
    pub const fn socket_gid(&self) -> u32 {
        self.socket_gid
    }

    /// Returns the only client effective UID accepted by this service.
    pub const fn client_uid(&self) -> u32 {
        self.client_uid
    }

    /// Returns the bounded read/write deadline for one connection.
    pub const fn io_timeout(&self) -> Duration {
        self.io_timeout
    }
}

impl fmt::Debug for CheckpointAuthorityConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointAuthorityConfig")
            .field("authority", &self.authority)
            .field("state_root", &"<redacted>")
            .field("socket_directory", &"<redacted>")
            .field("socket_name", &"<redacted>")
            .field("socket_gid", &"<redacted>")
            .field("client_uid", &"<redacted>")
            .field("io_timeout", &self.io_timeout)
            .finish()
    }
}

/// A side-effect-free configuration failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointAuthorityConfigError {
    /// A root path was relative, contained NUL, or used `.`/`..` components.
    InvalidPath,
    /// The socket directory and name do not form a permitted Unix endpoint.
    InvalidSocketPath,
    /// The connection deadline was zero or exceeded the service bound.
    IoTimeoutOutOfRange,
}

impl fmt::Display for CheckpointAuthorityConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath => f.write_str("checkpoint authority path is invalid"),
            Self::InvalidSocketPath => f.write_str("checkpoint authority socket path is invalid"),
            Self::IoTimeoutOutOfRange => {
                f.write_str("checkpoint authority connection timeout is out of range")
            }
        }
    }
}

impl Error for CheckpointAuthorityConfigError {}

/// Failure while opening the authority's state or Unix endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointAuthorityBindError {
    /// The platform has no reviewed peer-credential or filesystem support.
    UnsupportedPlatform,
    /// Production binding was attempted with the service and client UID equal.
    ClientUidMatchesService,
    /// The service was not launched with the configured shared socket group.
    SocketGroupMismatch,
    /// The authority state root could not be opened safely.
    StateUnavailable,
    /// The socket directory, owner lock, or endpoint had an unsafe state.
    SocketUnavailable,
    /// The service could not create its private shutdown wake channel.
    WakeUnavailable,
    /// The endpoint could not be bound or configured safely.
    BindUnavailable,
    /// A failed bind left an endpoint whose removal could not be confirmed.
    EndpointCleanupUnavailable,
}

impl fmt::Display for CheckpointAuthorityBindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => f.write_str("checkpoint authority is unsupported"),
            Self::ClientUidMatchesService => {
                f.write_str("checkpoint authority client UID must differ from service UID")
            }
            Self::SocketGroupMismatch => {
                f.write_str("checkpoint authority socket group does not match the service")
            }
            Self::StateUnavailable => f.write_str("checkpoint authority state is unavailable"),
            Self::SocketUnavailable => f.write_str("checkpoint authority socket is unavailable"),
            Self::WakeUnavailable => {
                f.write_str("checkpoint authority shutdown wake is unavailable")
            }
            Self::BindUnavailable => {
                f.write_str("checkpoint authority endpoint could not be bound")
            }
            Self::EndpointCleanupUnavailable => {
                f.write_str("checkpoint authority endpoint cleanup failed")
            }
        }
    }
}

impl Error for CheckpointAuthorityBindError {}

/// A terminal service infrastructure failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointAuthorityRunError {
    /// The listener could no longer be polled or accepted safely.
    ListenerUnavailable,
    /// The shutdown wake descriptor failed while the service was live.
    WakeUnavailable,
    /// The authority state could not be read or durably updated.
    StateUnavailable,
    /// A connection worker panicked or could not be recovered safely.
    WorkerUnavailable,
    /// The service could not confirm identity-safe endpoint cleanup.
    EndpointCleanupUnavailable,
}

impl fmt::Display for CheckpointAuthorityRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ListenerUnavailable => f.write_str("checkpoint authority listener failed"),
            Self::WakeUnavailable => f.write_str("checkpoint authority wake failed"),
            Self::StateUnavailable => f.write_str("checkpoint authority state failed"),
            Self::WorkerUnavailable => f.write_str("checkpoint authority worker failed"),
            Self::EndpointCleanupUnavailable => {
                f.write_str("checkpoint authority endpoint cleanup failed")
            }
        }
    }
}

impl Error for CheckpointAuthorityRunError {}

/// Counts ordinary client failures without retaining client details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointAuthorityStats {
    rejected_clients: u64,
    failed_clients: u64,
}

impl CheckpointAuthorityStats {
    /// Returns clients rejected before a successful authority operation.
    pub const fn rejected_clients(self) -> u64 {
        self.rejected_clients
    }

    /// Returns clients whose I/O or authority operation failed.
    pub const fn failed_clients(self) -> u64 {
        self.failed_clients
    }
}

/// A bounded shutdown result and redacted service counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointAuthorityRunReport {
    stats: CheckpointAuthorityStats,
    endpoint_removed: bool,
}

impl CheckpointAuthorityRunReport {
    /// Returns redacted client counters.
    pub const fn stats(self) -> CheckpointAuthorityStats {
        self.stats
    }

    /// Returns whether the endpoint owned by this run was unlinked.
    pub const fn endpoint_removed(self) -> bool {
        self.endpoint_removed
    }
}

/// A cloneable signal that wakes and stops one authority run.
#[derive(Clone)]
pub struct CheckpointAuthorityShutdown {
    control: Arc<ServiceControl>,
    wake_writer: Arc<UnixStream>,
}

impl CheckpointAuthorityShutdown {
    /// Requests shutdown. The current bounded connection operation may finish;
    /// no later connection is admitted.
    pub fn shutdown(&self) {
        if !self.control.shutdown.swap(true, Ordering::AcqRel) {
            if let Ok(mut writer) = self.wake_writer.try_clone() {
                let _ = writer.write_all(&[1]);
            }
        }
    }

    /// Returns whether shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.control.shutdown.load(Ordering::Acquire)
    }

    /// Returns a snapshot of redacted client counters.
    pub fn stats(&self) -> CheckpointAuthorityStats {
        self.control.stats()
    }
}

impl fmt::Debug for CheckpointAuthorityShutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CheckpointAuthorityShutdown { <redacted> }")
    }
}

/// The single-owner local checkpoint authority service.
pub struct CheckpointAuthority {
    listener: UnixListener,
    wake_reader: UnixStream,
    _wake_writer: UnixStream,
    directory: UnixSocketDirectory,
    _owner_lock: SocketOwnerLock,
    endpoint_identity: Option<SocketEndpointIdentity>,
    store: Arc<CheckpointStore>,
    peer_verifier: UnixPeerVerifier,
    authority: AuthorityId,
    socket_name: String,
    socket_path: PathBuf,
    io_timeout: Duration,
    control: Arc<ServiceControl>,
}

impl CheckpointAuthority {
    /// Opens the state and binds a production service.
    pub fn bind(config: CheckpointAuthorityConfig) -> Result<Self, CheckpointAuthorityBindError> {
        Self::bind_inner(config, false)
    }

    /// Binds a same-UID service only for this crate's unprivileged tests.
    #[cfg(test)]
    pub(crate) fn bind_for_test(
        config: CheckpointAuthorityConfig,
    ) -> Result<Self, CheckpointAuthorityBindError> {
        Self::bind_inner(config, true)
    }

    fn bind_inner(
        config: CheckpointAuthorityConfig,
        allow_same_uid: bool,
    ) -> Result<Self, CheckpointAuthorityBindError> {
        ensure_supported_platform()?;
        if !allow_same_uid && effective_uid() == config.client_uid {
            return Err(CheckpointAuthorityBindError::ClientUidMatchesService);
        }
        if effective_gid() != config.socket_gid {
            return Err(CheckpointAuthorityBindError::SocketGroupMismatch);
        }
        let store = CheckpointStore::open(&config.state_root, config.authority.clone())
            .map_err(|_| CheckpointAuthorityBindError::StateUnavailable)?;
        let peer_verifier =
            UnixPeerVerifier::for_effective_uid(config.client_uid).map_err(map_peer_bind_error)?;
        let directory =
            UnixSocketDirectory::open(&config.socket_directory).map_err(map_socket_bind_error)?;
        let owner_lock = directory
            .acquire_owner_lock()
            .map_err(map_socket_bind_error)?;
        let socket_path = directory
            .prepare_bind(&config.socket_directory, &config.socket_name)
            .map_err(map_socket_bind_error)?;
        let (wake_reader, wake_writer) =
            UnixStream::pair().map_err(|_| CheckpointAuthorityBindError::WakeUnavailable)?;
        wake_reader
            .set_nonblocking(true)
            .map_err(|_| CheckpointAuthorityBindError::WakeUnavailable)?;
        let listener = UnixListener::bind(&socket_path)
            .map_err(|_| CheckpointAuthorityBindError::BindUnavailable)?;
        let endpoint_identity = match directory.configure_bound_endpoint(&config.socket_name) {
            Ok(identity) => identity,
            Err(_) => {
                return Err(recover_unpublished_endpoint(
                    &directory,
                    &config.socket_name,
                    CheckpointAuthorityBindError::BindUnavailable,
                ));
            }
        };
        if listener.set_nonblocking(true).is_err() {
            return Err(recover_published_endpoint(
                &directory,
                &config.socket_name,
                endpoint_identity,
                CheckpointAuthorityBindError::BindUnavailable,
            ));
        }
        Ok(Self {
            listener,
            wake_reader,
            _wake_writer: wake_writer.try_clone().map_err(|_| {
                recover_published_endpoint(
                    &directory,
                    &config.socket_name,
                    endpoint_identity,
                    CheckpointAuthorityBindError::WakeUnavailable,
                )
            })?,
            directory,
            _owner_lock: owner_lock,
            endpoint_identity: Some(endpoint_identity),
            store: Arc::new(store),
            peer_verifier,
            authority: config.authority,
            socket_name: config.socket_name,
            socket_path,
            io_timeout: config.io_timeout,
            control: Arc::new(ServiceControl::default()),
        })
    }

    /// Returns a cloneable shutdown signal.
    pub fn shutdown_handle(&self) -> CheckpointAuthorityShutdown {
        CheckpointAuthorityShutdown {
            control: Arc::clone(&self.control),
            wake_writer: Arc::new(self._wake_writer.try_clone().expect("wake writer is live")),
        }
    }

    /// Returns the bound endpoint path for client configuration.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Returns a snapshot of client counters while the service is running.
    pub fn stats(&self) -> CheckpointAuthorityStats {
        self.control.stats()
    }

    /// Runs the blocking accept loop in the calling thread.
    pub fn run(mut self) -> Result<CheckpointAuthorityRunReport, CheckpointAuthorityRunError> {
        let run_result = self.run_loop();
        let cleanup_result = self.cleanup_endpoint();
        match (run_result, cleanup_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(endpoint_removed)) => Ok(CheckpointAuthorityRunReport {
                stats: self.control.stats(),
                endpoint_removed,
            }),
        }
    }

    fn run_loop(&mut self) -> Result<(), CheckpointAuthorityRunError> {
        let mut workers = Vec::with_capacity(MAX_CONNECTION_WORKERS);
        let result = loop {
            if let Err(error) = reap_finished_workers(&mut workers) {
                break Err(error);
            }
            if self.control.is_shutdown() {
                break Ok(());
            }
            let mut descriptors = [
                libc::pollfd {
                    fd: self.listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wake_reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: both descriptors remain owned by self, and the array has
            // exactly two writable pollfd values.
            let poll_timeout = if workers.is_empty() {
                -1
            } else {
                duration_to_poll_millis(WORKER_POLL_WAIT)
            };
            let poll_result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, poll_timeout) };
            if poll_result < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break Err(CheckpointAuthorityRunError::ListenerUnavailable);
            }
            if poll_result == 0 {
                continue;
            }
            let wake_events = descriptors[1].revents;
            if wake_events & libc::POLLNVAL != 0 {
                break Err(CheckpointAuthorityRunError::WakeUnavailable);
            }
            if wake_events & libc::POLLIN != 0 {
                if let Err(error) = self.drain_wake() {
                    break Err(error);
                }
            } else if wake_events & (libc::POLLERR | libc::POLLHUP) != 0 {
                break Err(CheckpointAuthorityRunError::WakeUnavailable);
            }
            if self.control.is_shutdown() {
                break Ok(());
            }
            let listener_events = descriptors[0].revents;
            if listener_events & libc::POLLNVAL != 0
                || listener_events & (libc::POLLERR | libc::POLLHUP) != 0
            {
                break Err(CheckpointAuthorityRunError::ListenerUnavailable);
            }
            if listener_events & libc::POLLIN == 0 {
                continue;
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = self.start_client_worker(stream, &mut workers) {
                        break Err(error);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => break Err(CheckpointAuthorityRunError::ListenerUnavailable),
            }
        };
        self.control.shutdown.store(true, Ordering::Release);
        match (result, join_all_workers(&mut workers)) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn start_client_worker(
        &self,
        stream: UnixStream,
        workers: &mut Vec<ClientWorker>,
    ) -> Result<(), CheckpointAuthorityRunError> {
        if self.peer_verifier.verify(&stream).is_err() {
            self.control.reject();
            return Ok(());
        }
        if self.control.is_shutdown() {
            return Ok(());
        }
        if workers.len() >= MAX_CONNECTION_WORKERS {
            self.control.reject();
            return Ok(());
        }
        if stream.set_nonblocking(true).is_err() {
            self.control.fail();
            return Ok(());
        }
        let store = Arc::clone(&self.store);
        let authority = self.authority.clone();
        let control = Arc::clone(&self.control);
        let io_timeout = self.io_timeout;
        match thread::Builder::new()
            .name("turso-checkpoint-authority".to_owned())
            .spawn(move || handle_client(stream, store, authority, control, io_timeout))
        {
            Ok(worker) => {
                workers.push(worker);
                Ok(())
            }
            Err(_) => {
                self.control.fail();
                Err(CheckpointAuthorityRunError::WorkerUnavailable)
            }
        }
    }

    fn drain_wake(&mut self) -> Result<(), CheckpointAuthorityRunError> {
        let mut bytes = [0; 64];
        loop {
            match self.wake_reader.read(&mut bytes) {
                Ok(0) => return Err(CheckpointAuthorityRunError::WakeUnavailable),
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err(CheckpointAuthorityRunError::WakeUnavailable),
            }
        }
    }

    fn cleanup_endpoint(&mut self) -> Result<bool, CheckpointAuthorityRunError> {
        let Some(identity) = self.endpoint_identity.take() else {
            return Ok(false);
        };
        self.directory
            .unlink_endpoint_if_matches(&self.socket_name, identity)
            .and_then(|removed| {
                if removed {
                    Ok(true)
                } else {
                    self.directory
                        .ensure_endpoint_absent(&self.socket_name)
                        .map(|()| false)
                }
            })
            .map_err(|_| CheckpointAuthorityRunError::EndpointCleanupUnavailable)
    }
}

fn handle_client(
    mut stream: UnixStream,
    store: Arc<CheckpointStore>,
    authority: AuthorityId,
    control: Arc<ServiceControl>,
    io_timeout: Duration,
) -> ClientOutcome {
    if control.is_shutdown() {
        return ClientOutcome::Continue;
    }
    let deadline = std::time::Instant::now() + io_timeout;
    let request = match read_request(&mut stream, deadline, &control) {
        Ok(request) => request,
        Err(ClientError::Rejected) => {
            control.reject();
            return ClientOutcome::Continue;
        }
        Err(ClientError::Failed) => {
            control.fail();
            return ClientOutcome::Continue;
        }
        Err(ClientError::Cancelled) => return ClientOutcome::Continue,
    };
    if control.is_shutdown() {
        return ClientOutcome::Continue;
    }
    if !request_has_authority(&request, &authority) {
        control.reject();
        return ClientOutcome::Continue;
    }
    let response = match request {
        Request::Get { .. } => match store.read() {
            Ok(Some(checkpoint)) => Response::Get(GetResponse::Checkpoint(checkpoint)),
            Ok(None) => Response::Get(GetResponse::Missing),
            Err(_) => {
                control.fail();
                control.terminate();
                return ClientOutcome::Fatal(CheckpointAuthorityRunError::StateUnavailable);
            }
        },
        Request::CompareAndPersist {
            expected,
            replacement,
            ..
        } => {
            match persist_checkpoint(&store, expected.as_ref(), &replacement, deadline, &control) {
                Ok(CheckpointStoreCas::Durable) => {
                    Response::CompareAndPersist(CasResponse::Durable)
                }
                Ok(CheckpointStoreCas::Conflict) => {
                    Response::CompareAndPersist(CasResponse::Conflict)
                }
                Err(PersistError::TimedOut) => {
                    control.fail();
                    return ClientOutcome::Continue;
                }
                Err(PersistError::Cancelled) => return ClientOutcome::Continue,
                Err(PersistError::StateUnavailable) => {
                    control.fail();
                    control.terminate();
                    return ClientOutcome::Fatal(CheckpointAuthorityRunError::StateUnavailable);
                }
            }
        }
    };
    let encoded = match encode_response(response) {
        Ok(encoded) => encoded,
        Err(_) => {
            control.fail();
            return ClientOutcome::Continue;
        }
    };
    match write_all_until(&mut stream, &encoded, deadline, &control) {
        Ok(()) | Err(ClientError::Cancelled) => {}
        Err(_) => control.fail(),
    }
    let _ = stream.shutdown(Shutdown::Both);
    ClientOutcome::Continue
}

impl fmt::Debug for CheckpointAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointAuthority")
            .field("listener", &"<bound>")
            .field("authority", &self.authority)
            .field("socket_path", &"<redacted>")
            .field("store", &self.store)
            .finish()
    }
}

impl Drop for CheckpointAuthority {
    fn drop(&mut self) {
        if let Some(identity) = self.endpoint_identity.take() {
            let _ = self
                .directory
                .unlink_endpoint_if_matches(&self.socket_name, identity);
        }
    }
}

#[derive(Default)]
struct ServiceControl {
    shutdown: AtomicBool,
    rejected_clients: AtomicU64,
    failed_clients: AtomicU64,
}

impl ServiceControl {
    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    fn reject(&self) {
        self.rejected_clients.fetch_add(1, Ordering::Relaxed);
    }

    fn fail(&self) {
        self.failed_clients.fetch_add(1, Ordering::Relaxed);
    }

    fn terminate(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    fn stats(&self) -> CheckpointAuthorityStats {
        CheckpointAuthorityStats {
            rejected_clients: self.rejected_clients.load(Ordering::Relaxed),
            failed_clients: self.failed_clients.load(Ordering::Relaxed),
        }
    }
}

enum ClientError {
    Rejected,
    Failed,
    Cancelled,
}

enum PersistError {
    TimedOut,
    Cancelled,
    StateUnavailable,
}

enum ClientOutcome {
    Continue,
    Fatal(CheckpointAuthorityRunError),
}

type ClientWorker = thread::JoinHandle<ClientOutcome>;

fn reap_finished_workers(
    workers: &mut Vec<ClientWorker>,
) -> Result<(), CheckpointAuthorityRunError> {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            join_worker(worker)?;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn join_all_workers(workers: &mut Vec<ClientWorker>) -> Result<(), CheckpointAuthorityRunError> {
    let mut failure = None;
    while let Some(worker) = workers.pop() {
        if let Err(error) = join_worker(worker) {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

fn join_worker(worker: ClientWorker) -> Result<(), CheckpointAuthorityRunError> {
    match worker.join() {
        Ok(ClientOutcome::Continue) => Ok(()),
        Ok(ClientOutcome::Fatal(error)) => Err(error),
        Err(_) => Err(CheckpointAuthorityRunError::WorkerUnavailable),
    }
}

fn persist_checkpoint(
    store: &CheckpointStore,
    expected: Option<&turso_mysql_server::AccountStoreCheckpoint>,
    replacement: &turso_mysql_server::AccountStoreCheckpoint,
    deadline: std::time::Instant,
    control: &ServiceControl,
) -> Result<CheckpointStoreCas, PersistError> {
    if control.is_shutdown() {
        return Err(PersistError::Cancelled);
    }
    store
        .compare_and_persist_until(expected, replacement, deadline, || control.is_shutdown())
        .map_err(|error| match error {
            crate::store::CheckpointStoreCasUntilError::TimedOut => PersistError::TimedOut,
            crate::store::CheckpointStoreCasUntilError::Cancelled => PersistError::Cancelled,
            crate::store::CheckpointStoreCasUntilError::Store(_) => PersistError::StateUnavailable,
        })
}

fn read_request(
    stream: &mut UnixStream,
    deadline: std::time::Instant,
    control: &ServiceControl,
) -> Result<Request, ClientError> {
    let mut header = [0; 4];
    read_exact_until(stream, &mut header, deadline, control)?;
    let payload_len = u32::from_be_bytes(header);
    if usize::try_from(payload_len).map_or(true, |length| length > MAX_FRAME_PAYLOAD_BYTES) {
        return Err(ClientError::Rejected);
    }
    let payload_len = payload_len as usize;
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&header);
    frame.resize(4 + payload_len, 0);
    read_exact_until(stream, &mut frame[4..], deadline, control)?;
    decode_request(&frame).map_err(|error| match error {
        ProtocolError::InvalidFrame
        | ProtocolError::UnsupportedVersion
        | ProtocolError::InvalidOperation
        | ProtocolError::InvalidAuthority
        | ProtocolError::InvalidCheckpoint => ClientError::Rejected,
    })
}

fn read_exact_until(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: std::time::Instant,
    control: &ServiceControl,
) -> Result<(), ClientError> {
    let mut offset = 0;
    while offset < buffer.len() {
        if control.is_shutdown() {
            return Err(ClientError::Cancelled);
        }
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => return Err(ClientError::Rejected),
            Ok(read) => offset += read,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_for_stream(stream, libc::POLLIN, deadline, control)?;
            }
            Err(error) => return Err(classify_io_error(error)),
        }
    }
    Ok(())
}

fn write_all_until(
    stream: &mut UnixStream,
    buffer: &[u8],
    deadline: std::time::Instant,
    control: &ServiceControl,
) -> Result<(), ClientError> {
    let mut offset = 0;
    while offset < buffer.len() {
        if control.is_shutdown() {
            return Err(ClientError::Cancelled);
        }
        match stream.write(&buffer[offset..]) {
            Ok(0) => return Err(ClientError::Failed),
            Ok(written) => offset += written,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_for_stream(stream, libc::POLLOUT, deadline, control)?;
            }
            Err(_) => return Err(ClientError::Failed),
        }
    }
    Ok(())
}

fn wait_for_stream(
    stream: &UnixStream,
    events: libc::c_short,
    deadline: std::time::Instant,
    control: &ServiceControl,
) -> Result<(), ClientError> {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    loop {
        if control.is_shutdown() {
            return Err(ClientError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Failed);
        }
        let timeout = duration_to_poll_millis(remaining.min(WORKER_POLL_WAIT));
        descriptor.revents = 0;
        // SAFETY: the descriptor remains owned by the caller and the pollfd is
        // one writable value for this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(ClientError::Failed);
        }
        if result == 0 {
            continue;
        }
        if descriptor.revents & events != 0 {
            return Ok(());
        }
        if descriptor.revents & (libc::POLLNVAL | libc::POLLERR | libc::POLLHUP) != 0 {
            return Err(ClientError::Failed);
        }
        return Err(ClientError::Failed);
    }
}

fn duration_to_poll_millis(duration: Duration) -> i32 {
    duration.as_millis().min(i32::MAX as u128).max(1) as i32
}

fn recover_unpublished_endpoint(
    directory: &UnixSocketDirectory,
    socket_name: &str,
    primary: CheckpointAuthorityBindError,
) -> CheckpointAuthorityBindError {
    match directory.remove_unpublished_socket(socket_name) {
        Ok(true) => primary,
        Ok(false) if directory.ensure_endpoint_absent(socket_name).is_ok() => primary,
        Ok(false) | Err(_) => CheckpointAuthorityBindError::EndpointCleanupUnavailable,
    }
}

fn recover_published_endpoint(
    directory: &UnixSocketDirectory,
    socket_name: &str,
    identity: SocketEndpointIdentity,
    primary: CheckpointAuthorityBindError,
) -> CheckpointAuthorityBindError {
    match directory.unlink_endpoint_if_matches(socket_name, identity) {
        Ok(true) => primary,
        Ok(false) if directory.ensure_endpoint_absent(socket_name).is_ok() => primary,
        Ok(false) | Err(_) => CheckpointAuthorityBindError::EndpointCleanupUnavailable,
    }
}

fn classify_io_error(error: std::io::Error) -> ClientError {
    match error.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::InvalidData => ClientError::Rejected,
        _ => ClientError::Failed,
    }
}

fn request_has_authority(request: &Request, expected: &AuthorityId) -> bool {
    match request {
        Request::Get { authority } | Request::CompareAndPersist { authority, .. } => {
            authority == expected
        }
    }
}

fn validate_absolute_path(path: &Path) -> Result<(), CheckpointAuthorityConfigError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(CheckpointAuthorityConfigError::InvalidPath);
    }
    for component in bytes.split(|byte| *byte == b'/') {
        if component == b"." || component == b".." {
            return Err(CheckpointAuthorityConfigError::InvalidPath);
        }
    }
    Ok(())
}

fn ensure_supported_platform() -> Result<(), CheckpointAuthorityBindError> {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        Ok(())
    } else {
        Err(CheckpointAuthorityBindError::UnsupportedPlatform)
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments and cannot access Rust memory.
    unsafe { libc::geteuid() }
}

fn effective_gid() -> u32 {
    // SAFETY: getegid has no pointer arguments and cannot access Rust memory.
    unsafe { libc::getegid() }
}

fn map_peer_bind_error(error: turso_mysql_server::UnixPeerError) -> CheckpointAuthorityBindError {
    match error {
        turso_mysql_server::UnixPeerError::UnsupportedPlatform => {
            CheckpointAuthorityBindError::UnsupportedPlatform
        }
        turso_mysql_server::UnixPeerError::CredentialsUnavailable
        | turso_mysql_server::UnixPeerError::EffectiveUidMismatch => {
            CheckpointAuthorityBindError::SocketUnavailable
        }
    }
}

fn map_socket_bind_error(_error: UnixSocketFsError) -> CheckpointAuthorityBindError {
    CheckpointAuthorityBindError::SocketUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixStream,
        },
        thread,
    };

    use crate::CHECKPOINT_BYTES;

    fn checkpoint(revision: u64, digest: u8) -> turso_mysql_server::AccountStoreCheckpoint {
        let mut bytes = [0; CHECKPOINT_BYTES];
        bytes[..32].fill(1);
        bytes[32..40].copy_from_slice(&revision.to_be_bytes());
        bytes[40..].fill(digest);
        turso_mysql_server::AccountStoreCheckpoint::from_bytes(&bytes).unwrap()
    }

    struct TestDirs {
        state: tempfile::TempDir,
        socket: tempfile::TempDir,
    }

    fn test_dirs() -> TestDirs {
        let state = tempfile::Builder::new()
            .prefix("ca-state-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = tempfile::Builder::new()
            .prefix("ca-socket-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        fs::set_permissions(socket.path(), fs::Permissions::from_mode(0o710)).unwrap();
        TestDirs { state, socket }
    }

    fn config_with_timeout(
        dirs: &TestDirs,
        authority: &str,
        client_uid: u32,
        io_timeout: Duration,
    ) -> CheckpointAuthorityConfig {
        CheckpointAuthorityConfig::new(
            AuthorityId::new(authority).unwrap(),
            dirs.state.path(),
            dirs.socket.path(),
            "authority.sock",
            effective_gid(),
            client_uid,
            io_timeout,
        )
        .unwrap()
    }

    fn config(dirs: &TestDirs, authority: &str, client_uid: u32) -> CheckpointAuthorityConfig {
        config_with_timeout(dirs, authority, client_uid, Duration::from_millis(100))
    }

    fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let length = u32::from_be_bytes(header) as usize;
        let mut frame = Vec::with_capacity(4 + length);
        frame.extend_from_slice(&header);
        frame.resize(4 + length, 0);
        stream.read_exact(&mut frame[4..]).unwrap();
        frame
    }

    fn request(endpoint: &Path, value: Request) -> Response {
        let mut stream = UnixStream::connect(endpoint).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(&crate::encode_request(&value).unwrap())
            .unwrap();
        crate::decode_response(&read_frame(&mut stream)).unwrap()
    }

    fn start(
        dirs: &TestDirs,
    ) -> (
        PathBuf,
        CheckpointAuthorityShutdown,
        thread::JoinHandle<Result<CheckpointAuthorityRunReport, CheckpointAuthorityRunError>>,
    ) {
        let service =
            CheckpointAuthority::bind_for_test(config(dirs, "accounts", effective_uid())).unwrap();
        let endpoint = service.socket_path().to_owned();
        let shutdown = service.shutdown_handle();
        let run = thread::spawn(move || service.run());
        (endpoint, shutdown, run)
    }

    #[test]
    fn production_bind_rejects_equal_uid() {
        let dirs = test_dirs();
        assert_eq!(
            CheckpointAuthority::bind(config(&dirs, "accounts", effective_uid())).unwrap_err(),
            CheckpointAuthorityBindError::ClientUidMatchesService
        );
    }

    #[test]
    fn same_uid_test_service_reads_and_cas_is_idempotent_and_conflicting() {
        let dirs = test_dirs();
        let (endpoint, shutdown, run) = start(&dirs);
        let first = checkpoint(0, 7);
        let second = checkpoint(1, 8);
        assert_eq!(
            request(
                &endpoint,
                Request::Get {
                    authority: AuthorityId::new("accounts").unwrap()
                }
            ),
            Response::Get(GetResponse::Missing)
        );
        assert_eq!(
            request(
                &endpoint,
                Request::CompareAndPersist {
                    authority: AuthorityId::new("accounts").unwrap(),
                    expected: None,
                    replacement: first,
                }
            ),
            Response::CompareAndPersist(CasResponse::Durable)
        );
        assert_eq!(
            request(
                &endpoint,
                Request::CompareAndPersist {
                    authority: AuthorityId::new("accounts").unwrap(),
                    expected: None,
                    replacement: first,
                }
            ),
            Response::CompareAndPersist(CasResponse::Durable)
        );
        assert_eq!(
            request(
                &endpoint,
                Request::CompareAndPersist {
                    authority: AuthorityId::new("accounts").unwrap(),
                    expected: None,
                    replacement: second,
                }
            ),
            Response::CompareAndPersist(CasResponse::Conflict)
        );
        shutdown.shutdown();
        assert!(run.join().unwrap().unwrap().endpoint_removed());
    }

    #[test]
    fn malformed_and_truncated_clients_do_not_stop_same_uid_test_service() {
        let dirs = test_dirs();
        let (endpoint, shutdown, run) = start(&dirs);
        let mut oversized = UnixStream::connect(&endpoint).unwrap();
        oversized
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        oversized.write_all(&[0, 0, 2, 1]).unwrap();
        assert_eq!(oversized.read(&mut [0; 1]).unwrap(), 0);
        let mut truncated = UnixStream::connect(&endpoint).unwrap();
        truncated
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        truncated.write_all(&[0, 0, 0, 10, b'T', b'M']).unwrap();
        assert_eq!(truncated.read(&mut [0; 1]).unwrap(), 0);
        assert_eq!(
            request(
                &endpoint,
                Request::Get {
                    authority: AuthorityId::new("accounts").unwrap()
                }
            ),
            Response::Get(GetResponse::Missing)
        );
        shutdown.shutdown();
        let report = run.join().unwrap().unwrap();
        assert!(report.stats().rejected_clients() >= 1);
        assert!(report.stats().failed_clients() >= 1);
    }

    #[test]
    fn wrong_authority_is_rejected_and_shutdown_cleans_the_endpoint() {
        let dirs = test_dirs();
        let (endpoint, shutdown, run) = start(&dirs);
        let mut stream = UnixStream::connect(&endpoint).unwrap();
        stream
            .write_all(
                &crate::encode_request(&Request::Get {
                    authority: AuthorityId::new("wrong").unwrap(),
                })
                .unwrap(),
            )
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
        shutdown.shutdown();
        let report = run.join().unwrap().unwrap();
        assert!(report.stats().rejected_clients() >= 1);
        assert!(!endpoint.exists());
        let metadata = fs::metadata(dirs.socket.path()).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o710);
    }

    #[test]
    fn fragmented_client_has_one_absolute_io_deadline() {
        let dirs = test_dirs();
        let service = CheckpointAuthority::bind_for_test(config_with_timeout(
            &dirs,
            "accounts",
            effective_uid(),
            Duration::from_millis(80),
        ))
        .unwrap();
        let endpoint = service.socket_path().to_owned();
        let shutdown = service.shutdown_handle();
        let run = thread::spawn(move || service.run());

        let mut stream = UnixStream::connect(&endpoint).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let frame = crate::encode_request(&Request::Get {
            authority: AuthorityId::new("accounts").unwrap(),
        })
        .unwrap();
        stream.write_all(&frame[..5]).unwrap();
        let writer = stream.try_clone().unwrap();
        let remaining = frame[5..].to_vec();
        let started = std::time::Instant::now();
        let writer_thread = thread::spawn(move || {
            let mut writer = writer;
            for byte in remaining {
                thread::sleep(Duration::from_millis(25));
                if writer.write_all(&[byte]).is_err() {
                    break;
                }
            }
        });
        let mut byte = [0; 1];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
        assert!(started.elapsed() < Duration::from_millis(300));
        writer_thread.join().unwrap();

        assert_eq!(
            request(
                &endpoint,
                Request::Get {
                    authority: AuthorityId::new("accounts").unwrap()
                }
            ),
            Response::Get(GetResponse::Missing)
        );
        shutdown.shutdown();
        assert!(run.join().unwrap().is_ok());
    }

    #[test]
    fn stalled_client_does_not_block_another_get() {
        let dirs = test_dirs();
        let service = CheckpointAuthority::bind_for_test(config_with_timeout(
            &dirs,
            "accounts",
            effective_uid(),
            Duration::from_millis(500),
        ))
        .unwrap();
        let endpoint = service.socket_path().to_owned();
        let shutdown = service.shutdown_handle();
        let run = thread::spawn(move || service.run());

        let mut stalled = UnixStream::connect(&endpoint).unwrap();
        stalled.write_all(&[0, 0]).unwrap();
        thread::sleep(Duration::from_millis(30));

        let started = std::time::Instant::now();
        assert_eq!(
            request(
                &endpoint,
                Request::Get {
                    authority: AuthorityId::new("accounts").unwrap()
                }
            ),
            Response::Get(GetResponse::Missing)
        );
        assert!(started.elapsed() < Duration::from_millis(300));

        shutdown.shutdown();
        assert!(run.join().unwrap().is_ok());
    }

    #[test]
    fn shutdown_recovers_stalled_workers_without_waiting_for_io_timeout() {
        let dirs = test_dirs();
        let service = CheckpointAuthority::bind_for_test(config_with_timeout(
            &dirs,
            "accounts",
            effective_uid(),
            Duration::from_secs(1),
        ))
        .unwrap();
        let endpoint = service.socket_path().to_owned();
        let shutdown = service.shutdown_handle();
        let run = thread::spawn(move || service.run());

        let mut stalled = UnixStream::connect(&endpoint).unwrap();
        stalled.write_all(&[0, 0]).unwrap();
        thread::sleep(Duration::from_millis(30));

        let started = std::time::Instant::now();
        shutdown.shutdown();
        assert!(run.join().unwrap().is_ok());
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn connection_worker_limit_rejects_excess_authenticated_clients() {
        let dirs = test_dirs();
        let service = CheckpointAuthority::bind_for_test(config_with_timeout(
            &dirs,
            "accounts",
            effective_uid(),
            Duration::from_secs(1),
        ))
        .unwrap();
        let endpoint = service.socket_path().to_owned();
        let shutdown = service.shutdown_handle();
        let run = thread::spawn(move || service.run());

        let mut stalled = Vec::with_capacity(MAX_CONNECTION_WORKERS);
        for _ in 0..MAX_CONNECTION_WORKERS {
            let mut stream = UnixStream::connect(&endpoint).unwrap();
            stream.write_all(&[0, 0]).unwrap();
            stalled.push(stream);
        }
        assert_eq!(stalled.len(), MAX_CONNECTION_WORKERS);
        thread::sleep(Duration::from_millis(100));

        let mut excess = UnixStream::connect(&endpoint).unwrap();
        excess
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        excess
            .write_all(
                &crate::encode_request(&Request::Get {
                    authority: AuthorityId::new("accounts").unwrap(),
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(excess.read(&mut [0; 1]).unwrap(), 0);

        shutdown.shutdown();
        let report = run.join().unwrap().unwrap();
        assert!(report.stats().rejected_clients() >= 1);
    }

    #[test]
    fn worker_panics_are_joined_and_fail_closed() {
        let mut workers: Vec<ClientWorker> = vec![thread::spawn(|| -> ClientOutcome {
            panic!("worker panic for recovery test")
        })];

        assert_eq!(
            join_all_workers(&mut workers),
            Err(CheckpointAuthorityRunError::WorkerUnavailable)
        );
        assert!(workers.is_empty());
    }

    #[test]
    fn corrupt_state_is_a_terminal_failure_and_cleans_the_endpoint() {
        let dirs = test_dirs();
        let authority = AuthorityId::new("accounts").unwrap();
        let store = CheckpointStore::open(&dirs.state, authority.clone()).unwrap();
        store.compare_and_persist(None, &checkpoint(0, 7)).unwrap();
        drop(store);
        let service =
            CheckpointAuthority::bind_for_test(config(&dirs, "accounts", effective_uid())).unwrap();
        let endpoint = service.socket_path().to_owned();
        let path = dirs.state.path().join(".turso-mysql-checkpoint-v1");
        let mut bytes = fs::read(&path).unwrap();
        bytes[10] ^= 1;
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let run = thread::spawn(move || service.run());

        let mut stream = UnixStream::connect(&endpoint).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(&crate::encode_request(&Request::Get { authority }).unwrap())
            .unwrap();
        let mut byte = [0; 1];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
        assert_eq!(
            run.join().unwrap(),
            Err(CheckpointAuthorityRunError::StateUnavailable)
        );
        assert!(!endpoint.exists());
    }
}
