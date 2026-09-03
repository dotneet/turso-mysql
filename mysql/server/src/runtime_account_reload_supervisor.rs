//! Periodic ownership of runtime account-store reloads.
//!
//! Reloads run on one dedicated thread.  The thread sleeps before its first
//! tick and between completed ticks, so a slow checkpoint read cannot create a
//! queue of overdue reloads.  Shutdown requests mark the account store first,
//! then wake the thread and any checkpoint wait owned by it.

use std::{
    error::Error,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use crate::runtime_config::{MAX_RELOAD_INTERVAL, MIN_RELOAD_INTERVAL};
use crate::{RuntimeAccountReload, RuntimeAccountStore};

/// One background owner for periodic account-store reloads.
///
/// The owner must be stopped and joined. Dropping it requests shutdown and
/// joins the worker as a final safety net, so this type never detaches a live
/// reload thread.
#[must_use = "an account reload supervisor must be joined so its status is observed"]
pub struct RuntimeAccountReloadSupervisor {
    accounts: Arc<RuntimeAccountStore>,
    control: Arc<RuntimeAccountReloadSupervisorControl>,
    handle: Option<thread::JoinHandle<Result<(), RuntimeAccountReloadSupervisorError>>>,
    completion: Option<mpsc::Receiver<Result<(), RuntimeAccountReloadSupervisorError>>>,
    completion_result: Option<Result<(), RuntimeAccountReloadSupervisorError>>,
}

impl RuntimeAccountReloadSupervisor {
    /// Starts one joinable periodic account-store reload owner.
    pub(crate) fn spawn(
        accounts: Arc<RuntimeAccountStore>,
        interval: Duration,
    ) -> Result<Self, RuntimeAccountReloadSupervisorSpawnError> {
        spawn_runtime_account_reload_supervisor(accounts, interval)
    }

    /// Requests shutdown and wakes a sleeping or checkpoint-waiting worker.
    pub(crate) fn request_stop(&self) {
        self.request_stop_inner();
    }

    /// Waits up to `deadline` without detaching the worker. On timeout, the
    /// handle remains owned and its drop guard will still join the worker.
    pub(crate) fn join_until(
        &mut self,
        deadline: Instant,
    ) -> Result<(), RuntimeAccountReloadSupervisorJoinError> {
        if self.handle.is_none() {
            return Ok(());
        }
        if self.completion_result.is_none() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(RuntimeAccountReloadSupervisorJoinError::TimedOut)?;
            let completion = self.completion.as_ref().ok_or({
                RuntimeAccountReloadSupervisorJoinError::Worker(
                    RuntimeAccountReloadSupervisorError::Panicked,
                )
            })?;
            match completion.recv_timeout(remaining) {
                Ok(result) => self.completion_result = Some(result),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(RuntimeAccountReloadSupervisorJoinError::TimedOut);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.completion_result =
                        Some(Err(RuntimeAccountReloadSupervisorError::Panicked));
                }
            }
        }
        while !self
            .handle
            .as_ref()
            .expect("a completed reload worker must retain its join handle")
            .is_finished()
        {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(RuntimeAccountReloadSupervisorJoinError::TimedOut);
            };
            thread::sleep(remaining.min(Duration::from_millis(1)));
        }
        let handle = self
            .handle
            .take()
            .expect("a finished reload worker must retain its join handle");
        let thread_result = join_reload_handle(handle);
        self.completion.take();
        let completion_result = self
            .completion_result
            .take()
            .expect("a finished reload worker must retain its completion result");
        match (thread_result, completion_result) {
            (Err(error), _) | (_, Err(error)) => {
                Err(RuntimeAccountReloadSupervisorJoinError::Worker(error))
            }
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn request_stop_inner(&self) {
        self.accounts.begin_shutdown();
        self.control.request_stop();
    }

    fn join_unbounded(&mut self) -> Result<(), RuntimeAccountReloadSupervisorError> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let completion_result = self.completion_result.take().or_else(|| {
            self.completion
                .take()
                .and_then(|completion| completion.recv().ok())
        });
        let result = join_reload_handle(handle);
        match result {
            Ok(()) => completion_result.unwrap_or(Ok(())),
            Err(error) => Err(error),
        }
    }
}

impl Drop for RuntimeAccountReloadSupervisor {
    fn drop(&mut self) {
        self.request_stop_inner();
        if self.handle.is_none() {
            return;
        }
        let _ = self.join_unbounded();
    }
}

impl fmt::Debug for RuntimeAccountReloadSupervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeAccountReloadSupervisor")
            .field("accounts", &"<retained>")
            .field("control", &"<retained>")
            .field("handle", &"<redacted>")
            .finish()
    }
}

/// A periodic reload worker ended without exposing checkpoint or panic data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAccountReloadSupervisorError {
    /// The reload worker panicked; its payload is intentionally discarded.
    Panicked,
}

impl fmt::Display for RuntimeAccountReloadSupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("runtime account reload supervisor panicked")
    }
}

impl Error for RuntimeAccountReloadSupervisorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

/// Waiting for a periodic reload worker either completed or reached the
/// caller's deadline. The worker remains owned after a timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAccountReloadSupervisorJoinError {
    /// The deadline elapsed before the worker completed.
    TimedOut,
    /// The worker completed with a typed, redacted status.
    Worker(RuntimeAccountReloadSupervisorError),
}

impl fmt::Display for RuntimeAccountReloadSupervisorJoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => f.write_str("runtime account reload supervisor join timed out"),
            Self::Worker(error) => error.fmt(f),
        }
    }
}

impl Error for RuntimeAccountReloadSupervisorJoinError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TimedOut => None,
            Self::Worker(error) => Some(error),
        }
    }
}

/// Starting the account reload supervisor failed without exposing runtime
/// configuration details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAccountReloadSupervisorSpawnError {
    /// The interval is outside the runtime's accepted one-to-sixty-second range.
    IntervalOutOfRange,
    /// The operating system refused to create the worker thread.
    SpawnUnavailable,
}

impl fmt::Display for RuntimeAccountReloadSupervisorSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntervalOutOfRange => {
                f.write_str("runtime account reload interval is out of range")
            }
            Self::SpawnUnavailable => {
                f.write_str("runtime account reload supervisor could not start")
            }
        }
    }
}

impl Error for RuntimeAccountReloadSupervisorSpawnError {}

/// Starts one joinable periodic account-store reload owner.
///
/// The first reload starts only after `interval` has elapsed. This function is
/// crate-private until the Unix listener owns the supervisor as part of its
/// runtime lifecycle.
pub(crate) fn spawn_runtime_account_reload_supervisor(
    accounts: Arc<RuntimeAccountStore>,
    interval: Duration,
) -> Result<RuntimeAccountReloadSupervisor, RuntimeAccountReloadSupervisorSpawnError> {
    validate_reload_interval(interval)?;

    let control = Arc::new(RuntimeAccountReloadSupervisorControl::default());
    let worker_accounts = Arc::clone(&accounts);
    let panic_accounts = Arc::clone(&accounts);
    let worker_control = Arc::clone(&control);
    let (completion_sender, completion_receiver) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("turso-mysql-account-reload".to_owned())
        .spawn(move || {
            let result = match catch_unwind(AssertUnwindSafe(|| {
                run_reload_supervisor(worker_accounts, worker_control, interval)
            })) {
                Ok(result) => result,
                Err(_) => {
                    panic_accounts.begin_shutdown();
                    Err(RuntimeAccountReloadSupervisorError::Panicked)
                }
            };
            let _ = completion_sender.send(result.clone());
            result
        })
        .map_err(|_| RuntimeAccountReloadSupervisorSpawnError::SpawnUnavailable)?;
    Ok(RuntimeAccountReloadSupervisor {
        accounts,
        control,
        handle: Some(handle),
        completion: Some(completion_receiver),
        completion_result: None,
    })
}

struct RuntimeAccountReloadSupervisorControl {
    state: Mutex<RuntimeAccountReloadSupervisorState>,
    wake: Condvar,
}

impl Default for RuntimeAccountReloadSupervisorControl {
    fn default() -> Self {
        Self {
            state: Mutex::new(RuntimeAccountReloadSupervisorState::Running),
            wake: Condvar::new(),
        }
    }
}

impl RuntimeAccountReloadSupervisorControl {
    fn request_stop(&self) {
        let mut state = self
            .state
            .lock()
            .expect("reload supervisor control must not be poisoned");
        *state = RuntimeAccountReloadSupervisorState::StopRequested;
        self.wake.notify_one();
    }

    fn wait_for_tick(&self, interval: Duration) -> bool {
        let state = self
            .state
            .lock()
            .expect("reload supervisor control must not be poisoned");
        let (state, _) = self
            .wake
            .wait_timeout_while(state, interval, |state| {
                matches!(state, RuntimeAccountReloadSupervisorState::Running)
            })
            .expect("reload supervisor control must not be poisoned");
        matches!(*state, RuntimeAccountReloadSupervisorState::StopRequested)
    }
}

#[derive(Clone, Copy)]
enum RuntimeAccountReloadSupervisorState {
    Running,
    StopRequested,
}

fn run_reload_supervisor(
    accounts: Arc<RuntimeAccountStore>,
    control: Arc<RuntimeAccountReloadSupervisorControl>,
    interval: Duration,
) -> Result<(), RuntimeAccountReloadSupervisorError> {
    run_reload_loop(control, interval, || accounts.reload_once())
}

fn run_reload_loop(
    control: Arc<RuntimeAccountReloadSupervisorControl>,
    interval: Duration,
    mut reload_once: impl FnMut() -> RuntimeAccountReload,
) -> Result<(), RuntimeAccountReloadSupervisorError> {
    loop {
        if control.wait_for_tick(interval) {
            return Ok(());
        }

        match reload_once() {
            RuntimeAccountReload::Healthy(_) | RuntimeAccountReload::Degraded(_) => {}
        }
    }
}

fn validate_reload_interval(
    interval: Duration,
) -> Result<(), RuntimeAccountReloadSupervisorSpawnError> {
    if !(MIN_RELOAD_INTERVAL..=MAX_RELOAD_INTERVAL).contains(&interval) {
        return Err(RuntimeAccountReloadSupervisorSpawnError::IntervalOutOfRange);
    }
    Ok(())
}

fn join_reload_handle(
    handle: thread::JoinHandle<Result<(), RuntimeAccountReloadSupervisorError>>,
) -> Result<(), RuntimeAccountReloadSupervisorError> {
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(RuntimeAccountReloadSupervisorError::Panicked),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc, Arc, Mutex,
        },
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{
        AccountDefinition, AccountGenerationBuilder, AccountId, AccountStoreCheckpoint,
        AccountStoreCheckpointAuthority, AccountStoreCheckpointReader,
        AccountStoreCheckpointRequest, AccountStoreCheckpointResponse, CheckpointPersistence,
        OfflineAccountProvisioner, ReloadOutcome, RuntimeConfig, RuntimeLimits, RuntimeTimeouts,
        UnixSocketConfig, MIN_WRITE_LIMIT,
    };

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

    struct PendingCheckpointReader {
        checkpoint: AccountStoreCheckpoint,
        first: AtomicBool,
        calls: AtomicUsize,
        panic_on_reload: AtomicBool,
        pending: Mutex<Option<AccountStoreCheckpointResponse>>,
    }

    impl AccountStoreCheckpointReader for PendingCheckpointReader {
        fn request_checkpoint(
            &self,
            _authority: &crate::CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, crate::CheckpointReadError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            if self.first.swap(false, Ordering::AcqRel) {
                return Ok(AccountStoreCheckpointRequest::completed(
                    Ok(self.checkpoint),
                ));
            }
            if self.panic_on_reload.swap(false, Ordering::AcqRel) {
                panic!("test panic payload");
            }
            let (response, request) = AccountStoreCheckpointRequest::channel();
            *self.pending.lock().unwrap() = Some(response);
            Ok(request)
        }
    }

    fn account_root() -> tempfile::TempDir {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn account_config(root: &Path) -> RuntimeConfig {
        RuntimeConfig::new(
            None,
            Some(UnixSocketConfig::new("/run/turso", "mysql.sock").unwrap()),
            "/var/lib/turso/data",
            root,
            crate::CheckpointAuthorityId::new("runtime-control-plane").unwrap(),
            MIN_RELOAD_INTERVAL,
            RuntimeLimits::new(16, 16, MIN_WRITE_LIMIT, 16).unwrap(),
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

    fn provision_checkpoint(root: &Path) -> AccountStoreCheckpoint {
        let account_id = AccountId::from_bytes([7; 32]);
        let account = AccountDefinition::new("alice", account_id, true, [0x11; 32]);
        let mut authority = MemoryAuthority { checkpoint: None };
        let provisioner = OfflineAccountProvisioner::initialize(
            root,
            AccountGenerationBuilder::new().with_account(account),
            &mut authority,
        )
        .unwrap();
        provisioner.checkpoint().unwrap()
    }

    fn wait_for_pending(reader: &PendingCheckpointReader) -> AccountStoreCheckpointResponse {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(response) = reader.pending.lock().unwrap().take() {
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "supervisor did not start a reload"
            );
            thread::yield_now();
        }
    }

    fn healthy_reload() -> RuntimeAccountReload {
        RuntimeAccountReload::Healthy(ReloadOutcome::Unchanged)
    }

    #[test]
    fn validates_the_existing_runtime_interval_bounds() {
        assert_eq!(
            validate_reload_interval(MIN_RELOAD_INTERVAL.saturating_sub(Duration::from_nanos(1))),
            Err(RuntimeAccountReloadSupervisorSpawnError::IntervalOutOfRange)
        );
        assert_eq!(validate_reload_interval(MIN_RELOAD_INTERVAL), Ok(()));
        assert_eq!(validate_reload_interval(MAX_RELOAD_INTERVAL), Ok(()));
        assert_eq!(
            validate_reload_interval(MAX_RELOAD_INTERVAL + Duration::from_nanos(1)),
            Err(RuntimeAccountReloadSupervisorSpawnError::IntervalOutOfRange)
        );
    }

    #[test]
    fn waits_before_first_tick_and_wakes_without_waiting_for_the_interval() {
        let control = Arc::new(RuntimeAccountReloadSupervisorControl::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker_control = Arc::clone(&control);
        let worker_calls = Arc::clone(&calls);
        let worker = thread::spawn(move || {
            let _ = ready_tx.send(());
            go_rx.recv().unwrap();
            let result = run_reload_loop(worker_control, Duration::from_millis(40), || {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                let _ = started_tx.send(Instant::now());
                healthy_reload()
            });
            let _ = finished_tx.send(result.clone());
            result
        });

        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = Instant::now();
        go_tx.send(()).unwrap();
        let first_tick = started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(first_tick.duration_since(start) >= Duration::from_millis(40));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        control.request_stop();
        let result = finished_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(worker.join().unwrap(), result);
    }

    #[test]
    fn a_slow_tick_does_not_create_overlap_or_a_backlog() {
        let control = Arc::new(RuntimeAccountReloadSupervisorControl::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_control = Arc::clone(&control);
        let worker_calls = Arc::clone(&calls);
        let worker = thread::spawn(move || {
            run_reload_loop(worker_control, Duration::from_millis(10), || {
                let call = worker_calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    let _ = started_tx.send(Instant::now());
                    release_rx.recv().unwrap();
                    let _ = completed_tx.send(Instant::now());
                } else {
                    let _ = second_tx.send(Instant::now());
                }
                healthy_reload()
            })
        });

        let first_tick = started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        let completed_tick = completed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let second_tick = second_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(completed_tick.duration_since(first_tick) >= Duration::from_millis(50));
        assert!(second_tick.duration_since(completed_tick) >= Duration::from_millis(10));

        control.request_stop();
        assert_eq!(worker.join().unwrap(), Ok(()));
    }

    #[test]
    fn join_redacts_a_worker_panic() {
        let handle = thread::spawn(|| -> Result<(), RuntimeAccountReloadSupervisorError> {
            panic!("test panic payload");
        });
        let error = join_reload_handle(handle).unwrap_err();
        assert!(matches!(
            error,
            RuntimeAccountReloadSupervisorError::Panicked
        ));
        assert_eq!(
            error.to_string(),
            "runtime account reload supervisor panicked"
        );
        assert!(!format!("{error:?}").contains("test panic payload"));
    }

    #[test]
    fn completion_notice_does_not_allow_join_to_cross_its_deadline() {
        let root = account_root();
        let checkpoint = provision_checkpoint(root.path());
        let reader = Arc::new(PendingCheckpointReader {
            checkpoint,
            first: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            panic_on_reload: AtomicBool::new(false),
            pending: Mutex::new(None),
        });
        let accounts =
            Arc::new(RuntimeAccountStore::open(&account_config(root.path()), reader).unwrap());
        let control = Arc::new(RuntimeAccountReloadSupervisorControl::default());
        let (completion_tx, completion_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            completion_tx.send(Ok(())).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        });
        let mut supervisor = RuntimeAccountReloadSupervisor {
            accounts,
            control,
            handle: Some(handle),
            completion: Some(completion_rx),
            completion_result: None,
        };

        assert_eq!(
            supervisor.join_until(Instant::now() + Duration::from_millis(5)),
            Err(RuntimeAccountReloadSupervisorJoinError::TimedOut)
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            supervisor.join_until(Instant::now() + Duration::from_secs(1)),
            Ok(())
        );
    }

    #[test]
    fn a_worker_panic_fails_closed_before_it_reports_the_panic() {
        let root = account_root();
        let checkpoint = provision_checkpoint(root.path());
        let reader = Arc::new(PendingCheckpointReader {
            checkpoint,
            first: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            panic_on_reload: AtomicBool::new(true),
            pending: Mutex::new(None),
        });
        let accounts = Arc::new(
            RuntimeAccountStore::open(&account_config(root.path()), reader.clone()).unwrap(),
        );
        let mut supervisor =
            RuntimeAccountReloadSupervisor::spawn(Arc::clone(&accounts), MIN_RELOAD_INTERVAL)
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while reader.calls.load(Ordering::Acquire) < 2 {
            assert!(
                Instant::now() < deadline,
                "supervisor did not start a reload"
            );
            thread::yield_now();
        }

        let error = supervisor
            .join_until(Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeAccountReloadSupervisorJoinError::Worker(
                RuntimeAccountReloadSupervisorError::Panicked
            )
        ));
        assert!(!accounts.is_ready_for_new_connections());
    }

    #[test]
    fn stop_wakes_a_checkpoint_wait_and_discards_a_late_reply() {
        let root = account_root();
        let checkpoint = provision_checkpoint(root.path());
        let reader = Arc::new(PendingCheckpointReader {
            checkpoint,
            first: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            panic_on_reload: AtomicBool::new(false),
            pending: Mutex::new(None),
        });
        let accounts = Arc::new(
            RuntimeAccountStore::open(&account_config(root.path()), reader.clone()).unwrap(),
        );
        let mut supervisor =
            RuntimeAccountReloadSupervisor::spawn(Arc::clone(&accounts), MIN_RELOAD_INTERVAL)
                .unwrap();
        let response = wait_for_pending(&reader);

        let started = Instant::now();
        supervisor.request_stop();
        let result = supervisor.join_until(Instant::now() + Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(result, Ok(()));
        assert!(response.is_cancelled());
        assert!(response.complete(Ok(checkpoint)));
        assert_eq!(accounts.revision(), Ok(0));
        assert!(!accounts.is_ready_for_new_connections());
    }

    #[test]
    fn scheduled_failure_blocks_admission_until_a_later_exact_reload_recovers() {
        let root = account_root();
        let checkpoint = provision_checkpoint(root.path());
        let reader = Arc::new(PendingCheckpointReader {
            checkpoint,
            first: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            panic_on_reload: AtomicBool::new(false),
            pending: Mutex::new(None),
        });
        let accounts = Arc::new(
            RuntimeAccountStore::open(&account_config(root.path()), reader.clone()).unwrap(),
        );
        let mut supervisor =
            RuntimeAccountReloadSupervisor::spawn(Arc::clone(&accounts), MIN_RELOAD_INTERVAL)
                .unwrap();

        let failed = wait_for_pending(&reader);
        assert!(failed.complete(Err(crate::CheckpointReadError::Unavailable)));
        let recovering = wait_for_pending(&reader);
        assert!(!accounts.is_ready_for_new_connections());
        assert!(recovering.complete(Ok(checkpoint)));

        let deadline = Instant::now() + Duration::from_secs(1);
        while !accounts.is_ready_for_new_connections() {
            assert!(
                Instant::now() < deadline,
                "an exact scheduled reload did not restore readiness"
            );
            thread::yield_now();
        }
        supervisor.request_stop();
        assert_eq!(
            supervisor.join_until(Instant::now() + Duration::from_secs(1)),
            Ok(())
        );
    }
}
