//! Runtime ownership of one externally checkpointed account store.
//!
//! The checkpoint is read before opening the store and before every reload.
//! This keeps an account file under the local root from authorizing itself.

use std::{
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

use crate::runtime_config::{AccountStoreCheckpointWait, AccountStoreCheckpointWake};
use crate::{
    AccountStoreCheckpointReader, AuthenticatedPrincipal, AuthorizationError,
    CheckpointAuthorityId, CheckpointReadError, CredentialProvider, CredentialProviderError,
    CredentialSnapshot, DatabaseAction, DatabaseAuthorizer, PersistentAccountStore,
    PersistentAccountStoreError, ReloadOutcome, RuntimeConfig,
};

/// Owns the account generation used by one Unix server runtime.
///
/// Construct this before binding any listener. [`Self::reload_once`] is an
/// explicit runtime event; this type does not schedule background work.
pub struct RuntimeAccountStore {
    authority: CheckpointAuthorityId,
    checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
    checkpoint_timeout: Duration,
    store: Arc<PersistentAccountStore>,
    reload_gate: Mutex<()>,
    outstanding_checkpoint: Mutex<Option<crate::AccountStoreCheckpointRequest>>,
    checkpoint_wake: Mutex<Option<AccountStoreCheckpointWake>>,
    shutdown_requested: AtomicBool,
    ready_for_new_connections: AtomicBool,
    readiness: Mutex<RuntimeAccountReadiness>,
    readiness_wake: Condvar,
    #[cfg(test)]
    readiness_wait_count: std::sync::atomic::AtomicUsize,
}

struct RuntimeAccountReadiness {
    ready: bool,
    shutdown_requested: bool,
    pending_reloads: usize,
}

impl RuntimeAccountStore {
    /// Reads the external checkpoint and opens only the exact local generation.
    pub fn open(
        config: &RuntimeConfig,
        checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
    ) -> Result<Self, RuntimeAccountStoreError> {
        let authority = config.checkpoint_authority().clone();
        let request = checkpoint_reader
            .request_checkpoint(&authority)
            .map_err(RuntimeAccountStoreError::CheckpointRead)?;
        let checkpoint = match request.wait(config.timeouts().checkpoint()) {
            AccountStoreCheckpointWait::Completed(result) => {
                result.map_err(RuntimeAccountStoreError::CheckpointRead)?
            }
            AccountStoreCheckpointWait::TimedOut(_) => {
                return Err(RuntimeAccountStoreError::CheckpointRead(
                    CheckpointReadError::TimedOut,
                ));
            }
            AccountStoreCheckpointWait::Stopped(_) => {
                unreachable!("startup checkpoint reads cannot be stopped")
            }
        };
        let store = PersistentAccountStore::open(config.account_root(), &checkpoint)
            .map_err(RuntimeAccountStoreError::Store)?;
        Ok(Self {
            authority,
            checkpoint_reader,
            checkpoint_timeout: config.timeouts().checkpoint(),
            store: Arc::new(store),
            reload_gate: Mutex::new(()),
            outstanding_checkpoint: Mutex::new(None),
            checkpoint_wake: Mutex::new(None),
            shutdown_requested: AtomicBool::new(false),
            ready_for_new_connections: AtomicBool::new(true),
            readiness: Mutex::new(RuntimeAccountReadiness {
                ready: true,
                shutdown_requested: false,
                pending_reloads: 0,
            }),
            readiness_wake: Condvar::new(),
            #[cfg(test)]
            readiness_wait_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Reads an external checkpoint, then installs only the exact local bytes.
    ///
    /// Reading the authority happens before entering the account store. A
    /// failed read or rejected candidate leaves the current generation intact.
    pub fn reload_once(&self) -> RuntimeAccountReload {
        if !self.begin_reload() {
            return self.stopped();
        }
        let reload_gate = self
            .reload_gate
            .lock()
            .expect("runtime account reload gate must not be poisoned");
        let result = self.reload_once_started();
        self.finish_reload(result, reload_gate)
    }

    fn reload_once_started(&self) -> RuntimeAccountReload {
        let mut outstanding = match self.outstanding_checkpoint.lock() {
            Ok(outstanding) => outstanding,
            Err(_) => {
                return RuntimeAccountReload::Degraded(
                    RuntimeAccountStoreError::SupervisorUnavailable,
                );
            }
        };
        if self.shutdown_requested() {
            return RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable);
        }
        if let Some(request) = outstanding.as_mut() {
            if !request.cancellation_finished() {
                return RuntimeAccountReload::Degraded(
                    RuntimeAccountStoreError::SupervisorUnavailable,
                );
            }
        }
        *outstanding = None;
        let request = match self.checkpoint_reader.request_checkpoint(&self.authority) {
            Ok(request) => request,
            Err(error) => {
                return RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                    error,
                ));
            }
        };
        {
            let mut wake = self
                .checkpoint_wake
                .lock()
                .expect("runtime account checkpoint wake state must not be poisoned");
            assert!(
                wake.is_none(),
                "only one checkpoint request may wait at once"
            );
            *wake = Some(request.wake_handle());
        }
        let wait = request.wait_until_shutdown(self.checkpoint_timeout, &self.shutdown_requested);
        *self
            .checkpoint_wake
            .lock()
            .expect("runtime account checkpoint wake state must not be poisoned") = None;
        let checkpoint = match wait {
            AccountStoreCheckpointWait::Completed(Ok(checkpoint)) => checkpoint,
            AccountStoreCheckpointWait::Completed(Err(error)) => {
                return RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                    error,
                ));
            }
            AccountStoreCheckpointWait::TimedOut(request) => {
                *outstanding = Some(request);
                return RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                    CheckpointReadError::TimedOut,
                ));
            }
            AccountStoreCheckpointWait::Stopped(request) => {
                *outstanding = Some(request);
                return RuntimeAccountReload::Degraded(
                    RuntimeAccountStoreError::SupervisorUnavailable,
                );
            }
        };
        if self.shutdown_requested() {
            return RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable);
        }
        match self.store.reload(&checkpoint) {
            Ok(outcome) => RuntimeAccountReload::Healthy(outcome),
            Err(error) => RuntimeAccountReload::Degraded(RuntimeAccountStoreError::Store(error)),
        }
    }

    /// Returns whether new authentication attempts may consult this store.
    pub fn is_ready_for_new_connections(&self) -> bool {
        !self.shutdown_requested() && self.ready_for_new_connections.load(Ordering::Acquire)
    }

    /// Makes this account store permanently reject new authentication attempts.
    pub(crate) fn begin_shutdown(&self) {
        {
            let mut readiness = self
                .readiness
                .lock()
                .expect("runtime account readiness state must not be poisoned");
            readiness.shutdown_requested = true;
            readiness.ready = false;
            self.shutdown_requested.store(true, Ordering::Release);
            self.ready_for_new_connections
                .store(false, Ordering::Release);
            self.readiness_wake.notify_all();
        }
        if let Some(wake) = self
            .checkpoint_wake
            .lock()
            .expect("runtime account checkpoint wake state must not be poisoned")
            .as_ref()
        {
            wake.notify();
        }
    }

    pub(crate) fn while_ready_for_new_connection<T>(
        &self,
        operation: impl FnOnce() -> T,
    ) -> Option<T> {
        let _gate = self.outstanding_checkpoint.try_lock().ok()?;
        self.is_ready_for_new_connections().then(operation)
    }

    /// Waits until an exact checkpoint reload is ready or shutdown starts.
    ///
    /// Returns `true` when new connections may proceed and `false` after
    /// shutdown. The atomic check keeps the ready path lock-free; the mutex
    /// makes a transition that happens before waiting observable.
    pub(crate) fn wait_until_ready_or_shutdown(&self) -> bool {
        if self.is_ready_for_new_connections() {
            return true;
        }
        let mut readiness = self
            .readiness
            .lock()
            .expect("runtime account readiness state must not be poisoned");
        while !readiness.ready && !readiness.shutdown_requested {
            #[cfg(test)]
            self.readiness_wait_count.fetch_add(1, Ordering::Release);
            readiness = self
                .readiness_wake
                .wait(readiness)
                .expect("runtime account readiness state must not be poisoned");
        }
        readiness.ready
    }

    /// Returns the revision currently serving authentication and authorization.
    pub fn revision(&self) -> Result<u64, RuntimeAccountStoreError> {
        self.store
            .revision()
            .map_err(RuntimeAccountStoreError::Store)
    }

    fn begin_reload(&self) -> bool {
        let mut readiness = self
            .readiness
            .lock()
            .expect("runtime account readiness state must not be poisoned");
        if readiness.shutdown_requested {
            return false;
        }
        readiness.pending_reloads = readiness
            .pending_reloads
            .checked_add(1)
            .expect("pending runtime account reload count must not overflow");
        readiness.ready = false;
        self.ready_for_new_connections
            .store(false, Ordering::Release);
        self.readiness_wake.notify_all();
        true
    }

    fn finish_reload(
        &self,
        result: RuntimeAccountReload,
        reload_gate: std::sync::MutexGuard<'_, ()>,
    ) -> RuntimeAccountReload {
        let mut readiness = self
            .readiness
            .lock()
            .expect("runtime account readiness state must not be poisoned");
        readiness.pending_reloads = readiness
            .pending_reloads
            .checked_sub(1)
            .expect("a finished runtime account reload must have started");
        drop(reload_gate);
        if readiness.shutdown_requested {
            readiness.ready = false;
            self.ready_for_new_connections
                .store(false, Ordering::Release);
            self.readiness_wake.notify_all();
            return RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable);
        }
        readiness.ready =
            matches!(result, RuntimeAccountReload::Healthy(_)) && readiness.pending_reloads == 0;
        self.ready_for_new_connections
            .store(readiness.ready, Ordering::Release);
        self.readiness_wake.notify_all();
        result
    }

    fn stopped(&self) -> RuntimeAccountReload {
        let mut readiness = self
            .readiness
            .lock()
            .expect("runtime account readiness state must not be poisoned");
        assert!(
            readiness.shutdown_requested,
            "only shutdown may stop the runtime account store"
        );
        readiness.ready = false;
        self.ready_for_new_connections
            .store(false, Ordering::Release);
        self.readiness_wake.notify_all();
        RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable)
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }
}

impl fmt::Debug for RuntimeAccountStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeAccountStore")
            .field("authority", &"<redacted>")
            .field("checkpoint_reader", &"<redacted>")
            .field("revision", &self.revision().ok())
            .field(
                "ready_for_new_connections",
                &self.is_ready_for_new_connections(),
            )
            .finish()
    }
}

impl CredentialProvider for RuntimeAccountStore {
    fn lookup(
        &self,
        username: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
        if !self.is_ready_for_new_connections() {
            return Err(CredentialProviderError::BackendUnavailable);
        }
        self.store.lookup(username)
    }
}

impl DatabaseAuthorizer for RuntimeAccountStore {
    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        if matches!(action, DatabaseAction::Connect { .. }) && !self.is_ready_for_new_connections()
        {
            return Err(AuthorizationError::Unavailable);
        }
        self.store.authorize(principal, action)
    }
}

impl CredentialProvider for Arc<RuntimeAccountStore> {
    fn lookup(
        &self,
        username: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
        self.as_ref().lookup(username)
    }
}

impl DatabaseAuthorizer for Arc<RuntimeAccountStore> {
    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        self.as_ref().authorize(principal, action)
    }
}

/// Result of one supervisor reload tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAccountReload {
    /// The exact authorized generation is active and new authentication may proceed.
    Healthy(ReloadOutcome),
    /// The last-good generation remains active, but new authentication is blocked.
    Degraded(RuntimeAccountStoreError),
}

/// A runtime account-store operation failed without exposing local paths or
/// checkpoint contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAccountStoreError {
    /// The single reload owner could not serialize a tick.
    SupervisorUnavailable,
    /// The external checkpoint could not be read safely.
    CheckpointRead(CheckpointReadError),
    /// The local account snapshot could not be opened or reloaded exactly.
    Store(PersistentAccountStoreError),
}

impl fmt::Display for RuntimeAccountStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SupervisorUnavailable => {
                f.write_str("runtime account reload supervisor unavailable")
            }
            Self::CheckpointRead(error) => {
                write!(f, "runtime account checkpoint read failed: {error}")
            }
            Self::Store(error) => write!(f, "runtime account store operation failed: {error}"),
        }
    }
}

impl Error for RuntimeAccountStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SupervisorUnavailable => None,
            Self::CheckpointRead(error) => Some(error),
            Self::Store(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        sync::{mpsc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{
        AccountDefinition, AccountGenerationBuilder, AccountId, AccountStoreCheckpoint,
        AccountStoreCheckpointAuthority, AccountStoreCheckpointRequest,
        AccountStoreCheckpointResponse, AuthorizedDatabaseAdapterFactory, CachingSha2Verifier,
        CheckpointPersistence, DatabaseGrant, DatabasePrivileges, GlobalPrivileges,
        OfflineAccountProvisioner, RuntimeLimits, RuntimeTimeouts, UnixSocketConfig,
        MIN_WRITE_LIMIT,
    };
    use turso_mysql::{
        schema_sql::{CharacterSet, Collation, SchemaSqlMode, SchemaSqlSessionContext},
        MySqlDatabaseCatalog,
    };

    struct FakeCheckpointReader {
        results: Mutex<VecDeque<Result<AccountStoreCheckpoint, CheckpointReadError>>>,
    }

    struct PendingAfterFirstReader {
        first: AccountStoreCheckpoint,
        first_sent: AtomicBool,
        pending: Mutex<Option<AccountStoreCheckpointResponse>>,
        recovery: Mutex<Option<AccountStoreCheckpoint>>,
    }

    impl FakeCheckpointReader {
        fn new(result: Result<AccountStoreCheckpoint, CheckpointReadError>) -> Self {
            Self {
                results: Mutex::new(VecDeque::from([result])),
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

    impl AccountStoreCheckpointReader for PendingAfterFirstReader {
        fn request_checkpoint(
            &self,
            _authority: &CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
            if !self.first_sent.swap(true, Ordering::AcqRel) {
                return Ok(AccountStoreCheckpointRequest::completed(Ok(self.first)));
            }
            if let Some(checkpoint) = self.recovery.lock().unwrap().take() {
                return Ok(AccountStoreCheckpointRequest::completed(Ok(checkpoint)));
            }
            let (response, request) = AccountStoreCheckpointRequest::channel();
            *self.pending.lock().unwrap() = Some(response);
            Ok(request)
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

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn authority_id() -> CheckpointAuthorityId {
        CheckpointAuthorityId::new("runtime-control-plane").unwrap()
    }

    fn config(account_root: &Path) -> RuntimeConfig {
        config_with_checkpoint_timeout(account_root, Duration::from_secs(5))
    }

    fn config_with_checkpoint_timeout(
        account_root: &Path,
        checkpoint_timeout: Duration,
    ) -> RuntimeConfig {
        RuntimeConfig::new(
            None,
            Some(UnixSocketConfig::new("/run/turso", "mysql.sock").unwrap()),
            "/var/lib/turso/data",
            account_root,
            authority_id(),
            Duration::from_secs(5),
            RuntimeLimits::new(16, 16, MIN_WRITE_LIMIT, 16).unwrap(),
            RuntimeTimeouts::new(
                checkpoint_timeout,
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

    fn builder(query_allowed: bool) -> AccountGenerationBuilder {
        let account_id = AccountId::from_bytes([7; 32]);
        let account = AccountDefinition::new("alice", account_id.clone(), true, [0x11; 32])
            .with_global_privileges(GlobalPrivileges::new(true, false));
        let builder = AccountGenerationBuilder::new().with_account(account);
        if query_allowed {
            builder.with_grant(DatabaseGrant::new(
                account_id,
                "reports",
                DatabasePrivileges::new(false, true, false, false),
            ))
        } else {
            builder
        }
    }

    fn provision(
        root: &Path,
        query_allowed: bool,
    ) -> (
        OfflineAccountProvisioner,
        MemoryAuthority,
        AccountStoreCheckpoint,
    ) {
        let mut authority = MemoryAuthority::default();
        let provisioner =
            OfflineAccountProvisioner::initialize(root, builder(query_allowed), &mut authority)
                .unwrap();
        let checkpoint = provisioner.checkpoint().unwrap();
        (provisioner, authority, checkpoint)
    }

    fn principal() -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([7; 32]))
    }

    fn binary_context() -> SchemaSqlSessionContext {
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

    fn pending_response(reader: &PendingAfterFirstReader) -> AccountStoreCheckpointResponse {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(response) = reader.pending.lock().unwrap().take() {
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "checkpoint reader did not receive a reload request"
            );
            thread::yield_now();
        }
    }

    fn wait_for_readiness_waits(store: &RuntimeAccountStore, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while store.readiness_wait_count.load(Ordering::Acquire) < expected {
            assert!(Instant::now() < deadline, "readiness waiter did not block");
            thread::yield_now();
        }
        let _readiness = store.readiness.lock().unwrap();
    }

    fn wait_for_pending_reloads(store: &RuntimeAccountStore, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if store.readiness.lock().unwrap().pending_reloads == expected {
                return;
            }
            assert!(Instant::now() < deadline, "reload did not start");
            thread::yield_now();
        }
    }

    fn wake_readiness_waiter(store: &RuntimeAccountStore) {
        let _readiness = store.readiness.lock().unwrap();
        store.readiness_wake.notify_all();
    }

    #[test]
    fn startup_rejects_missing_and_unavailable_checkpoints_before_opening_a_store() {
        let root = root();
        for error in [
            CheckpointReadError::Missing,
            CheckpointReadError::Unavailable,
        ] {
            let reader = Arc::new(FakeCheckpointReader::new(Err(error)));
            assert!(matches!(
                RuntimeAccountStore::open(&config(root.path()), reader),
                Err(RuntimeAccountStoreError::CheckpointRead(actual)) if actual == error
            ));
        }
    }

    #[test]
    fn startup_opens_only_the_exact_checkpointed_generation() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));

        let store = RuntimeAccountStore::open(&config(root.path()), reader).unwrap();

        assert_eq!(store.revision(), Ok(0));
        assert!(format!("{store:?}").contains("<redacted>"));
        assert!(!format!("{store:?}").contains(&root.path().display().to_string()));
    }

    #[test]
    fn startup_timeout_returns_without_leaving_a_live_runtime_request() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(PendingAfterFirstReader {
            first: checkpoint,
            first_sent: AtomicBool::new(true),
            pending: Mutex::new(None),
            recovery: Mutex::new(None),
        });
        let started = Instant::now();

        assert!(matches!(
            RuntimeAccountStore::open(
                &config_with_checkpoint_timeout(root.path(), Duration::from_millis(5)),
                reader.clone()
            ),
            Err(RuntimeAccountStoreError::CheckpointRead(
                CheckpointReadError::TimedOut
            ))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));

        let response = reader.pending.lock().unwrap().take().unwrap();
        assert!(response.is_cancelled());
        assert!(!response.complete(Ok(checkpoint)));
    }

    #[test]
    fn timed_out_reload_cancels_the_response_and_keeps_the_last_good_generation() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(PendingAfterFirstReader {
            first: checkpoint,
            first_sent: AtomicBool::new(false),
            pending: Mutex::new(None),
            recovery: Mutex::new(None),
        });
        let store = RuntimeAccountStore::open(
            &config_with_checkpoint_timeout(root.path(), Duration::from_millis(5)),
            reader.clone(),
        )
        .unwrap();

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                CheckpointReadError::TimedOut
            ))
        );
        assert!(!store.is_ready_for_new_connections());
        assert_eq!(store.revision(), Ok(0));

        let response = reader.pending.lock().unwrap().take().unwrap();
        assert!(response.is_cancelled());
        let registration_called = AtomicBool::new(false);
        assert_eq!(
            store.while_ready_for_new_connection(
                || registration_called.store(true, Ordering::SeqCst)
            ),
            None
        );
        assert!(!registration_called.load(Ordering::SeqCst));
        *reader.recovery.lock().unwrap() = Some(checkpoint);
        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable)
        );
        assert!(response.complete(Ok(checkpoint)));
        assert_eq!(store.revision(), Ok(0));

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Unchanged)
        );
        assert!(store.is_ready_for_new_connections());
        assert_eq!(store.while_ready_for_new_connection(|| 7), Some(7));
    }

    #[test]
    fn readiness_wait_wakes_after_an_exact_checkpoint_reload() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store =
            Arc::new(RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap());
        reader.push(Err(CheckpointReadError::Unavailable));
        reader.push(Ok(checkpoint));

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                CheckpointReadError::Unavailable
            ))
        );
        let (sender, receiver) = mpsc::channel();
        let waiting_store = Arc::clone(&store);
        let waiting = thread::spawn(move || {
            sender
                .send(waiting_store.wait_until_ready_or_shutdown())
                .unwrap();
        });
        wait_for_readiness_waits(&store, 1);

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Unchanged)
        );
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)), Ok(true));
        waiting.join().unwrap();
    }

    #[test]
    fn readiness_waits_for_every_queued_reload() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(PendingAfterFirstReader {
            first: checkpoint,
            first_sent: AtomicBool::new(false),
            pending: Mutex::new(None),
            recovery: Mutex::new(None),
        });
        let store =
            Arc::new(RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap());

        let first_reload = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.reload_once())
        };
        let first_response = pending_response(&reader);
        let second_reload = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.reload_once())
        };
        wait_for_pending_reloads(&store, 2);

        let (sender, receiver) = mpsc::channel();
        let waiting_store = Arc::clone(&store);
        let waiting = thread::spawn(move || {
            sender
                .send(waiting_store.wait_until_ready_or_shutdown())
                .unwrap();
        });
        wait_for_readiness_waits(&store, 1);
        assert!(first_response.complete(Ok(checkpoint)));
        let second_response = pending_response(&reader);
        assert_eq!(
            first_reload.join().unwrap(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Unchanged)
        );
        assert!(!store.is_ready_for_new_connections());
        assert!(receiver.try_recv().is_err());

        assert!(second_response.complete(Ok(checkpoint)));
        assert_eq!(
            second_reload.join().unwrap(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Unchanged)
        );
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)), Ok(true));
        waiting.join().unwrap();
    }

    #[test]
    fn readiness_wait_wakes_for_shutdown() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store =
            Arc::new(RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap());
        reader.push(Err(CheckpointReadError::Unavailable));
        assert!(matches!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(_)
        ));

        let (sender, receiver) = mpsc::channel();
        let waiting_store = Arc::clone(&store);
        let waiting = thread::spawn(move || {
            sender
                .send(waiting_store.wait_until_ready_or_shutdown())
                .unwrap();
        });
        wait_for_readiness_waits(&store, 1);

        store.begin_shutdown();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)), Ok(false));
        waiting.join().unwrap();
    }

    #[test]
    fn readiness_wait_returns_the_current_ready_or_shutdown_state() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store = RuntimeAccountStore::open(&config(root.path()), reader).unwrap();

        assert!(store.wait_until_ready_or_shutdown());
        store.begin_shutdown();
        assert!(!store.wait_until_ready_or_shutdown());
    }

    #[test]
    fn readiness_wait_ignores_a_spurious_wake() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store =
            Arc::new(RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap());
        reader.push(Err(CheckpointReadError::Unavailable));
        reader.push(Ok(checkpoint));
        assert!(matches!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(_)
        ));

        let (sender, receiver) = mpsc::channel();
        let waiting_store = Arc::clone(&store);
        let waiting = thread::spawn(move || {
            sender
                .send(waiting_store.wait_until_ready_or_shutdown())
                .unwrap();
        });
        wait_for_readiness_waits(&store, 1);
        wake_readiness_waiter(&store);
        wait_for_readiness_waits(&store, 2);
        assert!(receiver.try_recv().is_err());

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Unchanged)
        );
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)), Ok(true));
        waiting.join().unwrap();
    }

    #[test]
    fn shutdown_cancels_an_in_flight_reload_and_discards_its_late_reply() {
        let root = root();
        let (mut provisioner, mut authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(PendingAfterFirstReader {
            first: checkpoint,
            first_sent: AtomicBool::new(false),
            pending: Mutex::new(None),
            recovery: Mutex::new(None),
        });
        let store = Arc::new(
            RuntimeAccountStore::open(
                &config_with_checkpoint_timeout(root.path(), Duration::from_millis(200)),
                reader.clone(),
            )
            .unwrap(),
        );
        provisioner.replace(builder(false), &mut authority).unwrap();
        let replacement = provisioner.checkpoint().unwrap();

        let reloading = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.reload_once())
        };
        let response = pending_response(&reader);

        store.begin_shutdown();
        assert!(store.shutdown_requested());
        assert!(!store.is_ready_for_new_connections());
        assert_eq!(
            reloading.join().unwrap(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable)
        );
        assert!(response.is_cancelled());
        assert!(response.complete(Ok(replacement)));
        assert_eq!(store.revision(), Ok(0));
        assert!(!store.is_ready_for_new_connections());

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable)
        );
        assert!(reader.pending.lock().unwrap().is_none());
    }

    #[test]
    fn shutdown_returns_while_a_checkpoint_reload_is_still_pending() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(PendingAfterFirstReader {
            first: checkpoint,
            first_sent: AtomicBool::new(false),
            pending: Mutex::new(None),
            recovery: Mutex::new(None),
        });
        let store =
            Arc::new(RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap());
        let reloading = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.reload_once())
        };
        let response = pending_response(&reader);
        let shutting_down = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.begin_shutdown())
        };

        shutting_down.join().unwrap();
        assert!(store.shutdown_requested());
        assert!(!store.is_ready_for_new_connections());
        assert_eq!(
            reloading.join().unwrap(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::SupervisorUnavailable)
        );
        assert!(response.is_cancelled());
    }

    #[test]
    fn one_shared_runtime_store_wires_authentication_and_authorization() {
        let account_root = root();
        let data_root = root();
        let (_provisioner, _authority, checkpoint) = provision(account_root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store =
            Arc::new(RuntimeAccountStore::open(&config(account_root.path()), reader).unwrap());
        let catalog = MySqlDatabaseCatalog::open(data_root.path()).unwrap();

        let _verifier = CachingSha2Verifier::new(Arc::clone(&store));
        let _factory =
            AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), Arc::clone(&store));
    }

    #[test]
    fn startup_rejects_a_checkpoint_from_another_store() {
        let other_root = root();
        let root = root();
        let (_provisioner, _authority, _checkpoint) = provision(root.path(), true);
        let (_other_provisioner, _other_authority, other_checkpoint) =
            provision(other_root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(other_checkpoint)));

        assert!(matches!(
            RuntimeAccountStore::open(&config(root.path()), reader),
            Err(RuntimeAccountStoreError::Store(
                PersistentAccountStoreError::CheckpointMismatch
            ))
        ));
    }

    #[test]
    fn reload_rejects_a_snapshot_that_arrived_before_its_checkpoint() {
        let root = root();
        let (mut provisioner, mut authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store = RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap();

        provisioner.replace(builder(false), &mut authority).unwrap();
        reader.push(Ok(checkpoint));

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::Store(
                PersistentAccountStoreError::CheckpointMismatch
            ))
        );
        assert_eq!(store.revision(), Ok(0));
    }

    #[test]
    fn reload_installs_a_checkpointed_revocation_before_the_next_authorization() {
        let root = root();
        let (mut provisioner, mut authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store =
            Arc::new(RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap());
        let action = DatabaseAction::Query {
            database: "reports",
        };
        assert!(CredentialProvider::lookup(&Arc::clone(&store), "alice")
            .unwrap()
            .is_some());
        assert_eq!(store.authorize(&principal(), action), Ok(()));

        provisioner.replace(builder(false), &mut authority).unwrap();
        let replacement = provisioner.checkpoint().unwrap();
        reader.push(Ok(replacement));

        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Reloaded { revision: 1 })
        );
        assert_eq!(
            store.authorize(&principal(), action),
            Err(AuthorizationError::Denied)
        );
    }

    #[test]
    fn failed_reads_and_checkpoint_mismatches_keep_the_last_good_generation() {
        let root = root();
        let (mut provisioner, mut authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store = RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap();
        let action = DatabaseAction::Query {
            database: "reports",
        };

        provisioner.replace(builder(false), &mut authority).unwrap();
        let replacement = provisioner.checkpoint().unwrap();
        reader.push(Err(CheckpointReadError::Unavailable));
        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                CheckpointReadError::Unavailable
            ))
        );
        assert_eq!(store.revision(), Ok(0));
        assert_eq!(store.authorize(&principal(), action), Ok(()));
        assert_eq!(
            store.authorize(&principal(), DatabaseAction::Connect { database: None }),
            Err(AuthorizationError::Unavailable)
        );
        assert!(!store.is_ready_for_new_connections());
        assert!(matches!(
            store.lookup("alice"),
            Err(CredentialProviderError::BackendUnavailable)
        ));

        reader.push(Ok(checkpoint));
        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::Store(
                PersistentAccountStoreError::CheckpointMismatch
            ))
        );
        assert_eq!(store.revision(), Ok(0));
        assert_eq!(store.authorize(&principal(), action), Ok(()));
        assert!(!store.is_ready_for_new_connections());

        reader.push(Ok(replacement));
        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Reloaded { revision: 1 })
        );
        assert!(store.is_ready_for_new_connections());
        assert!(store.lookup("alice").unwrap().is_some());
    }

    #[test]
    fn unchanged_checkpoint_restores_new_connection_readiness() {
        let root = root();
        let (_provisioner, _authority, checkpoint) = provision(root.path(), true);
        let reader = Arc::new(FakeCheckpointReader::new(Ok(checkpoint)));
        let store = RuntimeAccountStore::open(&config(root.path()), reader.clone()).unwrap();

        reader.push(Err(CheckpointReadError::Unavailable));
        assert!(matches!(
            store.reload_once(),
            RuntimeAccountReload::Degraded(RuntimeAccountStoreError::CheckpointRead(
                CheckpointReadError::Unavailable
            ))
        ));
        assert!(!store.is_ready_for_new_connections());

        reader.push(Ok(checkpoint));
        assert_eq!(
            store.reload_once(),
            RuntimeAccountReload::Healthy(ReloadOutcome::Unchanged)
        );
        assert!(store.is_ready_for_new_connections());
        assert!(store.lookup("alice").unwrap().is_some());
    }
}
