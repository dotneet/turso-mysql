//! Blocking protocol ownership for one peer-verified Unix connection.

use std::{
    error::Error,
    fmt,
    io::{Read, Write},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use turso_mysql::schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext};

use crate::{
    AcceptedUnixStream, AuthorizedDatabaseAdapterFactory, CachingSha2Verifier,
    ClassicConnectionOrchestrator, ClassicFrame, InitialHandshakeSettings, OrchestratorError,
    OrchestratorEvent, PacketCodec, PacketCodecError, PacketStreamDecoder,
    RuntimeUnixListenerError, StreamDecoderError, TransportSecurity, CLIENT_SSL,
    MAX_COMMAND_PAYLOAD_LENGTH, SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
};

const READ_BUFFER_BYTES: usize = MAX_COMMAND_PAYLOAD_LENGTH;
const MAX_PACKETS_PER_FEED: usize = 16;
const MAX_INPUT_BYTES_PER_FEED: usize = MAX_PACKETS_PER_FEED * crate::PACKET_HEADER_LEN;

/// One background owner for an accepted Unix protocol connection.
#[must_use = "a protocol worker must be joined so connection failure is observed"]
pub struct RuntimeUnixConnectionWorker {
    connection_id: u32,
    handle: thread::JoinHandle<Result<(), RuntimeUnixConnectionError>>,
}

impl RuntimeUnixConnectionWorker {
    /// Returns the nonzero ID assigned to this live protocol connection.
    pub const fn connection_id(&self) -> u32 {
        self.connection_id
    }

    /// Returns whether the protocol owner has stopped.
    pub(crate) fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Waits for the connection owner and reports a failure without retaining
    /// a panic payload.
    pub fn join(self) -> Result<(), RuntimeUnixConnectionWorkerError> {
        match self.handle.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(RuntimeUnixConnectionWorkerError::Connection(error)),
            Err(_) => Err(RuntimeUnixConnectionWorkerError::Panicked),
        }
    }
}

impl fmt::Debug for RuntimeUnixConnectionWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeUnixConnectionWorker")
            .field("connection_id", &"<redacted>")
            .field("handle", &"<redacted>")
            .finish()
    }
}

/// A background Unix connection ended without exposing transport or panic details.
#[derive(Debug)]
pub enum RuntimeUnixConnectionWorkerError {
    /// The owned protocol connection returned an ordinary terminal error.
    Connection(RuntimeUnixConnectionError),
    /// The protocol owner panicked; its payload is intentionally discarded.
    Panicked,
}

impl fmt::Display for RuntimeUnixConnectionWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(error) => write!(f, "Unix protocol worker failed: {error}"),
            Self::Panicked => f.write_str("Unix protocol worker panicked"),
        }
    }
}

impl Error for RuntimeUnixConnectionWorkerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Connection(error) => Some(error),
            Self::Panicked => None,
        }
    }
}

/// Accepting or spawning the Unix protocol owner failed without exposing socket details.
#[derive(Debug)]
pub enum RuntimeUnixConnectionSpawnError {
    /// The listener rejected or could not admit a Unix stream.
    Accept(RuntimeUnixListenerError),
    /// The accepted stream could not be assigned a background owner.
    SpawnUnavailable,
}

impl fmt::Display for RuntimeUnixConnectionSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept(error) => write!(f, "Unix protocol accept failed: {error}"),
            Self::SpawnUnavailable => f.write_str("Unix protocol worker could not start"),
        }
    }
}

impl Error for RuntimeUnixConnectionSpawnError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept(error) => Some(error),
            Self::SpawnUnavailable => None,
        }
    }
}

impl crate::RuntimeUnixListener {
    /// Blocks for one accepted peer and gives it an isolated protocol owner.
    pub fn accept_and_spawn_protocol(
        &self,
    ) -> Result<RuntimeUnixConnectionWorker, RuntimeUnixConnectionSpawnError> {
        let stream = self
            .accept()
            .map_err(RuntimeUnixConnectionSpawnError::Accept)?;
        self.spawn_protocol(stream, || {})
    }

    /// Gives an accepted stream to a protocol owner that reports completion
    /// through a callback.
    pub(crate) fn spawn_protocol<F>(
        &self,
        stream: AcceptedUnixStream,
        completion: F,
    ) -> Result<RuntimeUnixConnectionWorker, RuntimeUnixConnectionSpawnError>
    where
        F: FnOnce() + Send + 'static,
    {
        let connection_id = stream.connection_id();
        spawn_protocol_worker(connection_id, completion, move || {
            run_unix_connection(stream)
        })
    }
}

fn spawn_protocol_worker<F, R>(
    connection_id: u32,
    completion: F,
    run: R,
) -> Result<RuntimeUnixConnectionWorker, RuntimeUnixConnectionSpawnError>
where
    F: FnOnce() + Send + 'static,
    R: FnOnce() -> Result<(), RuntimeUnixConnectionError> + Send + 'static,
{
    let handle = thread::Builder::new()
        .name(format!("turso-mysql-{connection_id}"))
        .spawn(move || {
            let _completion = CompletionGuard::new(completion);
            run()
        })
        .map_err(|_| RuntimeUnixConnectionSpawnError::SpawnUnavailable)?;
    Ok(RuntimeUnixConnectionWorker {
        connection_id,
        handle,
    })
}

struct CompletionGuard<F: FnOnce()> {
    completion: Option<F>,
}

impl<F: FnOnce()> CompletionGuard<F> {
    fn new(completion: F) -> Self {
        Self {
            completion: Some(completion),
        }
    }
}

impl<F: FnOnce()> Drop for CompletionGuard<F> {
    fn drop(&mut self) {
        (self
            .completion
            .take()
            .expect("completion callback must be present until its guard drops"))();
    }
}

fn run_unix_connection(mut stream: AcceptedUnixStream) -> Result<(), RuntimeUnixConnectionError> {
    run_unix_connection_with_before_frame(&mut stream, || {})
}

fn run_unix_connection_with_before_frame<F>(
    stream: &mut AcceptedUnixStream,
    before_frame: F,
) -> Result<(), RuntimeUnixConnectionError>
where
    F: FnMut(),
{
    let limits = stream.limits();
    let timeouts = stream.timeouts();
    let settings = unix_handshake_settings(stream.connection_id());
    let verifier = CachingSha2Verifier::new(stream.account_store());
    let factory = AuthorizedDatabaseAdapterFactory::new(
        stream.catalog(),
        binary_schema_context(),
        stream.account_store(),
    )
    .with_query_timeout(timeouts.query());
    let mut orchestrator = ClassicConnectionOrchestrator::with_transport_security(
        settings,
        TransportSecurity::Secure,
        verifier,
        factory,
        limits.max_write_bytes(),
        limits.max_write_frames(),
    )
    .map_err(RuntimeUnixConnectionError::Orchestrator)?;

    let authentication_deadline = stream.authentication_deadline();
    let result = (|| {
        stream
            .begin_protocol_work()
            .map_err(RuntimeUnixConnectionError::Listener)?;
        orchestrator
            .start()
            .map_err(RuntimeUnixConnectionError::Orchestrator)?;
        flush_writes(
            stream,
            &mut orchestrator,
            bounded_write_deadline(authentication_deadline, timeouts.write()),
        )?;
        run_inner(
            stream,
            &mut orchestrator,
            authentication_deadline,
            timeouts.idle(),
            timeouts.write(),
            before_frame,
        )
    })();
    orchestrator
        .transport_closed()
        .expect("every terminal Unix owner state must accept transport closure");
    result
}

fn run_inner(
    stream: &mut AcceptedUnixStream,
    orchestrator: &mut UnixOrchestrator,
    authentication_deadline: Instant,
    idle_timeout: Duration,
    write_timeout: Duration,
    mut before_frame: impl FnMut(),
) -> Result<(), RuntimeUnixConnectionError> {
    let codec = PacketCodec::new(MAX_COMMAND_PAYLOAD_LENGTH)
        .map_err(RuntimeUnixConnectionError::PacketCodec)?;
    let mut decoder =
        PacketStreamDecoder::new(codec, MAX_COMMAND_PAYLOAD_LENGTH, MAX_PACKETS_PER_FEED)
            .expect("the fixed Unix stream decoder bounds are valid");
    let mut read_deadline = authentication_deadline;
    let mut admission_complete = false;

    let mut buffer = [0; READ_BUFFER_BYTES];
    loop {
        let read = loop {
            set_read_deadline(stream, read_deadline, admission_complete)?;
            match stream.read(&mut buffer) {
                Ok(read) => break read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(read_error(error, admission_complete)),
            }
        };
        if read == 0 {
            if decoder.has_partial_frame() {
                return Err(RuntimeUnixConnectionError::TruncatedFrame);
            }
            return Ok(());
        }

        // Even when a previous feed left one packet a byte from completion,
        // this chunk size cannot complete more than MAX_PACKETS_PER_FEED frames.
        for chunk in buffer[..read].chunks(MAX_INPUT_BYTES_PER_FEED) {
            let packets = decoder
                .feed(chunk)
                .map_err(RuntimeUnixConnectionError::StreamDecoder)?;
            for packet in packets {
                before_frame();
                stream
                    .begin_protocol_work()
                    .map_err(RuntimeUnixConnectionError::Listener)?;
                let frame = ClassicFrame::from_payload(codec, packet.sequence_id, &packet.payload)
                    .map_err(RuntimeUnixConnectionError::PacketCodec)?;
                let event = orchestrator
                    .receive_frame(frame)
                    .map_err(RuntimeUnixConnectionError::Orchestrator)?;
                let write_deadline = if admission_complete {
                    Instant::now() + write_timeout
                } else {
                    bounded_write_deadline(authentication_deadline, write_timeout)
                };
                flush_writes(stream, orchestrator, write_deadline)?;

                match event {
                    OrchestratorEvent::Ready => {
                        if !admission_complete {
                            stream
                                .complete_admission()
                                .map_err(RuntimeUnixConnectionError::Listener)?;
                            admission_complete = true;
                        }
                        // A complete, flushed command marks the start of a new
                        // idle period. Partial packets never extend this deadline.
                        read_deadline = Instant::now() + idle_timeout;
                    }
                    OrchestratorEvent::AwaitingClientFrame => {}
                    OrchestratorEvent::Closing | OrchestratorEvent::Closed => return Ok(()),
                    OrchestratorEvent::TlsUpgradeRequired => {
                        return Err(RuntimeUnixConnectionError::UnexpectedTlsUpgrade)
                    }
                }
            }
        }
    }
}

type UnixFactory = AuthorizedDatabaseAdapterFactory<crate::RuntimeAccountStore>;
type UnixOrchestrator = ClassicConnectionOrchestrator<Arc<crate::RuntimeAccountStore>, UnixFactory>;

fn unix_handshake_settings(connection_id: u32) -> InitialHandshakeSettings {
    assert_ne!(
        connection_id, 0,
        "accepted Unix connections need non-zero IDs"
    );
    InitialHandshakeSettings {
        connection_id,
        capability_flags: SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES & !CLIENT_SSL,
        ..InitialHandshakeSettings::default()
    }
}

fn binary_schema_context() -> SchemaSqlSessionContext {
    SchemaSqlSessionContext {
        sql_mode: SchemaSqlMode {
            ansi_quotes: false,
            no_backslash_escapes: false,
        },
        character_set_client: CharacterSet::Binary,
        collation_connection: Collation::Binary,
        default_character_set: CharacterSet::Binary,
        default_collation: Collation::Binary,
    }
}

fn flush_writes(
    stream: &mut AcceptedUnixStream,
    orchestrator: &mut UnixOrchestrator,
    deadline: Instant,
) -> Result<(), RuntimeUnixConnectionError> {
    while orchestrator.front_write().is_some() {
        let remaining = remaining_until(deadline, DeadlineKind::Write)?;
        stream
            .set_write_timeout(remaining)
            .map_err(RuntimeUnixConnectionError::Listener)?;
        let written = {
            let frame = orchestrator
                .front_write()
                .expect("a queued frame remained until the write call");
            match stream.write(frame) {
                Ok(written) => written,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(write_error(error)),
            }
        };
        orchestrator
            .advance_write(written)
            .map_err(RuntimeUnixConnectionError::Orchestrator)?;
    }
    Ok(())
}

fn bounded_write_deadline(phase_deadline: Instant, write_timeout: Duration) -> Instant {
    phase_deadline.min(Instant::now() + write_timeout)
}

fn set_read_deadline(
    stream: &mut AcceptedUnixStream,
    deadline: Instant,
    admission_complete: bool,
) -> Result<(), RuntimeUnixConnectionError> {
    let remaining = remaining_until(
        deadline,
        if admission_complete {
            DeadlineKind::Idle
        } else {
            DeadlineKind::Authentication
        },
    )?;
    stream
        .set_read_timeout(remaining)
        .map_err(RuntimeUnixConnectionError::Listener)
}

fn remaining_until(
    deadline: Instant,
    kind: DeadlineKind,
) -> Result<Duration, RuntimeUnixConnectionError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| kind.exceeded())
}

fn read_error(error: std::io::Error, admission_complete: bool) -> RuntimeUnixConnectionError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        return if admission_complete {
            RuntimeUnixConnectionError::IdleDeadlineExceeded
        } else {
            RuntimeUnixConnectionError::AuthenticationDeadlineExceeded
        };
    }
    RuntimeUnixConnectionError::Read(error)
}

fn write_error(error: std::io::Error) -> RuntimeUnixConnectionError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        RuntimeUnixConnectionError::WriteDeadlineExceeded
    } else {
        RuntimeUnixConnectionError::Write(error)
    }
}

#[derive(Clone, Copy)]
enum DeadlineKind {
    Authentication,
    Idle,
    Write,
}

impl DeadlineKind {
    fn exceeded(self) -> RuntimeUnixConnectionError {
        match self {
            Self::Authentication => RuntimeUnixConnectionError::AuthenticationDeadlineExceeded,
            Self::Idle => RuntimeUnixConnectionError::IdleDeadlineExceeded,
            Self::Write => RuntimeUnixConnectionError::WriteDeadlineExceeded,
        }
    }
}

/// A Unix connection ended without exposing peer, account, or catalog details.
#[derive(Debug)]
pub enum RuntimeUnixConnectionError {
    /// The listener could not configure or finish its owned stream.
    Listener(RuntimeUnixListenerError),
    /// A fixed protocol packet bound was invalid.
    PacketCodec(PacketCodecError),
    /// Incremental framing rejected the client stream.
    StreamDecoder(StreamDecoderError),
    /// The protocol state machine or command adapter rejected the stream.
    Orchestrator(OrchestratorError),
    /// The peer disconnected in the middle of a packet.
    TruncatedFrame,
    /// The client attempted TLS on this already-secure Unix-only endpoint.
    UnexpectedTlsUpgrade,
    /// Authentication did not finish by its original deadline.
    AuthenticationDeadlineExceeded,
    /// A command did not arrive by its original idle deadline.
    IdleDeadlineExceeded,
    /// A complete queued response did not drain by its original deadline.
    WriteDeadlineExceeded,
    /// A terminal socket read failed.
    Read(std::io::Error),
    /// A terminal socket write failed.
    Write(std::io::Error),
}

impl fmt::Display for RuntimeUnixConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listener(error) => {
                write!(f, "Unix connection listener operation failed: {error}")
            }
            Self::PacketCodec(error) => write!(f, "Unix connection packet codec failed: {error}"),
            Self::StreamDecoder(error) => {
                write!(f, "Unix connection stream decoder failed: {error}")
            }
            Self::Orchestrator(error) => write!(f, "Unix connection protocol failed: {error}"),
            Self::TruncatedFrame => f.write_str("Unix connection closed during a packet"),
            Self::UnexpectedTlsUpgrade => f.write_str("Unix connection requested unsupported TLS"),
            Self::AuthenticationDeadlineExceeded => {
                f.write_str("Unix connection authentication timed out")
            }
            Self::IdleDeadlineExceeded => f.write_str("Unix connection idle deadline elapsed"),
            Self::WriteDeadlineExceeded => f.write_str("Unix connection write deadline elapsed"),
            Self::Read(error) => write!(f, "Unix connection read failed: {error}"),
            Self::Write(error) => write!(f, "Unix connection write failed: {error}"),
        }
    }
}

impl Error for RuntimeUnixConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Listener(error) => Some(error),
            Self::PacketCodec(error) => Some(error),
            Self::StreamDecoder(error) => Some(error),
            Self::Orchestrator(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Write(error) => Some(error),
            Self::TruncatedFrame
            | Self::UnexpectedTlsUpgrade
            | Self::AuthenticationDeadlineExceeded
            | Self::IdleDeadlineExceeded
            | Self::WriteDeadlineExceeded => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixStream},
        path::Path,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Barrier, Mutex,
        },
        thread,
        time::Duration,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{
        AccountStoreCheckpoint, AccountStoreCheckpointAuthority, AccountStoreCheckpointRequest,
        AuthMoreData, AuthMoreDataKind, AuthOkPacket, CheckpointAuthorityId, CheckpointPersistence,
        CheckpointReadError, ClientHandshakeResponseConfig, DatabasePrivileges, GlobalPrivileges,
        InitialHandshake, OfflineAccountProvisioner, ProtectedPassword, ResultTerminatorPacket,
        RuntimeConfig, RuntimeLimits, RuntimeTimeouts, RuntimeUnixListener, TextRowPacket,
        TextRowValue, UnixSocketConfig, CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_CONNECT_WITH_DB,
        CLIENT_DEPRECATE_EOF, COMMAND_SEQUENCE_ID, COM_PING, COM_QUERY, COM_QUIT,
        DEFAULT_UTF8MB4_COLLATION, MIN_WRITE_LIMIT, PACKET_HEADER_LEN,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };
    use turso_mysql::MySqlDatabaseCatalog;

    #[test]
    fn unix_handshake_is_secure_without_tls_upgrade_capability() {
        let settings = unix_handshake_settings(7);

        assert_eq!(settings.connection_id, 7);
        assert_eq!(settings.capability_flags & CLIENT_SSL, 0);
        assert_eq!(
            settings.capability_flags,
            SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES & !CLIENT_SSL
        );
    }

    #[test]
    fn expired_deadlines_fail_before_another_socket_operation() {
        let expired = Instant::now() - Duration::from_millis(1);

        assert!(matches!(
            remaining_until(expired, DeadlineKind::Authentication),
            Err(RuntimeUnixConnectionError::AuthenticationDeadlineExceeded)
        ));
        assert!(matches!(
            remaining_until(expired, DeadlineKind::Idle),
            Err(RuntimeUnixConnectionError::IdleDeadlineExceeded)
        ));
        assert!(matches!(
            remaining_until(expired, DeadlineKind::Write),
            Err(RuntimeUnixConnectionError::WriteDeadlineExceeded)
        ));
    }

    #[test]
    fn socket_timeout_errors_keep_the_phase_that_owns_the_deadline() {
        let timeout = || std::io::Error::from(std::io::ErrorKind::TimedOut);

        assert!(matches!(
            read_error(timeout(), false),
            RuntimeUnixConnectionError::AuthenticationDeadlineExceeded
        ));
        assert!(matches!(
            read_error(timeout(), true),
            RuntimeUnixConnectionError::IdleDeadlineExceeded
        ));
        assert!(matches!(
            write_error(timeout()),
            RuntimeUnixConnectionError::WriteDeadlineExceeded
        ));
    }

    #[test]
    fn protocol_worker_completion_runs_once_after_normal_return() {
        let completions = Arc::new(AtomicUsize::new(0));
        let callback_completions = Arc::clone(&completions);
        let worker = spawn_protocol_worker(
            7,
            move || {
                callback_completions.fetch_add(1, Ordering::SeqCst);
            },
            || Ok(()),
        )
        .unwrap();

        while !worker.is_finished() {
            thread::yield_now();
        }
        assert!(matches!(worker.join(), Ok(())));
        assert_eq!(completions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn protocol_worker_completion_runs_once_when_worker_panics() {
        let completions = Arc::new(AtomicUsize::new(0));
        let callback_completions = Arc::clone(&completions);
        let worker = spawn_protocol_worker(
            8,
            move || {
                callback_completions.fetch_add(1, Ordering::SeqCst);
            },
            || -> Result<(), RuntimeUnixConnectionError> {
                panic!("protocol worker test panic");
            },
        )
        .unwrap();

        while !worker.is_finished() {
            thread::yield_now();
        }
        assert!(matches!(
            worker.join(),
            Err(RuntimeUnixConnectionWorkerError::Panicked)
        ));
        assert_eq!(completions.load(Ordering::SeqCst), 1);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct TestCheckpointReader {
        results: Mutex<VecDeque<Result<AccountStoreCheckpoint, CheckpointReadError>>>,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl TestCheckpointReader {
        fn new(
            results: impl IntoIterator<Item = Result<AccountStoreCheckpoint, CheckpointReadError>>,
        ) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl crate::AccountStoreCheckpointReader for TestCheckpointReader {
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[derive(Default)]
    struct TestAuthority {
        checkpoint: Option<AccountStoreCheckpoint>,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl AccountStoreCheckpointAuthority for TestAuthority {
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn private_directory() -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn protocol_runtime(
        authentication: Duration,
        idle: Duration,
        query: Duration,
        shutdown: Duration,
    ) -> (
        RuntimeUnixListener,
        TempDir,
        TempDir,
        TempDir,
        std::path::PathBuf,
    ) {
        let data_root = private_directory();
        let account_root = private_directory();
        let socket_directory = private_directory();

        let mut password = b"secret".to_vec();
        let account = crate::provision_account(
            "alice",
            ProtectedPassword::new(password.as_mut_slice()),
            true,
            GlobalPrivileges::new(true, false),
        )
        .unwrap();
        let testdb_grant =
            account.grant("testdb", DatabasePrivileges::new(true, true, false, false));
        let blocked_grant =
            account.grant("blocked", DatabasePrivileges::new(true, true, true, true));
        let mut authority = TestAuthority::default();
        let provisioner = OfflineAccountProvisioner::initialize(
            account_root.path(),
            account
                .into_builder()
                .with_grant(testdb_grant)
                .with_grant(blocked_grant),
            &mut authority,
        )
        .unwrap();
        let checkpoint = provisioner.checkpoint().unwrap();
        drop(provisioner);

        let catalog = MySqlDatabaseCatalog::open(data_root.path()).unwrap();
        catalog.create("testdb").unwrap();
        drop(catalog);

        let limits = RuntimeLimits::new(4, 4, MIN_WRITE_LIMIT, 16).unwrap();
        let socket = UnixSocketConfig::new(
            socket_directory.path().canonicalize().unwrap(),
            "mysql.sock",
        )
        .unwrap();
        let config = RuntimeConfig::new(
            None,
            Some(socket),
            data_root.path().canonicalize().unwrap(),
            account_root.path().canonicalize().unwrap(),
            CheckpointAuthorityId::new("runtime-checkpoints").unwrap(),
            Duration::from_secs(1),
            limits,
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                authentication,
                idle,
                Duration::from_secs(1),
                shutdown,
            )
            .unwrap()
            .with_query_timeout(query)
            .unwrap(),
        )
        .unwrap();
        let endpoint = config.unix_socket().unwrap().socket_path();
        let reader = Arc::new(TestCheckpointReader::new([Ok(checkpoint), Ok(checkpoint)]));
        let listener = RuntimeUnixListener::bind(&config, reader).unwrap();
        (
            listener,
            data_root,
            account_root,
            socket_directory,
            endpoint,
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn packet_codec() -> PacketCodec {
        PacketCodec::new(MAX_COMMAND_PAYLOAD_LENGTH).unwrap()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let payload_length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        assert!(payload_length <= MAX_COMMAND_PAYLOAD_LENGTH);
        let mut frame = vec![0; PACKET_HEADER_LEN + payload_length];
        frame[..PACKET_HEADER_LEN].copy_from_slice(&header);
        stream.read_exact(&mut frame[PACKET_HEADER_LEN..]).unwrap();
        frame
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn start_worker(
        listener: &RuntimeUnixListener,
        endpoint: &Path,
    ) -> (UnixStream, RuntimeUnixConnectionWorker) {
        let client = UnixStream::connect(endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = listener.accept_and_spawn_protocol().unwrap();
        (client, worker)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn client_handshake(
        client: &mut UnixStream,
        database: Option<&str>,
        capabilities: u32,
    ) -> InitialHandshake {
        let codec = packet_codec();
        let handshake_frame = read_frame(client);
        let handshake = InitialHandshake::decode(codec, &handshake_frame).unwrap();
        assert_eq!(handshake.sequence_id, 0);
        assert_ne!(handshake.connection_id, 0);
        assert_eq!(handshake.capability_flags() & CLIENT_SSL, 0);

        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | capabilities,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "alice",
            vec![0; 32],
            database.map(str::to_owned),
            Some(CACHING_SHA2_PASSWORD_PLUGIN.to_owned()),
            None,
        )
        .encode(codec, 1)
        .unwrap();
        client.write_all(&response).unwrap();

        let auth_more_frame = read_frame(client);
        let auth_more = AuthMoreData::decode(codec, &auth_more_frame).unwrap();
        assert_eq!(auth_more.sequence_id, 2);
        assert_eq!(auth_more.kind, AuthMoreDataKind::FullAuthenticationRequired);
        client
            .write_all(&codec.encode(3, b"secret\0").unwrap())
            .unwrap();

        let auth_ok_frame = read_frame(client);
        let auth_ok = AuthOkPacket::decode(codec, &auth_ok_frame).unwrap();
        assert_eq!(auth_ok.sequence_id, 4);
        handshake
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_runs_full_auth_database_query_ping_and_quit() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        assert_ne!(worker.connection_id(), 0);
        assert_eq!(
            format!("{worker:?}"),
            "RuntimeUnixConnectionWorker { connection_id: \"<redacted>\", handle: \"<redacted>\" }"
        );
        let capabilities = CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF;
        let handshake = client_handshake(&mut client, Some("testdb"), capabilities);
        assert_eq!(handshake.capability_flags() & CLIENT_SSL, 0);

        let codec = packet_codec();
        let mut query_payload = vec![COM_QUERY];
        query_payload.extend_from_slice(b"SELECT 1");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &query_payload).unwrap())
            .unwrap();

        let count_frame = read_frame(&mut client);
        let count = crate::ColumnCountPacket::decode(codec, &count_frame).unwrap();
        assert_eq!(count.sequence_id, 1);
        assert_eq!(count.column_count, 1);
        let definition = read_frame(&mut client);
        assert_eq!(
            crate::ColumnDefinitionPacket::decode(codec, &definition)
                .unwrap()
                .sequence_id,
            2
        );
        let row_frame = read_frame(&mut client);
        let row = TextRowPacket::decode(codec, &row_frame, 1).unwrap();
        assert_eq!(row.sequence_id, 3);
        assert_eq!(row.values, vec![TextRowValue::Bytes(b"1")]);
        let terminator = read_frame(&mut client);
        assert!(matches!(
            ResultTerminatorPacket::decode(
                codec,
                &terminator,
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | capabilities,
            )
            .unwrap(),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 4
        ));

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_PING]).unwrap())
            .unwrap();
        let ping = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(ping.sequence_id, 1);

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn query_timeout_returns_a_bounded_error_and_keeps_the_connection_ready() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_nanos(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        client_handshake(&mut client, Some("testdb"), CLIENT_CONNECT_WITH_DB);

        let codec = packet_codec();
        let mut query = vec![COM_QUERY];
        query.extend_from_slice(b"SELECT 1");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &query).unwrap())
            .unwrap();
        let error = crate::ErrPacket::decode(
            codec,
            &read_frame(&mut client),
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap();
        assert_eq!(error.error_code, 3024);
        assert_eq!(error.sql_state, Some(*b"HY000"));

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_PING]).unwrap())
            .unwrap();
        assert_eq!(
            crate::ResponseOkPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            1
        );
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_accepts_fragmented_and_coalesced_packets() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let codec = packet_codec();
        let _handshake = read_frame(&mut client);
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "alice",
            vec![0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN.to_owned()),
            None,
        )
        .encode(codec, 1)
        .unwrap();
        client.write_all(&response[..2]).unwrap();
        thread::sleep(Duration::from_millis(10));
        client.write_all(&response[2..]).unwrap();
        let auth_more = read_frame(&mut client);
        assert_eq!(
            AuthMoreData::decode(codec, &auth_more).unwrap().sequence_id,
            2
        );

        let full = codec.encode(3, b"secret\0").unwrap();
        let ping = codec.encode(COMMAND_SEQUENCE_ID, &[COM_PING]).unwrap();
        let mut coalesced = full;
        coalesced.extend_from_slice(&ping);
        client.write_all(&coalesced).unwrap();
        assert_eq!(
            AuthOkPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            4
        );
        assert_eq!(
            crate::ResponseOkPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            1
        );

        let ping = codec.encode(COMMAND_SEQUENCE_ID, &[COM_PING]).unwrap();
        let mut many_pings = Vec::with_capacity(ping.len() * (MAX_PACKETS_PER_FEED + 1));
        for _ in 0..=MAX_PACKETS_PER_FEED {
            many_pings.extend_from_slice(&ping);
        }
        client.write_all(&many_pings).unwrap();
        for _ in 0..=MAX_PACKETS_PER_FEED {
            assert_eq!(
                crate::ResponseOkPacket::decode(codec, &read_frame(&mut client))
                    .unwrap()
                    .sequence_id,
                1
            );
        }

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_reports_truncated_eof_and_releases_registration() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let _handshake = read_frame(&mut client);
        let codec = packet_codec();
        let response = ClientHandshakeResponseConfig::default()
            .encode(codec, 1)
            .unwrap();
        client.write_all(&response[..PACKET_HEADER_LEN]).unwrap();
        thread::sleep(Duration::from_millis(20));
        drop(client);
        assert!(matches!(
            worker.join(),
            Err(RuntimeUnixConnectionWorkerError::Connection(
                RuntimeUnixConnectionError::TruncatedFrame
            ))
        ));
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_shutdown_unblocks_authentication_read() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(1),
            Duration::from_millis(250),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let _handshake = read_frame(&mut client);
        let report = listener.shutdown();
        assert!(report.drained());
        drop(client);
        assert!(matches!(
            worker.join(),
            Ok(())
                | Err(RuntimeUnixConnectionWorkerError::Connection(
                    RuntimeUnixConnectionError::Read(_) | RuntimeUnixConnectionError::Write(_)
                ))
        ));
        assert!(!endpoint.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn authentication_deadline_is_cumulative_across_slow_drip_full_authentication() {
        let authentication = Duration::from_secs(1);
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            authentication,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let codec = packet_codec();
        let _handshake = read_frame(&mut client);
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "alice",
            vec![0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN.to_owned()),
            None,
        )
        .encode(codec, 1)
        .unwrap();
        client.write_all(&response).unwrap();
        let auth_more = read_frame(&mut client);
        assert_eq!(
            AuthMoreData::decode(codec, &auth_more).unwrap().sequence_id,
            2
        );

        thread::sleep(Duration::from_millis(600));
        let full = codec.encode(3, b"secret\0").unwrap();
        client.write_all(&full[..5]).unwrap();
        thread::sleep(Duration::from_millis(600));
        drop(client);
        assert!(matches!(
            worker.join(),
            Err(RuntimeUnixConnectionWorkerError::Connection(
                RuntimeUnixConnectionError::AuthenticationDeadlineExceeded
            ))
        ));
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn shutdown_rejects_a_buffered_privileged_command_before_catalog_mutation() {
        let (listener, data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let mut client = UnixStream::connect(endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let accepted = listener.accept().unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let worker = thread::spawn(move || {
            let mut frame_count = 0;
            let mut stream = accepted;
            run_unix_connection_with_before_frame(&mut stream, move || {
                frame_count += 1;
                if frame_count == 3 {
                    worker_entered.wait();
                    worker_release.wait();
                }
            })
        });

        let codec = packet_codec();
        let _greeting = read_frame(&mut client);
        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "alice",
            vec![0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN.to_owned()),
            None,
        )
        .encode(codec, 1)
        .unwrap();
        client.write_all(&response).unwrap();
        assert_eq!(
            AuthMoreData::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            2
        );

        let mut buffered = codec.encode(3, b"secret\0").unwrap();
        let mut create = vec![COM_QUERY];
        create.extend_from_slice(b"CREATE DATABASE blocked");
        buffered.extend_from_slice(&codec.encode(COMMAND_SEQUENCE_ID, &create).unwrap());
        client.write_all(&buffered).unwrap();
        entered.wait();
        assert_eq!(
            AuthOkPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            4
        );

        thread::scope(|scope| {
            let shutdown = scope.spawn(|| listener.shutdown());
            while !listener.is_shutting_down() {
                thread::yield_now();
            }
            release.wait();
            assert!(shutdown.join().unwrap().drained());
        });
        drop(client);
        assert!(matches!(
            worker.join().unwrap(),
            Err(RuntimeUnixConnectionError::Listener(
                RuntimeUnixListenerError::ShuttingDown
            ))
        ));
        drop(listener);
        let catalog = MySqlDatabaseCatalog::open(data_root.path()).unwrap();
        assert!(!catalog.list().unwrap().iter().any(|name| name == "blocked"));
    }
}
