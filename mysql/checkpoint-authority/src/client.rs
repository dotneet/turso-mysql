// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Deadline-bounded Unix client for the local checkpoint authority.

use std::{
    error::Error,
    fmt,
    mem::{self, MaybeUninit},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use turso_mysql_server::{
    AccountStoreCheckpoint, AccountStoreCheckpointAuthority, AccountStoreCheckpointReader,
    AccountStoreCheckpointRequest, CheckpointAuthorityId, CheckpointPersistence,
    CheckpointReadError, UnixPeerVerifier,
};

use crate::protocol::{
    decode_response, encode_request, AuthorityId, CasResponse, GetResponse, Request, Response,
    MAX_FRAME_PAYLOAD_BYTES,
};

const MAX_RPC_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_SOCKET_PATH_BYTES: usize = 103;
const MAX_POLL_WAIT: Duration = Duration::from_millis(10);

/// Side-effect-free client configuration for one local checkpoint authority.
#[derive(Clone)]
pub struct UnixCheckpointAuthorityClientConfig {
    socket_path: PathBuf,
    authority: AuthorityId,
    expected_service_uid: u32,
    rpc_timeout: Duration,
}

impl UnixCheckpointAuthorityClientConfig {
    /// Validates the local authority endpoint and rejects a same-UID service.
    pub fn new(
        socket_path: impl AsRef<Path>,
        authority: AuthorityId,
        expected_service_uid: u32,
        rpc_timeout: Duration,
    ) -> Result<Self, UnixCheckpointAuthorityClientConfigError> {
        Self::new_inner(
            socket_path.as_ref(),
            authority,
            expected_service_uid,
            rpc_timeout,
            false,
        )
    }

    /// Builds a same-UID configuration for tests that cannot create another OS user.
    #[cfg(test)]
    pub fn new_for_test_same_uid(
        socket_path: impl AsRef<Path>,
        authority: AuthorityId,
        rpc_timeout: Duration,
    ) -> Result<Self, UnixCheckpointAuthorityClientConfigError> {
        Self::new_inner(
            socket_path.as_ref(),
            authority,
            effective_uid(),
            rpc_timeout,
            true,
        )
    }

    fn new_inner(
        socket_path: &Path,
        authority: AuthorityId,
        expected_service_uid: u32,
        rpc_timeout: Duration,
        allow_same_uid: bool,
    ) -> Result<Self, UnixCheckpointAuthorityClientConfigError> {
        let bytes = socket_path.as_os_str().as_bytes();
        if !socket_path.is_absolute() || bytes.is_empty() || bytes.contains(&0) {
            return Err(UnixCheckpointAuthorityClientConfigError::InvalidSocketPath);
        }
        if bytes.len() > MAX_SOCKET_PATH_BYTES {
            return Err(UnixCheckpointAuthorityClientConfigError::SocketPathTooLong);
        }
        if rpc_timeout.is_zero() || rpc_timeout > MAX_RPC_TIMEOUT {
            return Err(UnixCheckpointAuthorityClientConfigError::InvalidTimeout);
        }
        if !allow_same_uid && expected_service_uid == effective_uid() {
            return Err(UnixCheckpointAuthorityClientConfigError::ServiceUidMatchesClient);
        }
        Ok(Self {
            socket_path: socket_path.to_owned(),
            authority,
            expected_service_uid,
            rpc_timeout,
        })
    }

    /// Returns the configured authority ID.
    pub fn authority(&self) -> &AuthorityId {
        &self.authority
    }

    /// Returns the absolute deadline applied to every complete RPC.
    pub const fn rpc_timeout(&self) -> Duration {
        self.rpc_timeout
    }
}

impl fmt::Debug for UnixCheckpointAuthorityClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnixCheckpointAuthorityClientConfig")
            .field("socket_path", &"<redacted>")
            .field("authority", &self.authority)
            .field("expected_service_uid", &"<redacted>")
            .field("rpc_timeout", &self.rpc_timeout)
            .finish()
    }
}

/// A rejected local checkpoint-authority client configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixCheckpointAuthorityClientConfigError {
    /// The socket path was not absolute or contained a NUL.
    InvalidSocketPath,
    /// The socket path exceeded the Linux/macOS-safe pathname limit.
    SocketPathTooLong,
    /// The RPC deadline was zero or unreasonably large.
    InvalidTimeout,
    /// Production configuration would not create an OS-user boundary.
    ServiceUidMatchesClient,
}

impl fmt::Display for UnixCheckpointAuthorityClientConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSocketPath => f.write_str("checkpoint authority socket path is invalid"),
            Self::SocketPathTooLong => f.write_str("checkpoint authority socket path is too long"),
            Self::InvalidTimeout => f.write_str("checkpoint authority RPC timeout is invalid"),
            Self::ServiceUidMatchesClient => {
                f.write_str("checkpoint authority service UID must differ from client UID")
            }
        }
    }
}

impl Error for UnixCheckpointAuthorityClientConfigError {}

/// One local checkpoint-authority client with at most one owned read worker.
pub struct UnixCheckpointAuthorityClient {
    config: UnixCheckpointAuthorityClientConfig,
    verifier: UnixPeerVerifier,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    worker_panicked: AtomicBool,
}

impl UnixCheckpointAuthorityClient {
    /// Creates a client that verifies the server's kernel-reported UID.
    pub fn new(
        config: UnixCheckpointAuthorityClientConfig,
    ) -> Result<Self, UnixCheckpointAuthorityClientError> {
        let verifier = UnixPeerVerifier::for_effective_uid(config.expected_service_uid)
            .map_err(|_| UnixCheckpointAuthorityClientError::Unavailable)?;
        Ok(Self {
            config,
            verifier,
            worker: Mutex::new(None),
            worker_panicked: AtomicBool::new(false),
        })
    }

    /// Reads the configured authority checkpoint synchronously.
    ///
    /// A live asynchronous checkpoint request owns the client worker slot, so
    /// this method rejects rather than issuing a concurrent authority read.
    pub fn get_checkpoint(
        &self,
    ) -> Result<UnixCheckpointAuthorityGet, UnixCheckpointAuthorityGetError> {
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| UnixCheckpointAuthorityGetError::Unavailable)?;
        if !self.join_finished_worker(&mut worker) || worker.is_some() {
            return Err(UnixCheckpointAuthorityGetError::Unavailable);
        }
        match rpc(
            &self.config,
            &self.verifier,
            Request::Get {
                authority: self.config.authority.clone(),
            },
            || false,
        ) {
            Ok(Response::Get(GetResponse::Checkpoint(checkpoint))) => {
                Ok(UnixCheckpointAuthorityGet::Checkpoint(checkpoint))
            }
            Ok(Response::Get(GetResponse::Missing)) => Ok(UnixCheckpointAuthorityGet::Missing),
            Ok(Response::Get(GetResponse::Invalid)) | Ok(Response::CompareAndPersist(_)) => {
                Err(UnixCheckpointAuthorityGetError::Invalid)
            }
            Err(failure) => Err(map_get_failure(failure)),
        }
    }

    fn request_get(
        &self,
        authority: &CheckpointAuthorityId,
    ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
        if authority.as_str() != self.config.authority.as_str() {
            return Err(CheckpointReadError::Unavailable);
        }
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| CheckpointReadError::Unavailable)?;
        if !self.join_finished_worker(&mut worker) || worker.is_some() {
            return Err(CheckpointReadError::Unavailable);
        }
        let config = self.config.clone();
        let verifier = UnixPeerVerifier::for_effective_uid(config.expected_service_uid)
            .map_err(|_| CheckpointReadError::Unavailable)?;
        let (response, request) = AccountStoreCheckpointRequest::channel();
        let handle = thread::Builder::new()
            .name("turso-checkpoint-read".to_owned())
            .spawn(move || {
                let result = rpc(
                    &config,
                    &verifier,
                    Request::Get {
                        authority: config.authority.clone(),
                    },
                    || response.is_cancelled(),
                );
                if response.is_cancelled() {
                    return;
                }
                let mapped = match result {
                    Ok(Response::Get(GetResponse::Checkpoint(checkpoint))) => Ok(checkpoint),
                    Ok(Response::Get(GetResponse::Missing)) => Err(CheckpointReadError::Missing),
                    Ok(Response::Get(GetResponse::Invalid)) => Err(CheckpointReadError::Invalid),
                    Ok(Response::CompareAndPersist(_)) => Err(CheckpointReadError::Invalid),
                    Err(failure) => Err(failure.read_error()),
                };
                let _ = response.complete(mapped);
            })
            .map_err(|_| CheckpointReadError::Unavailable)?;
        *worker = Some(handle);
        Ok(request)
    }

    fn compare(
        &self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
    ) -> CheckpointPersistence {
        let mut worker = match self.worker.lock() {
            Ok(worker) => worker,
            Err(_) => return CheckpointPersistence::Failed,
        };
        if !self.join_finished_worker(&mut worker) || worker.is_some() {
            return CheckpointPersistence::Failed;
        }
        match rpc(
            &self.config,
            &self.verifier,
            Request::CompareAndPersist {
                authority: self.config.authority.clone(),
                expected: expected.copied(),
                replacement: *replacement,
            },
            || false,
        ) {
            Ok(Response::CompareAndPersist(CasResponse::Durable)) => CheckpointPersistence::Durable,
            Ok(Response::CompareAndPersist(CasResponse::Conflict)) => {
                CheckpointPersistence::Conflict
            }
            Ok(Response::CompareAndPersist(CasResponse::Failed)) => CheckpointPersistence::Failed,
            Ok(Response::Get(_)) => CheckpointPersistence::Ambiguous,
            Err(failure) if !failure.write_started => CheckpointPersistence::Failed,
            Err(_) => CheckpointPersistence::Ambiguous,
        }
    }

    fn join_finished_worker(&self, worker: &mut Option<thread::JoinHandle<()>>) -> bool {
        if self.worker_panicked.load(Ordering::Acquire) {
            return false;
        }
        if worker.as_ref().is_some_and(thread::JoinHandle::is_finished) {
            let handle = worker.take().expect("finished worker handle must exist");
            if handle.join().is_err() {
                self.worker_panicked.store(true, Ordering::Release);
                return false;
            }
        }
        true
    }
}

impl AccountStoreCheckpointReader for UnixCheckpointAuthorityClient {
    fn request_checkpoint(
        &self,
        authority: &CheckpointAuthorityId,
    ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
        self.request_get(authority)
    }
}

impl AccountStoreCheckpointAuthority for UnixCheckpointAuthorityClient {
    fn serves_authority(&self, authority: &CheckpointAuthorityId) -> bool {
        authority.as_str() == self.config.authority.as_str()
    }

    fn compare_and_persist(
        &mut self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
    ) -> CheckpointPersistence {
        self.compare(expected, replacement)
    }
}

impl fmt::Debug for UnixCheckpointAuthorityClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnixCheckpointAuthorityClient")
            .field("config", &self.config)
            .field("worker", &"<owned>")
            .finish()
    }
}

impl Drop for UnixCheckpointAuthorityClient {
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = worker.take() {
            let _ = handle.join();
        }
    }
}

/// A redacted local checkpoint-authority client failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixCheckpointAuthorityClientError {
    /// The configured server UID cannot be verified on this Unix target.
    Unavailable,
}

impl fmt::Display for UnixCheckpointAuthorityClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("checkpoint authority client is unavailable")
    }
}

impl Error for UnixCheckpointAuthorityClientError {}

/// One exact checkpoint read from the local authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixCheckpointAuthorityGet {
    /// The authority holds the exact durable checkpoint.
    Checkpoint(AccountStoreCheckpoint),
    /// The authority has no checkpoint for this account store yet.
    Missing,
}

/// A redacted synchronous checkpoint-read failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixCheckpointAuthorityGetError {
    /// The complete authority RPC exceeded its configured deadline.
    TimedOut,
    /// The authority response did not match the strict local protocol.
    Invalid,
    /// The authority was unavailable, a read worker was active, or a worker panicked.
    Unavailable,
}

impl fmt::Display for UnixCheckpointAuthorityGetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => f.write_str("checkpoint authority read timed out"),
            Self::Invalid => f.write_str("checkpoint authority response is invalid"),
            Self::Unavailable => f.write_str("checkpoint authority is unavailable"),
        }
    }
}

impl Error for UnixCheckpointAuthorityGetError {}

#[derive(Debug, Clone, Copy)]
enum RpcFailureKind {
    Unavailable,
    TimedOut,
    Invalid,
    Cancelled,
}

#[derive(Debug)]
struct RpcFailure {
    kind: RpcFailureKind,
    write_started: bool,
}

impl RpcFailure {
    fn read_error(&self) -> CheckpointReadError {
        match self.kind {
            RpcFailureKind::TimedOut => CheckpointReadError::TimedOut,
            RpcFailureKind::Invalid => CheckpointReadError::Invalid,
            RpcFailureKind::Unavailable | RpcFailureKind::Cancelled => {
                CheckpointReadError::Unavailable
            }
        }
    }
}

fn map_get_failure(failure: RpcFailure) -> UnixCheckpointAuthorityGetError {
    match failure.kind {
        RpcFailureKind::TimedOut => UnixCheckpointAuthorityGetError::TimedOut,
        RpcFailureKind::Invalid => UnixCheckpointAuthorityGetError::Invalid,
        RpcFailureKind::Unavailable | RpcFailureKind::Cancelled => {
            UnixCheckpointAuthorityGetError::Unavailable
        }
    }
}

fn rpc(
    config: &UnixCheckpointAuthorityClientConfig,
    verifier: &UnixPeerVerifier,
    request: Request,
    cancelled: impl Fn() -> bool,
) -> Result<Response, RpcFailure> {
    let deadline = Instant::now() + config.rpc_timeout;
    let frame = encode_request(&request).map_err(|_| RpcFailure {
        kind: RpcFailureKind::Invalid,
        write_started: false,
    })?;
    if cancelled() {
        return Err(cancelled_failure(false));
    }
    let stream = connect(&config.socket_path, deadline, &cancelled)?;
    verifier.verify(&stream).map_err(|_| RpcFailure {
        kind: RpcFailureKind::Unavailable,
        write_started: false,
    })?;
    let mut write_started = false;
    write_all_deadline(&stream, &frame, deadline, &cancelled, &mut write_started)?;
    let response = read_response_deadline(&stream, deadline, &cancelled, write_started)?;
    if !matches_response(&request, &response) {
        return Err(RpcFailure {
            kind: RpcFailureKind::Invalid,
            write_started,
        });
    }
    Ok(response)
}

fn matches_response(request: &Request, response: &Response) -> bool {
    matches!(
        (request, response),
        (Request::Get { .. }, Response::Get(_))
            | (
                Request::CompareAndPersist { .. },
                Response::CompareAndPersist(_)
            )
    )
}

fn connect(
    path: &Path,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<UnixStream, RpcFailure> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_SOCKET_PATH_BYTES || bytes.contains(&0) {
        return Err(RpcFailure {
            kind: RpcFailureKind::Invalid,
            write_started: false,
        });
    }
    if cancelled() {
        return Err(cancelled_failure(false));
    }
    // SAFETY: socket creates a fresh file descriptor with no Rust pointers.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(unavailable_failure(false));
    }
    if configure_nonblocking_cloexec(fd).is_err() {
        // SAFETY: this branch still owns the fresh descriptor.
        unsafe { libc::close(fd) };
        return Err(unavailable_failure(false));
    }
    // SAFETY: fd is fresh and ownership moves into this UnixStream.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let address = MaybeUninit::<libc::sockaddr_un>::zeroed();
    // SAFETY: zeroed sockaddr_un is valid before filling family and sun_path.
    let mut address = unsafe { address.assume_init() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (index, byte) in bytes.iter().enumerate() {
        address.sun_path[index] = *byte as libc::c_char;
    }
    let path_len_including_nul = bytes
        .len()
        .checked_add(1)
        .ok_or_else(|| unavailable_failure(false))?;
    let address_len = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(path_len_including_nul)
        .and_then(|value| libc::socklen_t::try_from(value).ok())
        .ok_or_else(|| unavailable_failure(false))?;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = u8::try_from(address_len).map_err(|_| unavailable_failure(false))?;
    }
    // SAFETY: address points to initialized storage of exactly address_len bytes.
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_len,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error().raw_os_error();
        if !matches!(error, Some(code) if code == libc::EINPROGRESS || code == libc::EALREADY) {
            return Err(unavailable_failure(false));
        }
        wait_for_connect(stream.as_raw_fd(), deadline, cancelled)?;
    }
    let mut socket_error: libc::c_int = 0;
    let mut socket_error_len: libc::socklen_t = mem::size_of::<libc::c_int>()
        .try_into()
        .map_err(|_| unavailable_failure(false))?;
    // SAFETY: both output buffers are valid for the exact declared sizes.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut socket_error).cast::<libc::c_void>(),
            &mut socket_error_len,
        )
    };
    if result != 0 || socket_error_len != mem::size_of::<libc::c_int>() as libc::socklen_t {
        return Err(unavailable_failure(false));
    }
    if socket_error != 0 {
        return Err(unavailable_failure(false));
    }
    Ok(stream)
}

fn write_all_deadline(
    stream: &UnixStream,
    bytes: &[u8],
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    write_started: &mut bool,
) -> Result<(), RpcFailure> {
    let mut offset = 0;
    while offset < bytes.len() {
        if cancelled() {
            return Err(cancelled_failure(*write_started));
        }
        *write_started = true;
        // SAFETY: bytes is valid for its declared length and stream owns the descriptor.
        let written = unsafe {
            libc::write(
                stream.as_raw_fd(),
                bytes[offset..].as_ptr().cast::<libc::c_void>(),
                bytes.len() - offset,
            )
        };
        if written > 0 {
            *write_started = true;
            offset += usize::try_from(written).map_err(|_| unavailable_failure(*write_started))?;
            continue;
        }
        if written == 0 {
            return Err(unavailable_failure(*write_started));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => wait_for(
                stream.as_raw_fd(),
                libc::POLLOUT,
                deadline,
                cancelled,
                *write_started,
            )?,
            _ => return Err(unavailable_failure(*write_started)),
        }
    }
    Ok(())
}

fn read_response_deadline(
    stream: &UnixStream,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    write_started: bool,
) -> Result<Response, RpcFailure> {
    let mut header = [0; 4];
    read_exact_deadline(stream, &mut header, deadline, cancelled, write_started)?;
    let payload_len = usize::try_from(u32::from_be_bytes(header)).map_err(|_| RpcFailure {
        kind: RpcFailureKind::Invalid,
        write_started,
    })?;
    if payload_len > MAX_FRAME_PAYLOAD_BYTES {
        return Err(RpcFailure {
            kind: RpcFailureKind::Invalid,
            write_started,
        });
    }
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&header);
    frame.resize(4 + payload_len, 0);
    read_exact_deadline(stream, &mut frame[4..], deadline, cancelled, write_started)?;
    decode_response(&frame).map_err(|_| RpcFailure {
        kind: RpcFailureKind::Invalid,
        write_started,
    })
}

fn read_exact_deadline(
    stream: &UnixStream,
    bytes: &mut [u8],
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    write_started: bool,
) -> Result<(), RpcFailure> {
    let mut offset = 0;
    while offset < bytes.len() {
        if cancelled() {
            return Err(cancelled_failure(write_started));
        }
        // SAFETY: bytes is mutable valid storage and stream owns the descriptor.
        let read = unsafe {
            libc::read(
                stream.as_raw_fd(),
                bytes[offset..].as_mut_ptr().cast::<libc::c_void>(),
                bytes.len() - offset,
            )
        };
        if read > 0 {
            offset += usize::try_from(read).map_err(|_| unavailable_failure(write_started))?;
            continue;
        }
        if read == 0 {
            return Err(unavailable_failure(write_started));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => wait_for(
                stream.as_raw_fd(),
                libc::POLLIN,
                deadline,
                cancelled,
                write_started,
            )?,
            _ => return Err(unavailable_failure(write_started)),
        }
    }
    Ok(())
}

fn wait_for(
    fd: RawFd,
    events: libc::c_short,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    write_started: bool,
) -> Result<(), RpcFailure> {
    loop {
        if cancelled() {
            return Err(cancelled_failure(write_started));
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(RpcFailure {
                kind: RpcFailureKind::TimedOut,
                write_started,
            })?;
        let millis = remaining
            .min(MAX_POLL_WAIT)
            .as_millis()
            .clamp(1, i32::MAX as u128) as libc::c_int;
        let mut poll_fd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: poll_fd points to one initialized pollfd.
        let result = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if result > 0 {
            if poll_fd.revents & events != 0 {
                return Ok(());
            }
            if poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(unavailable_failure(write_started));
            }
            continue;
        }
        if result == 0 {
            if Instant::now() >= deadline {
                return Err(RpcFailure {
                    kind: RpcFailureKind::TimedOut,
                    write_started,
                });
            }
            continue;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Err(unavailable_failure(write_started));
        }
    }
}

fn wait_for_connect(
    fd: RawFd,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<(), RpcFailure> {
    loop {
        if cancelled() {
            return Err(cancelled_failure(false));
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(RpcFailure {
                kind: RpcFailureKind::TimedOut,
                write_started: false,
            })?;
        let millis = remaining
            .min(MAX_POLL_WAIT)
            .as_millis()
            .clamp(1, i32::MAX as u128) as libc::c_int;
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: poll_fd points to one initialized pollfd.
        let result = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if result > 0 {
            if poll_fd.revents & libc::POLLNVAL != 0 {
                return Err(unavailable_failure(false));
            }
            if poll_fd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
                return Ok(());
            }
            continue;
        }
        if result == 0 {
            if Instant::now() >= deadline {
                return Err(RpcFailure {
                    kind: RpcFailureKind::TimedOut,
                    write_started: false,
                });
            }
            continue;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Err(unavailable_failure(false));
        }
    }
}

fn cancelled_failure(write_started: bool) -> RpcFailure {
    RpcFailure {
        kind: RpcFailureKind::Cancelled,
        write_started,
    }
}

fn unavailable_failure(write_started: bool) -> RpcFailure {
    RpcFailure {
        kind: RpcFailureKind::Unavailable,
        write_started,
    }
}

fn configure_nonblocking_cloexec(fd: RawFd) -> Result<(), ()> {
    // SAFETY: fcntl reads flags from this owned descriptor.
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status < 0 {
        return Err(());
    }
    // SAFETY: fcntl updates only descriptor-local status flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK) } < 0 {
        return Err(());
    }
    // SAFETY: fcntl reads descriptor-local flags.
    let descriptor = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor < 0 {
        return Err(());
    }
    // SAFETY: fcntl updates only descriptor-local flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor | libc::FD_CLOEXEC) } < 0 {
        return Err(());
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments and reads process state only.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{ffi::OsStrExt, fs::PermissionsExt, net::UnixListener},
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc,
        },
    };

    use super::*;
    use crate::protocol::{encode_response, CHECKPOINT_BYTES};

    fn checkpoint(revision: u64, digest: u8) -> AccountStoreCheckpoint {
        let mut bytes = [0; CHECKPOINT_BYTES];
        bytes[..32].fill(1);
        bytes[32..40].copy_from_slice(&revision.to_be_bytes());
        bytes[40..].fill(digest);
        AccountStoreCheckpoint::from_bytes(&bytes).unwrap()
    }

    fn socket_path(root: &Path, name: &str) -> PathBuf {
        root.join(name)
    }

    fn longest_socket_path(root: &Path) -> PathBuf {
        let root_bytes = root.as_os_str().as_bytes().len();
        let name_len = MAX_SOCKET_PATH_BYTES
            .checked_sub(root_bytes + 1)
            .expect("temporary directory must leave room for a socket name");
        root.join("s".repeat(name_len))
    }

    fn config(path: &Path) -> UnixCheckpointAuthorityClientConfig {
        UnixCheckpointAuthorityClientConfig::new_for_test_same_uid(
            path,
            AuthorityId::new("authority").unwrap(),
            Duration::from_millis(100),
        )
        .unwrap()
    }

    fn spawn_server(
        path: PathBuf,
        action: impl FnOnce(UnixStream) + Send + 'static,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            action(stream);
        })
    }

    #[test]
    fn config_rejects_same_uid_and_bad_paths() {
        let authority = AuthorityId::new("authority").unwrap();
        assert!(matches!(
            UnixCheckpointAuthorityClientConfig::new(
                "relative.sock",
                authority.clone(),
                effective_uid().wrapping_add(1),
                Duration::from_secs(1),
            ),
            Err(UnixCheckpointAuthorityClientConfigError::InvalidSocketPath)
        ));
        assert!(matches!(
            UnixCheckpointAuthorityClientConfig::new(
                "/tmp/socket",
                authority,
                effective_uid(),
                Duration::from_secs(1),
            ),
            Err(UnixCheckpointAuthorityClientConfigError::ServiceUidMatchesClient)
        ));
    }

    #[test]
    fn crash_safe_authority_binding_matches_only_the_configured_id() {
        let root = tempfile::tempdir().unwrap();
        let client =
            UnixCheckpointAuthorityClient::new(config(&socket_path(root.path(), "sock"))).unwrap();

        assert!(client.serves_authority(&CheckpointAuthorityId::new("authority").unwrap()));
        assert!(!client.serves_authority(&CheckpointAuthorityId::new("other").unwrap()));
    }

    #[test]
    fn response_written_before_close_is_read() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = longest_socket_path(root.path());
        let server = spawn_server(path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(&encode_response(Response::Get(GetResponse::Missing)).unwrap())
                .unwrap();
        });
        let client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
        assert_eq!(
            client.get_checkpoint(),
            Ok(UnixCheckpointAuthorityGet::Missing)
        );
        server.join().unwrap();
    }

    #[test]
    fn exact_get_missing_and_malformed_responses_are_checked() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();

        let path = socket_path(root.path(), "get.sock");
        let expected = checkpoint(0, 3);
        let server = spawn_server(path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    &encode_response(Response::Get(GetResponse::Checkpoint(expected))).unwrap(),
                )
                .unwrap();
        });
        let client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
        assert_eq!(
            client.get_checkpoint(),
            Ok(UnixCheckpointAuthorityGet::Checkpoint(expected))
        );
        server.join().unwrap();

        let missing_path = socket_path(root.path(), "missing.sock");
        let server = spawn_server(missing_path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(&encode_response(Response::Get(GetResponse::Missing)).unwrap())
                .unwrap();
        });
        let client = UnixCheckpointAuthorityClient::new(config(&missing_path)).unwrap();
        assert_eq!(
            client.get_checkpoint(),
            Ok(UnixCheckpointAuthorityGet::Missing)
        );
        server.join().unwrap();

        let malformed_path = socket_path(root.path(), "malformed.sock");
        let server = spawn_server(malformed_path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(&[0, 0, 2, 1]).unwrap();
        });
        let client = UnixCheckpointAuthorityClient::new(config(&malformed_path)).unwrap();
        assert_eq!(
            client.get_checkpoint(),
            Err(UnixCheckpointAuthorityGetError::Invalid)
        );
        server.join().unwrap();
    }

    #[test]
    fn request_handoff_is_nonblocking_and_serializes_until_worker_exits() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = socket_path(root.path(), "handoff.sock");
        let (accepted_sender, accepted) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        let server = spawn_server(path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
            accepted_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            let _ =
                stream.write_all(&encode_response(Response::Get(GetResponse::Missing)).unwrap());
        });
        let mut client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
        let authority = CheckpointAuthorityId::new("authority").unwrap();
        let started = Instant::now();
        let request = client.request_checkpoint(&authority).unwrap();
        assert!(started.elapsed() < Duration::from_millis(50));
        accepted.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            client.request_checkpoint(&authority),
            Err(CheckpointReadError::Unavailable)
        ));
        assert_eq!(
            client.get_checkpoint(),
            Err(UnixCheckpointAuthorityGetError::Unavailable)
        );
        assert_eq!(
            client.compare_and_persist(None, &checkpoint(0, 4)),
            CheckpointPersistence::Failed
        );
        drop(request);
        release.send(()).unwrap();
        server.join().unwrap();
        for _ in 0..100 {
            if client.request_checkpoint(&authority).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("reader worker did not exit after cancellation");
    }

    #[test]
    fn cancellation_rechecks_a_pending_poll_promptly() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = socket_path(root.path(), "cancel.sock");
        let (accepted_sender, accepted) = mpsc::channel();
        let (cancellation_started, cancellation_observed) = mpsc::channel();
        let server = spawn_server(path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
            accepted_sender.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let config = UnixCheckpointAuthorityClientConfig::new_for_test_same_uid(
            &path,
            AuthorityId::new("authority").unwrap(),
            Duration::from_secs(1),
        )
        .unwrap();
        let client = UnixCheckpointAuthorityClient::new(config).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::clone(&cancelled);
        let canceller = thread::spawn(move || {
            accepted.recv().unwrap();
            thread::sleep(Duration::from_millis(20));
            cancellation.store(true, Ordering::Release);
            cancellation_started.send(()).unwrap();
        });
        let started = Instant::now();
        let failure = rpc(
            &client.config,
            &client.verifier,
            Request::Get {
                authority: client.config.authority.clone(),
            },
            || cancelled.load(Ordering::Acquire),
        )
        .unwrap_err();
        cancellation_observed
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(failure.kind, RpcFailureKind::Cancelled));
        assert!(started.elapsed() < Duration::from_millis(250));
        canceller.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn reader_worker_panic_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = socket_path(root.path(), "panic.sock");
        let mut client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
        *client.worker.lock().unwrap() = Some(thread::spawn(|| panic!("reader worker panic")));
        let mut finished = false;
        for _ in 0..100 {
            if client
                .worker
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished)
            {
                finished = true;
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(finished, "reader worker did not panic in time");
        let authority = CheckpointAuthorityId::new("authority").unwrap();
        assert!(matches!(
            client.request_checkpoint(&authority),
            Err(CheckpointReadError::Unavailable)
        ));
        assert_eq!(
            client.get_checkpoint(),
            Err(UnixCheckpointAuthorityGetError::Unavailable)
        );
        assert_eq!(
            client.compare_and_persist(None, &checkpoint(0, 5)),
            CheckpointPersistence::Failed
        );
    }

    #[test]
    fn oversized_truncated_and_deadline_responses_fail_safely() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        for (name, response, expected) in [
            (
                "oversized.sock",
                vec![0, 0, 2, 1],
                UnixCheckpointAuthorityGetError::Invalid,
            ),
            (
                "truncated.sock",
                vec![0, 0, 0, 8, 1, 2],
                UnixCheckpointAuthorityGetError::Unavailable,
            ),
        ] {
            let path = socket_path(root.path(), name);
            let server = spawn_server(path.clone(), move |mut stream| {
                let mut request = [0; 512];
                let _ = stream.read(&mut request).unwrap();
                stream.write_all(&response).unwrap();
            });
            let client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
            assert_eq!(client.get_checkpoint(), Err(expected));
            server.join().unwrap();
        }

        let path = socket_path(root.path(), "deadline.sock");
        let server = spawn_server(path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
            thread::sleep(Duration::from_millis(150));
        });
        let client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
        assert_eq!(
            client.get_checkpoint(),
            Err(UnixCheckpointAuthorityGetError::TimedOut)
        );
        server.join().unwrap();
    }

    #[test]
    fn wrong_server_uid_is_rejected_before_a_request_is_sent() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = socket_path(root.path(), "wrong-uid.sock");
        let server = spawn_server(path.clone(), |_| {});
        let config = UnixCheckpointAuthorityClientConfig::new(
            &path,
            AuthorityId::new("authority").unwrap(),
            effective_uid().wrapping_add(1),
            Duration::from_millis(100),
        )
        .unwrap();
        let client = UnixCheckpointAuthorityClient::new(config).unwrap();
        let failure = rpc(
            &client.config,
            &client.verifier,
            Request::Get {
                authority: client.config.authority.clone(),
            },
            || false,
        )
        .unwrap_err();
        assert!(matches!(failure.kind, RpcFailureKind::Unavailable));
        assert!(!failure.write_started);
        server.join().unwrap();
    }

    #[test]
    fn cas_maps_explicit_results_and_lost_reply_conservatively() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        for (name, response, expected) in [
            (
                "durable.sock",
                Response::CompareAndPersist(CasResponse::Durable),
                CheckpointPersistence::Durable,
            ),
            (
                "conflict.sock",
                Response::CompareAndPersist(CasResponse::Conflict),
                CheckpointPersistence::Conflict,
            ),
            (
                "failed.sock",
                Response::CompareAndPersist(CasResponse::Failed),
                CheckpointPersistence::Failed,
            ),
        ] {
            let path = socket_path(root.path(), name);
            let server = spawn_server(path.clone(), move |mut stream| {
                let mut request = [0; 512];
                let _ = stream.read(&mut request).unwrap();
                stream
                    .write_all(&encode_response(response).unwrap())
                    .unwrap();
            });
            let mut client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
            assert_eq!(
                client.compare_and_persist(None, &checkpoint(0, 1)),
                expected
            );
            server.join().unwrap();
        }

        let path = socket_path(root.path(), "lost.sock");
        let server = spawn_server(path.clone(), move |mut stream| {
            let mut request = [0; 512];
            let _ = stream.read(&mut request).unwrap();
        });
        let mut client = UnixCheckpointAuthorityClient::new(config(&path)).unwrap();
        assert_eq!(
            client.compare_and_persist(None, &checkpoint(0, 1)),
            CheckpointPersistence::Ambiguous
        );
        server.join().unwrap();

        let missing_path = socket_path(root.path(), "before-send.sock");
        let mut client = UnixCheckpointAuthorityClient::new(config(&missing_path)).unwrap();
        assert_eq!(
            client.compare_and_persist(None, &checkpoint(0, 1)),
            CheckpointPersistence::Failed
        );
    }
}
