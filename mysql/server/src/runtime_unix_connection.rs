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
    AcceptedUnixStream, AuthorizedDatabaseAdapterFactory, CLIENT_SSL, CachingSha2Verifier,
    ClassicConnectionOrchestrator, ClassicFrame, InitialHandshakeSettings,
    MAX_COMMAND_PAYLOAD_LENGTH, OrchestratorError, OrchestratorEvent, PacketCodec,
    PacketCodecError, PacketStreamDecoder, RuntimeUnixListenerError,
    SUPPORTED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES, StreamDecoderError, TransportSecurity,
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
    .with_prepared_statement_authority(stream.prepared_statement_authority())
    .with_query_timeout(timeouts.query())
    .with_bootstrap_settings(MAX_COMMAND_PAYLOAD_LENGTH, timeouts.idle());
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
                        return Err(RuntimeUnixConnectionError::UnexpectedTlsUpgrade);
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
            Arc, Barrier, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{
        AccountStoreCheckpoint, AccountStoreCheckpointAuthority, AccountStoreCheckpointRequest,
        AuthMoreData, AuthMoreDataKind, AuthOkPacket, BinaryRowColumnType, BinaryRowPacket,
        BinaryRowValue, CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_CONNECT_WITH_DB, CLIENT_DEPRECATE_EOF,
        COM_PING, COM_QUERY, COM_QUIT, COM_RESET_CONNECTION, COM_STMT_CLOSE, COM_STMT_EXECUTE,
        COM_STMT_PREPARE,
        COM_STMT_RESET, COM_STMT_SEND_LONG_DATA, COMMAND_SEQUENCE_ID, CURSOR_TYPE_NO_CURSOR,
        CheckpointAuthorityId, CheckpointPersistence, CheckpointReadError,
        ClientHandshakeResponseConfig, ColumnCountPacket, ColumnDefinitionPacket,
        DatabasePrivileges, GlobalPrivileges, InitialHandshake, OfflineAccountProvisioner,
        ProtectedPassword, ResultTerminatorPacket, RuntimeConfig, RuntimeLimits, RuntimeTimeouts,
        RuntimeUnixListener, StmtPrepareOkPacket, TextRowPacket, TextRowValue, UnixSocketConfig,
        DEFAULT_UTF8MB4_COLLATION, MIN_WRITE_LIMIT, MYSQL_TYPE_BLOB,
        MYSQL_TYPE_LONGLONG, MYSQL_TYPE_NULL, MYSQL_TYPE_VAR_STRING, PACKET_HEADER_LEN,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES, SERVER_STATUS_AUTOCOMMIT,
        SERVER_STATUS_IN_TRANS,
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
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
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
        let mut session = catalog.new_session(binary_schema_context());
        session.select_database("testdb").unwrap();
        session
            .connection()
            .unwrap()
            .execute(
                "CREATE TABLE records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
            )
            .unwrap();
        session
            .connection()
            .unwrap()
            .execute("CREATE TABLE update_records (id INT, label TEXT)")
            .unwrap();
        session
            .connection()
            .unwrap()
            .execute("INSERT INTO update_records (id, label) VALUES (1, 'visible')")
            .unwrap();
        drop(session);
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
    fn real_unix_socket_runs_prepared_execute_reset_cached_execute_and_close() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let capabilities = CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF;
        client_handshake(&mut client, Some("testdb"), capabilities);
        let codec = packet_codec();

        let mut prepare = vec![COM_STMT_PREPARE];
        prepare.extend_from_slice(b"SELECT ? AS value");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &prepare).unwrap())
            .unwrap();
        let prepare_ok = StmtPrepareOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(prepare_ok.sequence_id, 1);
        assert_eq!(prepare_ok.num_params, 1);
        assert_eq!(prepare_ok.num_columns, 1);
        let parameter = ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(parameter.sequence_id, 2);
        assert_eq!(parameter.name, "?1");
        assert_eq!(parameter.column_type, MYSQL_TYPE_NULL);
        let column = ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(column.sequence_id, 3);
        assert_eq!(column.name, "value");
        // A marker column starts generic, as MySQL 8.4.11 does.
        assert_eq!(column.column_type, MYSQL_TYPE_VAR_STRING);

        let execute = |client: &mut UnixStream, value: i64, new_types: bool| {
            let mut payload = vec![COM_STMT_EXECUTE];
            payload.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
            payload.push(CURSOR_TYPE_NO_CURSOR);
            payload.extend_from_slice(&1u32.to_le_bytes());
            payload.push(0);
            payload.push(u8::from(new_types));
            if new_types {
                payload.extend_from_slice(&[MYSQL_TYPE_LONGLONG, 0]);
            }
            payload.extend_from_slice(&value.to_le_bytes());
            client
                .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap())
                .unwrap();

            let count = ColumnCountPacket::decode(codec, &read_frame(client)).unwrap();
            assert_eq!(count.sequence_id, 1);
            assert_eq!(count.column_count, 1);
            let column = ColumnDefinitionPacket::decode(codec, &read_frame(client)).unwrap();
            assert_eq!(column.sequence_id, 2);
            assert_eq!(column.name, "value");
            assert_eq!(column.column_type, MYSQL_TYPE_LONGLONG);
            let row_frame = read_frame(client);
            let row =
                BinaryRowPacket::decode(codec, &row_frame, &[BinaryRowColumnType::Int64]).unwrap();
            assert_eq!(row.sequence_id, 3);
            assert_eq!(row.values, [BinaryRowValue::Int64(value)]);
            let terminator = ResultTerminatorPacket::decode(
                codec,
                &read_frame(client),
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | capabilities,
            )
            .unwrap();
            assert!(matches!(
                terminator,
                ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 4
            ));
        };

        execute(&mut client, 42, true);

        let mut reset = vec![COM_STMT_RESET];
        reset.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &reset).unwrap())
            .unwrap();
        let reset = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(reset.sequence_id, 1);

        execute(&mut client, 7, false);

        let mut close = vec![COM_STMT_CLOSE];
        close.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &close).unwrap())
            .unwrap();
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_reset_connection_rolls_back_and_clears_session_state() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let capabilities = CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF;
        client_handshake(&mut client, Some("testdb"), capabilities);
        let codec = packet_codec();
        let response_capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | capabilities;

        let mut set_autocommit = vec![COM_QUERY];
        set_autocommit.extend_from_slice(b"SET SESSION autocommit = 0");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &set_autocommit).unwrap())
            .unwrap();
        let set_autocommit =
            crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(set_autocommit.status_flags, 0);

        let mut insert = vec![COM_QUERY];
        insert.extend_from_slice(b"INSERT INTO records (label) VALUES ('rolled back')");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &insert).unwrap())
            .unwrap();
        let insert = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(insert.last_insert_id, 1);
        assert_eq!(insert.status_flags, SERVER_STATUS_IN_TRANS);

        let mut prepare = vec![COM_STMT_PREPARE];
        prepare.extend_from_slice(b"SELECT ? AS discarded");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &prepare).unwrap())
            .unwrap();
        let prepare_ok = StmtPrepareOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(prepare_ok.num_params, 1);
        assert_eq!(prepare_ok.num_columns, 1);
        let parameter = ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(parameter.name, "?1");
        let column = ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(column.name, "discarded");

        let mut long_data = vec![COM_STMT_SEND_LONG_DATA];
        long_data.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        long_data.extend_from_slice(&0u16.to_le_bytes());
        long_data.extend_from_slice(b"discarded");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &long_data).unwrap())
            .unwrap();

        client
            .write_all(
                &codec
                    .encode(COMMAND_SEQUENCE_ID, &[COM_RESET_CONNECTION])
                    .unwrap(),
            )
            .unwrap();
        let reset = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(reset.sequence_id, 1);
        assert_eq!(reset.last_insert_id, 0);
        assert_eq!(reset.status_flags, SERVER_STATUS_AUTOCOMMIT);

        let mut execute = vec![COM_STMT_EXECUTE];
        execute.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        execute.push(CURSOR_TYPE_NO_CURSOR);
        execute.extend_from_slice(&1u32.to_le_bytes());
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &execute).unwrap())
            .unwrap();
        let error =
            crate::ErrPacket::decode(codec, &read_frame(&mut client), response_capabilities)
                .unwrap();
        assert_eq!(error.error_code, 1243);

        let query_single = |client: &mut UnixStream, sql: &[u8]| -> Vec<u8> {
            let mut query = vec![COM_QUERY];
            query.extend_from_slice(sql);
            client
                .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &query).unwrap())
                .unwrap();
            let first_frame = read_frame(client);
            if first_frame[PACKET_HEADER_LEN] == 0xff {
                let error =
                    crate::ErrPacket::decode(codec, &first_frame, response_capabilities).unwrap();
                panic!(
                    "scalar query failed with {}: {:?}",
                    error.error_code, error.message
                );
            }
            let count = ColumnCountPacket::decode(codec, &first_frame).unwrap();
            assert_eq!(count.sequence_id, 1);
            assert_eq!(count.column_count, 1);
            let _column = ColumnDefinitionPacket::decode(codec, &read_frame(client)).unwrap();
            let row_frame = read_frame(client);
            let row = TextRowPacket::decode(codec, &row_frame, 1).unwrap();
            let value = match row.values.into_iter().next().unwrap() {
                TextRowValue::Bytes(value) => value.to_vec(),
                TextRowValue::Null => panic!("scalar query unexpectedly returned NULL"),
            };
            assert!(matches!(
                ResultTerminatorPacket::decode(
                    codec,
                    &read_frame(client),
                    response_capabilities,
                )
                .unwrap(),
                ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 4
            ));
            value
        };

        let query_ids = |client: &mut UnixStream| -> Vec<Vec<u8>> {
            let mut query = vec![COM_QUERY];
            query.extend_from_slice(b"SELECT id FROM records");
            client
                .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &query).unwrap())
                .unwrap();
            let count = ColumnCountPacket::decode(codec, &read_frame(client)).unwrap();
            assert_eq!(count.sequence_id, 1);
            assert_eq!(count.column_count, 1);
            let _column = ColumnDefinitionPacket::decode(codec, &read_frame(client)).unwrap();
            let mut values = Vec::new();
            loop {
                let frame = read_frame(client);
                if frame[PACKET_HEADER_LEN] == 0xfe {
                    assert!(matches!(
                        ResultTerminatorPacket::decode(codec, &frame, response_capabilities)
                            .unwrap(),
                        ResultTerminatorPacket::Ok(_)
                    ));
                    break;
                }
                let row = TextRowPacket::decode(codec, &frame, 1).unwrap();
                match row.values.into_iter().next().unwrap() {
                    TextRowValue::Bytes(value) => values.push(value.to_vec()),
                    TextRowValue::Null => panic!("record id unexpectedly returned NULL"),
                }
            }
            values
        };

        assert!(query_ids(&mut client).is_empty());
        assert_eq!(query_single(&mut client, b"SELECT LAST_INSERT_ID()"), b"0");

        let mut committed_insert = vec![COM_QUERY];
        committed_insert.extend_from_slice(b"INSERT INTO records (label) VALUES ('committed')");
        client
            .write_all(
                &codec
                    .encode(COMMAND_SEQUENCE_ID, &committed_insert)
                    .unwrap(),
            )
            .unwrap();
        let committed = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(committed.last_insert_id, 2);
        assert_eq!(committed.status_flags, SERVER_STATUS_AUTOCOMMIT);

        client
            .write_all(
                &codec
                    .encode(COMMAND_SEQUENCE_ID, &[COM_RESET_CONNECTION])
                    .unwrap(),
            )
            .unwrap();
        let reset = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(reset.status_flags, SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(query_ids(&mut client), [b"2".to_vec()]);
        assert_eq!(query_single(&mut client, b"SELECT LAST_INSERT_ID()"), b"0");

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_runs_prepared_insert_and_confirms_persistence() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let capabilities = CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF;
        client_handshake(&mut client, Some("testdb"), capabilities);
        let codec = packet_codec();

        let mut prepare = vec![COM_STMT_PREPARE];
        prepare.extend_from_slice(b"INSERT INTO update_records (id, label) VALUES (?, ?)");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &prepare).unwrap())
            .unwrap();
        let prepare_ok = StmtPrepareOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(prepare_ok.sequence_id, 1);
        assert_eq!(prepare_ok.num_params, 2);
        assert_eq!(prepare_ok.num_columns, 0);
        let first_parameter =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(first_parameter.sequence_id, 2);
        assert_eq!(first_parameter.name, "?1");
        assert_eq!(first_parameter.column_type, MYSQL_TYPE_NULL);
        let second_parameter =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(second_parameter.sequence_id, 3);
        assert_eq!(second_parameter.name, "?2");
        assert_eq!(second_parameter.column_type, MYSQL_TYPE_NULL);

        let mut execute = vec![COM_STMT_EXECUTE];
        execute.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        execute.push(CURSOR_TYPE_NO_CURSOR);
        execute.extend_from_slice(&1u32.to_le_bytes());
        execute.push(0);
        execute.push(1);
        execute.extend_from_slice(&[MYSQL_TYPE_LONGLONG, 0, MYSQL_TYPE_VAR_STRING, 0]);
        execute.extend_from_slice(&2i64.to_le_bytes());
        execute.extend_from_slice(&[8, b'p', b'r', b'e', b'p', b'a', b'r', b'e', b'd']);
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &execute).unwrap())
            .unwrap();
        let inserted = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(inserted.sequence_id, 1);
        assert_eq!(inserted.affected_rows, 1);
        assert_eq!(inserted.last_insert_id, 0);

        let mut select = vec![COM_QUERY];
        select.extend_from_slice(b"SELECT id, label FROM update_records");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &select).unwrap())
            .unwrap();
        let count = ColumnCountPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(count.sequence_id, 1);
        assert_eq!(count.column_count, 2);
        let id_definition =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(id_definition.sequence_id, 2);
        assert_eq!(id_definition.name, "id");
        let label_definition =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(label_definition.sequence_id, 3);
        assert_eq!(label_definition.name, "label");
        let first_row_frame = read_frame(&mut client);
        let first_row = TextRowPacket::decode(codec, &first_row_frame, 2).unwrap();
        assert_eq!(first_row.sequence_id, 4);
        assert_eq!(
            first_row.values,
            vec![TextRowValue::Bytes(b"1"), TextRowValue::Bytes(b"visible")]
        );
        let second_row_frame = read_frame(&mut client);
        let second_row = TextRowPacket::decode(codec, &second_row_frame, 2).unwrap();
        assert_eq!(second_row.sequence_id, 5);
        assert_eq!(
            second_row.values,
            vec![TextRowValue::Bytes(b"2"), TextRowValue::Bytes(b"prepared")]
        );
        let terminator = ResultTerminatorPacket::decode(
            codec,
            &read_frame(&mut client),
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | capabilities,
        )
        .unwrap();
        assert!(matches!(
            terminator,
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 6
        ));

        let mut close = vec![COM_STMT_CLOSE];
        close.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &close).unwrap())
            .unwrap();
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_runs_prepared_long_data_insert_and_reset() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let capabilities = CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF;
        client_handshake(&mut client, Some("testdb"), capabilities);
        let codec = packet_codec();
        let response_capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | capabilities;

        let mut create = vec![COM_QUERY];
        create.extend_from_slice(
            b"CREATE TABLE long_data_records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, blob_value BLOB NOT NULL, text_value TEXT NOT NULL)",
        );
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &create).unwrap())
            .unwrap();
        let created = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(created.sequence_id, 1);
        assert_eq!(created.affected_rows, 0);
        assert_eq!(created.last_insert_id, 0);

        let mut prepare = vec![COM_STMT_PREPARE];
        prepare.extend_from_slice(
            b"INSERT INTO long_data_records (blob_value, text_value) VALUES (?, ?)",
        );
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &prepare).unwrap())
            .unwrap();
        let prepare_ok = StmtPrepareOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(prepare_ok.sequence_id, 1);
        assert_eq!(prepare_ok.num_params, 2);
        assert_eq!(prepare_ok.num_columns, 0);
        let first_parameter =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(first_parameter.sequence_id, 2);
        assert_eq!(first_parameter.name, "?1");
        assert_eq!(first_parameter.column_type, MYSQL_TYPE_NULL);
        let second_parameter =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(second_parameter.sequence_id, 3);
        assert_eq!(second_parameter.name, "?2");
        assert_eq!(second_parameter.column_type, MYSQL_TYPE_NULL);

        let long_blob = [0x00, 0xff, 0x01, 0x80];
        let long_text = b"long text value";
        for (parameter_id, data) in [(0u16, &long_blob[..]), (1u16, &long_text[..])] {
            let mut payload = vec![COM_STMT_SEND_LONG_DATA];
            payload.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
            payload.extend_from_slice(&parameter_id.to_le_bytes());
            payload.extend_from_slice(data);
            client
                .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap())
                .unwrap();
        }

        let mut execute = vec![COM_STMT_EXECUTE];
        execute.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        execute.push(CURSOR_TYPE_NO_CURSOR);
        execute.extend_from_slice(&1u32.to_le_bytes());
        execute.extend_from_slice(&[0, 1, MYSQL_TYPE_BLOB, 0, MYSQL_TYPE_VAR_STRING, 0]);
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &execute).unwrap())
            .unwrap();
        let inserted = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(inserted.sequence_id, 1);
        assert_eq!(inserted.affected_rows, 1);
        assert_eq!(inserted.last_insert_id, 1);

        let select_rows = |client: &mut UnixStream| {
            let mut select = vec![COM_QUERY];
            select.extend_from_slice(b"SELECT blob_value, text_value FROM long_data_records");
            client
                .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &select).unwrap())
                .unwrap();

            let count = ColumnCountPacket::decode(codec, &read_frame(client)).unwrap();
            assert_eq!(count.sequence_id, 1);
            assert_eq!(count.column_count, 2);
            let blob_definition =
                ColumnDefinitionPacket::decode(codec, &read_frame(client)).unwrap();
            assert_eq!(blob_definition.sequence_id, 2);
            assert_eq!(blob_definition.name, "blob_value");
            assert_eq!(blob_definition.column_type, MYSQL_TYPE_BLOB);
            let text_definition =
                ColumnDefinitionPacket::decode(codec, &read_frame(client)).unwrap();
            assert_eq!(text_definition.sequence_id, 3);
            assert_eq!(text_definition.name, "text_value");
            assert_eq!(text_definition.column_type, MYSQL_TYPE_VAR_STRING);

            let first_row_frame = read_frame(client);
            let first_row = TextRowPacket::decode(codec, &first_row_frame, 2).unwrap();
            assert_eq!(first_row.sequence_id, 4);
            assert_eq!(
                first_row.values,
                [
                    TextRowValue::Bytes(&long_blob),
                    TextRowValue::Bytes(long_text),
                ]
            );
            let terminator =
                ResultTerminatorPacket::decode(codec, &read_frame(client), response_capabilities)
                    .unwrap();
            assert!(matches!(
                terminator,
                ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 5
            ));
        };

        select_rows(&mut client);

        let mut reset = vec![COM_STMT_RESET];
        reset.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &reset).unwrap())
            .unwrap();
        let reset = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(reset.sequence_id, 1);

        let ordinary_blob = [0x02, 0xfe, 0x03];
        let ordinary_text = b"ordinary value";
        let mut ordinary_execute = vec![COM_STMT_EXECUTE];
        ordinary_execute.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        ordinary_execute.push(CURSOR_TYPE_NO_CURSOR);
        ordinary_execute.extend_from_slice(&1u32.to_le_bytes());
        ordinary_execute.extend_from_slice(&[0, 1, MYSQL_TYPE_BLOB, 0, MYSQL_TYPE_VAR_STRING, 0]);
        ordinary_execute.push(ordinary_blob.len() as u8);
        ordinary_execute.extend_from_slice(&ordinary_blob);
        ordinary_execute.push(ordinary_text.len() as u8);
        ordinary_execute.extend_from_slice(ordinary_text);
        client
            .write_all(
                &codec
                    .encode(COMMAND_SEQUENCE_ID, &ordinary_execute)
                    .unwrap(),
            )
            .unwrap();
        let ordinary = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(ordinary.sequence_id, 1);
        assert_eq!(ordinary.affected_rows, 1);
        assert_eq!(ordinary.last_insert_id, 2);

        let mut select = vec![COM_QUERY];
        select.extend_from_slice(b"SELECT blob_value, text_value FROM long_data_records");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &select).unwrap())
            .unwrap();
        let count = ColumnCountPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(count.sequence_id, 1);
        assert_eq!(count.column_count, 2);
        assert_eq!(
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            2
        );
        assert_eq!(
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            3
        );
        let first_row_frame = read_frame(&mut client);
        let first_row = TextRowPacket::decode(codec, &first_row_frame, 2).unwrap();
        assert_eq!(first_row.sequence_id, 4);
        assert_eq!(
            first_row.values,
            [
                TextRowValue::Bytes(&long_blob),
                TextRowValue::Bytes(long_text),
            ]
        );
        let second_row_frame = read_frame(&mut client);
        let second_row = TextRowPacket::decode(codec, &second_row_frame, 2).unwrap();
        assert_eq!(second_row.sequence_id, 5);
        assert_eq!(
            second_row.values,
            [
                TextRowValue::Bytes(&ordinary_blob),
                TextRowValue::Bytes(ordinary_text),
            ]
        );
        assert!(matches!(
            ResultTerminatorPacket::decode(codec, &read_frame(&mut client), response_capabilities)
                .unwrap(),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 6
        ));

        let mut close = vec![COM_STMT_CLOSE];
        close.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &close).unwrap())
            .unwrap();
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_runs_prepared_auto_increment_insert_and_preserves_last_id() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        let capabilities = CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF;
        client_handshake(&mut client, Some("testdb"), capabilities);
        let codec = packet_codec();
        let response_capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | capabilities;

        let mut create = vec![COM_QUERY];
        create.extend_from_slice(
            b"CREATE TABLE prepared_auto_increment (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT UNIQUE)",
        );
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &create).unwrap())
            .unwrap();
        let created = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(created.sequence_id, 1);
        assert_eq!(created.affected_rows, 0);
        assert_eq!(created.last_insert_id, 0);

        let mut prepare = vec![COM_STMT_PREPARE];
        prepare.extend_from_slice(b"INSERT INTO prepared_auto_increment (name) VALUES (?)");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &prepare).unwrap())
            .unwrap();
        let prepare_ok = StmtPrepareOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(prepare_ok.sequence_id, 1);
        assert_eq!(prepare_ok.num_params, 1);
        assert_eq!(prepare_ok.num_columns, 0);
        let parameter = ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(parameter.sequence_id, 2);
        assert_eq!(parameter.name, "?1");
        assert_eq!(parameter.column_type, MYSQL_TYPE_NULL);

        let execute = |client: &mut UnixStream, name: &[u8]| {
            let mut payload = vec![COM_STMT_EXECUTE];
            payload.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
            payload.push(CURSOR_TYPE_NO_CURSOR);
            payload.extend_from_slice(&1u32.to_le_bytes());
            payload.push(0);
            payload.push(1);
            payload.extend_from_slice(&[MYSQL_TYPE_VAR_STRING, 0]);
            assert!(name.len() <= 250);
            payload.push(name.len() as u8);
            payload.extend_from_slice(name);
            client
                .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap())
                .unwrap();
        };

        execute(&mut client, b"Ada");
        let first = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(first.sequence_id, 1);
        assert_eq!(first.affected_rows, 1);
        assert_eq!(first.last_insert_id, 1);

        execute(&mut client, b"Grace");
        let second = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(second.sequence_id, 1);
        assert_eq!(second.affected_rows, 1);
        assert_eq!(second.last_insert_id, 2);

        execute(&mut client, b"Grace");
        let duplicate =
            crate::ErrPacket::decode(codec, &read_frame(&mut client), response_capabilities)
                .unwrap();
        assert_eq!(duplicate.sequence_id, 1);
        assert_eq!(duplicate.error_code, 1062);

        let mut last_insert_id = vec![COM_QUERY];
        last_insert_id.extend_from_slice(b"SELECT LAST_INSERT_ID()");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &last_insert_id).unwrap())
            .unwrap();
        let count = ColumnCountPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(count.sequence_id, 1);
        assert_eq!(count.column_count, 1);
        let definition = ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(definition.sequence_id, 2);
        let row_frame = read_frame(&mut client);
        let row = TextRowPacket::decode(codec, &row_frame, 1).unwrap();
        assert_eq!(row.sequence_id, 3);
        assert_eq!(row.values, vec![TextRowValue::Bytes(b"2")]);
        assert!(matches!(
            ResultTerminatorPacket::decode(
                codec,
                &read_frame(&mut client),
                response_capabilities,
            )
            .unwrap(),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 4
        ));

        execute(&mut client, b"Linus");
        let fourth = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(fourth.sequence_id, 1);
        assert_eq!(fourth.affected_rows, 1);
        assert_eq!(fourth.last_insert_id, 4);

        let mut select = vec![COM_QUERY];
        select.extend_from_slice(b"SELECT id, name FROM prepared_auto_increment");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &select).unwrap())
            .unwrap();
        let count = ColumnCountPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(count.sequence_id, 1);
        assert_eq!(count.column_count, 2);
        let id_definition =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(id_definition.sequence_id, 2);
        assert_eq!(id_definition.name, "id");
        let name_definition =
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(name_definition.sequence_id, 3);
        assert_eq!(name_definition.name, "name");

        let first_row_frame = read_frame(&mut client);
        let first_row = TextRowPacket::decode(codec, &first_row_frame, 2).unwrap();
        assert_eq!(first_row.sequence_id, 4);
        assert_eq!(
            first_row.values,
            vec![TextRowValue::Bytes(b"1"), TextRowValue::Bytes(b"Ada")]
        );
        let second_row_frame = read_frame(&mut client);
        let second_row = TextRowPacket::decode(codec, &second_row_frame, 2).unwrap();
        assert_eq!(second_row.sequence_id, 5);
        assert_eq!(
            second_row.values,
            vec![TextRowValue::Bytes(b"2"), TextRowValue::Bytes(b"Grace")]
        );
        let fourth_row_frame = read_frame(&mut client);
        let fourth_row = TextRowPacket::decode(codec, &fourth_row_frame, 2).unwrap();
        assert_eq!(fourth_row.sequence_id, 6);
        assert_eq!(
            fourth_row.values,
            vec![TextRowValue::Bytes(b"4"), TextRowValue::Bytes(b"Linus")]
        );
        assert!(matches!(
            ResultTerminatorPacket::decode(codec, &read_frame(&mut client), response_capabilities)
                .unwrap(),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 7
        ));

        let mut close = vec![COM_STMT_CLOSE];
        close.extend_from_slice(&prepare_ok.statement_id.to_le_bytes());
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &close).unwrap())
            .unwrap();
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_checked_insert_and_delete_encode_results_and_effects() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        client_handshake(
            &mut client,
            Some("testdb"),
            CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF,
        );
        let codec = packet_codec();

        let mut insert = vec![COM_QUERY];
        insert.extend_from_slice(b"INSERT INTO records (label) VALUES ('visible')");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &insert).unwrap())
            .unwrap();
        let inserted = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(inserted.sequence_id, 1);
        assert_eq!(inserted.affected_rows, 1);
        assert_eq!(inserted.last_insert_id, 1);

        let mut update = vec![COM_QUERY];
        update.extend_from_slice(b"UPDATE update_records SET label = 'visible' WHERE TRUE");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &update).unwrap())
            .unwrap();
        let unchanged = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(unchanged.sequence_id, 1);
        assert_eq!(unchanged.affected_rows, 0);
        assert_eq!(unchanged.last_insert_id, 0);

        let mut update = vec![COM_QUERY];
        update.extend_from_slice(b"UPDATE update_records SET label = 'changed' WHERE TRUE");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &update).unwrap())
            .unwrap();
        let changed = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(changed.sequence_id, 1);
        assert_eq!(changed.affected_rows, 1);
        assert_eq!(changed.last_insert_id, 0);

        let mut select = vec![COM_QUERY];
        select.extend_from_slice(b"SELECT id, label FROM records");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &select).unwrap())
            .unwrap();
        let count = crate::ColumnCountPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(count.sequence_id, 1);
        assert_eq!(count.column_count, 2);
        assert_eq!(
            crate::ColumnDefinitionPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            2
        );
        assert_eq!(
            crate::ColumnDefinitionPacket::decode(codec, &read_frame(&mut client))
                .unwrap()
                .sequence_id,
            3
        );
        let row_frame = read_frame(&mut client);
        let row = TextRowPacket::decode(codec, &row_frame, 2).unwrap();
        assert_eq!(row.sequence_id, 4);
        assert_eq!(
            row.values,
            vec![TextRowValue::Bytes(b"1"), TextRowValue::Bytes(b"visible")]
        );
        assert!(matches!(
            ResultTerminatorPacket::decode(
                codec,
                &read_frame(&mut client),
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                    | CLIENT_CONNECT_WITH_DB
                    | CLIENT_DEPRECATE_EOF,
            )
            .unwrap(),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 5
        ));

        let mut delete = vec![COM_QUERY];
        delete.extend_from_slice(b"DELETE FROM records WHERE id IS NOT NULL");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &delete).unwrap())
            .unwrap();
        let deleted = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(deleted.sequence_id, 1);
        assert_eq!(deleted.affected_rows, 1);
        assert_eq!(deleted.last_insert_id, 0);

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &select).unwrap())
            .unwrap();
        let count = crate::ColumnCountPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(count.column_count, 2);
        let _id_definition = read_frame(&mut client);
        let _label_definition = read_frame(&mut client);
        assert!(matches!(
            ResultTerminatorPacket::decode(
                codec,
                &read_frame(&mut client),
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                    | CLIENT_CONNECT_WITH_DB
                    | CLIENT_DEPRECATE_EOF,
            )
            .unwrap(),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 4
        ));

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        drop(client);
        assert!(worker.join().is_ok());
        assert!(listener.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn real_unix_socket_checked_update_reports_matched_rows_with_found_rows() {
        let (listener, _data_root, _account_root, _socket_directory, endpoint) = protocol_runtime(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let (mut client, worker) = start_worker(&listener, &endpoint);
        client_handshake(
            &mut client,
            Some("testdb"),
            CLIENT_CONNECT_WITH_DB | CLIENT_DEPRECATE_EOF | crate::CLIENT_FOUND_ROWS,
        );
        let codec = packet_codec();

        let mut update = vec![COM_QUERY];
        update.extend_from_slice(b"UPDATE update_records SET label = 'visible' WHERE TRUE");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &update).unwrap())
            .unwrap();
        let matched = crate::ResponseOkPacket::decode(codec, &read_frame(&mut client)).unwrap();
        assert_eq!(matched.sequence_id, 1);
        assert_eq!(matched.affected_rows, 1);
        assert_eq!(matched.last_insert_id, 0);

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
