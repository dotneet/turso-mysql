//! Process-style ownership for the blocking Unix MySQL runtime.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Condvar, Mutex,
    },
    thread,
    time::Instant,
};

use crate::{
    AccountStoreCheckpointReader, ConnectionLimitError, RuntimeAccountReload, RuntimeConfig,
    RuntimeUnixConnectionSpawnError, RuntimeUnixConnectionWorker, RuntimeUnixConnectionWorkerError,
    RuntimeUnixListener, RuntimeUnixListenerError, RuntimeUnixShutdownReport,
};

/// A blocking Unix MySQL server with joinable ownership of every connection worker.
pub struct RuntimeUnixServer {
    listener: Arc<RuntimeUnixListener>,
    control: Arc<RuntimeUnixServerControl>,
    events: SyncSender<ReaperEvent>,
    reaper: Mutex<RuntimeUnixReaper>,
    reaper_completion: Arc<ReaperCompletion>,
    reaper_snapshot: Arc<Mutex<ReaperSnapshot>>,
    shutdown_gate: ShutdownGate,
}

impl RuntimeUnixServer {
    /// Binds the Unix listener and starts its sole connection-worker reaper.
    pub fn bind(
        config: &RuntimeConfig,
        checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
    ) -> Result<Self, RuntimeUnixServerBindError> {
        let listener = Arc::new(
            RuntimeUnixListener::bind(config, checkpoint_reader)
                .map_err(RuntimeUnixServerBindError::Listener)?,
        );
        let control = Arc::new(RuntimeUnixServerControl::new());
        let reaper_completion = Arc::new(ReaperCompletion::new());
        let reaper_snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let (events, receiver) = worker_event_channel(config.limits().max_connections());
        let reaper = spawn_reaper(
            receiver,
            Arc::clone(&listener),
            Arc::clone(&control),
            Arc::clone(&reaper_completion),
            Arc::clone(&reaper_snapshot),
        )
        .map_err(|()| RuntimeUnixServerBindError::ReaperUnavailable)?;

        Ok(Self {
            listener,
            control,
            events,
            reaper: Mutex::new(RuntimeUnixReaper {
                handle: Some(reaper),
                terminal: None,
            }),
            reaper_completion,
            reaper_snapshot,
            shutdown_gate: ShutdownGate::new(),
        })
    }

    /// Runs the accept loop in the calling thread. A server may be run only once.
    pub fn run(&self) -> Result<(), RuntimeUnixServerRunError> {
        self.control.begin_run()?;
        let _run = RunGuard::new(&self.control, &self.events);
        let mut next_worker_token = Some(1_u64);

        loop {
            match self.listener.accept() {
                Ok(stream) => {
                    let token = next_worker_token.ok_or_else(|| {
                        self.fail_closed(RuntimeUnixServerFailure::WorkerTokenExhausted)
                    })?;
                    next_worker_token = token.checked_add(1);
                    let finished = self.events.clone();
                    let worker = self
                        .listener
                        .spawn_protocol(stream, move || {
                            let _ = finished.send(ReaperEvent::Finished(token));
                        })
                        .map_err(|error| self.handle_spawn_error(error))?;
                    if let Err(error) = self.events.send(ReaperEvent::Started {
                        token,
                        worker: Box::new(worker),
                    }) {
                        self.handle_lost_reaper(error.0);
                        return Err(RuntimeUnixServerRunError::ReaperUnavailable);
                    }
                }
                Err(RuntimeUnixListenerError::AccountNotReady) => {
                    if !self.listener.wait_until_ready_or_shutdown() {
                        if self.listener.is_shutting_down() {
                            return self.stopped_run_result();
                        }
                        return Err(
                            self.fail_closed(RuntimeUnixServerFailure::AccountReloadUnavailable)
                        );
                    }
                }
                Err(RuntimeUnixListenerError::ShuttingDown) => {
                    return self.stopped_run_result();
                }
                Err(error) if client_accept_error(&error) => {}
                Err(_) => {
                    return Err(self.fail_closed(RuntimeUnixServerFailure::ListenerUnavailable));
                }
            }
        }
    }

    /// Performs one serialized account reload for an explicit freshness barrier.
    pub fn reload_accounts_once(&self) -> RuntimeAccountReload {
        self.listener.reload_accounts_once()
    }

    /// Returns whether the server can admit a new authentication attempt.
    pub fn is_ready_for_new_connections(&self) -> bool {
        self.listener.is_ready_for_new_connections()
    }

    /// Stops acceptance and returns one bounded, redacted ownership report.
    pub fn shutdown(&self) -> RuntimeUnixServerShutdownReport {
        let deadline = Instant::now() + self.listener.shutdown_timeout();
        self.control.begin_shutdown();
        let Some(_shutdown) = self.shutdown_gate.enter_until(deadline) else {
            let listener = self.listener.shutdown_until(deadline);
            let accept_loop = if self.control.run_active() {
                RuntimeUnixAcceptLoopShutdown::TimedOut
            } else {
                RuntimeUnixAcceptLoopShutdown::Stopped
            };
            return self.shutdown_report(
                listener,
                accept_loop,
                RuntimeUnixWorkerReaperShutdown::TimedOut,
            );
        };
        let listener = self.listener.shutdown_until(deadline);
        let accept_loop = if self.control.wait_for_run_until(deadline) {
            self.control.send_accept_stopped(&self.events);
            RuntimeUnixAcceptLoopShutdown::Stopped
        } else {
            RuntimeUnixAcceptLoopShutdown::TimedOut
        };
        let worker_reaper = self.join_reaper_until(deadline);
        self.shutdown_report(listener, accept_loop, worker_reaper)
    }

    fn stopped_run_result(&self) -> Result<(), RuntimeUnixServerRunError> {
        match self.control.failure() {
            Some(failure) => Err(failure.into()),
            None => Ok(()),
        }
    }

    fn handle_spawn_error(
        &self,
        error: RuntimeUnixConnectionSpawnError,
    ) -> RuntimeUnixServerRunError {
        match error {
            RuntimeUnixConnectionSpawnError::SpawnUnavailable => {
                self.fail_closed(RuntimeUnixServerFailure::WorkerSpawnUnavailable)
            }
            RuntimeUnixConnectionSpawnError::Accept(_) => {
                unreachable!("an already accepted stream cannot fail a second accept")
            }
        }
    }

    fn handle_lost_reaper(&self, event: ReaperEvent) {
        let failure = self.fail_closed(RuntimeUnixServerFailure::ReaperUnavailable);
        debug_assert_eq!(failure, RuntimeUnixServerRunError::ReaperUnavailable);
        if let ReaperEvent::Started { worker, .. } = event {
            let _ = worker.join();
        }
    }

    fn fail_closed(&self, failure: RuntimeUnixServerFailure) -> RuntimeUnixServerRunError {
        self.control.record_failure(failure);
        let _ = self
            .listener
            .shutdown_until(Instant::now() + self.listener.shutdown_timeout());
        failure.into()
    }

    fn join_reaper_until(&self, deadline: Instant) -> RuntimeUnixWorkerReaperShutdown {
        let mut reaper = self
            .reaper
            .lock()
            .expect("Unix server reaper state must not be poisoned");
        reaper.join_until(&self.reaper_completion, deadline)
    }

    fn shutdown_report(
        &self,
        listener: RuntimeUnixShutdownReport,
        accept_loop: RuntimeUnixAcceptLoopShutdown,
        worker_reaper: RuntimeUnixWorkerReaperShutdown,
    ) -> RuntimeUnixServerShutdownReport {
        let snapshot = *self
            .reaper_snapshot
            .lock()
            .expect("Unix server reaper snapshot must not be poisoned");
        RuntimeUnixServerShutdownReport {
            listener,
            accept_loop,
            worker_reaper,
            workers_started: snapshot.started,
            workers_joined: snapshot.joined,
            connection_errors: snapshot.connection_errors,
            worker_panics: snapshot.panics,
            remaining_workers: snapshot.remaining,
        }
    }
}

impl fmt::Debug for RuntimeUnixServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeUnixServer")
            .field("listener", &self.listener)
            .field("control", &self.control)
            .field("reaper", &"<retained>")
            .finish()
    }
}

impl Drop for RuntimeUnixServer {
    fn drop(&mut self) {
        let _ = self.shutdown();
        self.control.wait_for_run();
        self.control.send_accept_stopped(&self.events);
        if let Some(handle) = self
            .reaper
            .get_mut()
            .expect("Unix server reaper state must not be poisoned")
            .handle
            .take()
        {
            let _ = handle.join();
        }
    }
}

/// Starting a supervised Unix server failed without exposing runtime paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeUnixServerBindError {
    /// The underlying listener could not be opened.
    Listener(RuntimeUnixListenerError),
    /// The connection-worker reaper thread could not be started.
    ReaperUnavailable,
}

impl fmt::Display for RuntimeUnixServerBindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listener(error) => write!(f, "Unix server listener failed: {error}"),
            Self::ReaperUnavailable => f.write_str("Unix server worker reaper could not start"),
        }
    }
}

impl Error for RuntimeUnixServerBindError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Listener(error) => Some(error),
            Self::ReaperUnavailable => None,
        }
    }
}

/// A blocking accept loop stopped for a redacted terminal reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeUnixServerRunError {
    /// This server has already entered its accept loop.
    AlreadyRun,
    /// Shutdown began before the accept loop started.
    ShuttingDown,
    /// The listener could no longer accept safely.
    ListenerUnavailable,
    /// The account reload owner stopped outside normal server shutdown.
    AccountReloadUnavailable,
    /// The non-reused worker token space was exhausted.
    WorkerTokenExhausted,
    /// A connection worker could not be started.
    WorkerSpawnUnavailable,
    /// The worker reaper was no longer available.
    ReaperUnavailable,
    /// A connection worker panicked.
    WorkerPanicked,
}

impl fmt::Display for RuntimeUnixServerRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AlreadyRun => "Unix server accept loop has already run",
            Self::ShuttingDown => "Unix server is shutting down",
            Self::ListenerUnavailable => "Unix server listener became unavailable",
            Self::AccountReloadUnavailable => "Unix server account reload owner stopped",
            Self::WorkerTokenExhausted => "Unix server worker tokens are exhausted",
            Self::WorkerSpawnUnavailable => "Unix server connection worker could not start",
            Self::ReaperUnavailable => "Unix server worker reaper became unavailable",
            Self::WorkerPanicked => "Unix server connection worker panicked",
        })
    }
}

impl Error for RuntimeUnixServerRunError {}

/// Whether the blocking accept loop stopped within the shared deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeUnixAcceptLoopShutdown {
    /// The accept loop returned or never started.
    Stopped,
    /// The accept loop outlived the shared shutdown deadline.
    TimedOut,
}

/// Whether the connection-worker reaper stopped within the shared deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeUnixWorkerReaperShutdown {
    /// The reaper stopped and its thread was joined.
    Stopped,
    /// The reaper outlived the shared shutdown deadline and remains joinable.
    TimedOut,
    /// The reaper panicked; its payload was discarded.
    Failed,
}

struct RuntimeUnixReaper {
    handle: Option<thread::JoinHandle<Result<(), ()>>>,
    terminal: Option<RuntimeUnixWorkerReaperShutdown>,
}

impl RuntimeUnixReaper {
    fn join_until(
        &mut self,
        completion: &ReaperCompletion,
        deadline: Instant,
    ) -> RuntimeUnixWorkerReaperShutdown {
        if let Some(terminal) = self.terminal {
            return terminal;
        }
        let Some(handle) = self.handle.as_ref() else {
            unreachable!("a Unix reaper without a handle must retain its terminal status")
        };
        if !completion.wait_until(deadline) && !handle.is_finished() {
            return RuntimeUnixWorkerReaperShutdown::TimedOut;
        }
        while !self
            .handle
            .as_ref()
            .expect("a completing Unix reaper must retain its handle")
            .is_finished()
        {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return RuntimeUnixWorkerReaperShutdown::TimedOut;
            };
            thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
        }
        let handle = self
            .handle
            .take()
            .expect("finished Unix server reaper must remain joinable");
        let terminal = match handle.join() {
            Ok(Ok(())) => RuntimeUnixWorkerReaperShutdown::Stopped,
            Ok(Err(())) | Err(_) => RuntimeUnixWorkerReaperShutdown::Failed,
        };
        self.terminal = Some(terminal);
        terminal
    }
}

/// The bounded result of stopping one supervised Unix server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeUnixServerShutdownReport {
    listener: RuntimeUnixShutdownReport,
    accept_loop: RuntimeUnixAcceptLoopShutdown,
    worker_reaper: RuntimeUnixWorkerReaperShutdown,
    workers_started: usize,
    workers_joined: usize,
    connection_errors: usize,
    worker_panics: usize,
    remaining_workers: usize,
}

impl RuntimeUnixServerShutdownReport {
    /// Returns the listener-owned shutdown result.
    pub const fn listener(&self) -> &RuntimeUnixShutdownReport {
        &self.listener
    }

    /// Returns whether the accept loop stopped within the shared deadline.
    pub const fn accept_loop(&self) -> RuntimeUnixAcceptLoopShutdown {
        self.accept_loop
    }

    /// Returns whether the worker reaper stopped within the shared deadline.
    pub const fn worker_reaper(&self) -> RuntimeUnixWorkerReaperShutdown {
        self.worker_reaper
    }

    /// Returns the number of workers transferred to the reaper.
    pub const fn workers_started(&self) -> usize {
        self.workers_started
    }

    /// Returns the number of workers joined by the reaper.
    pub const fn workers_joined(&self) -> usize {
        self.workers_joined
    }

    /// Returns the number of ordinary, redacted connection failures.
    pub const fn connection_errors(&self) -> usize {
        self.connection_errors
    }

    /// Returns the number of worker panics.
    pub const fn worker_panics(&self) -> usize {
        self.worker_panics
    }

    /// Returns the number of worker handles still retained by the reaper.
    pub const fn remaining_workers(&self) -> usize {
        self.remaining_workers
    }

    /// Returns whether every owned runtime thread and connection drained.
    pub const fn drained(&self) -> bool {
        self.listener.drained()
            && matches!(self.accept_loop, RuntimeUnixAcceptLoopShutdown::Stopped)
            && matches!(self.worker_reaper, RuntimeUnixWorkerReaperShutdown::Stopped)
            && self.remaining_workers == 0
    }
}

fn client_accept_error(error: &RuntimeUnixListenerError) -> bool {
    matches!(
        error,
        RuntimeUnixListenerError::PeerCredentialsUnavailable
            | RuntimeUnixListenerError::PeerUidMismatch
            | RuntimeUnixListenerError::ConnectionLimit(
                ConnectionLimitError::ConnectionsExhausted
                    | ConnectionLimitError::AdmissionsExhausted
            )
            | RuntimeUnixListenerError::TransportConfiguration
    )
}

fn worker_event_channel(
    max_connections: usize,
) -> (SyncSender<ReaperEvent>, Receiver<ReaperEvent>) {
    let capacity = max_connections
        .checked_mul(2)
        .and_then(|capacity| capacity.checked_add(1))
        .expect("validated Unix connection limit must fit its worker event queue");
    mpsc::sync_channel(capacity)
}

fn spawn_reaper(
    receiver: Receiver<ReaperEvent>,
    listener: Arc<RuntimeUnixListener>,
    control: Arc<RuntimeUnixServerControl>,
    completion: Arc<ReaperCompletion>,
    snapshot: Arc<Mutex<ReaperSnapshot>>,
) -> Result<thread::JoinHandle<Result<(), ()>>, ()> {
    thread::Builder::new()
        .name("turso-mysql-reaper".to_owned())
        .spawn(move || {
            let _finished = ReaperCompletionGuard::new(completion);
            let panic_control = Arc::clone(&control);
            let panic_listener = Arc::clone(&listener);
            run_reaper_safely(
                receiver,
                snapshot,
                move || {
                    panic_control.record_failure(RuntimeUnixServerFailure::WorkerPanicked);
                    let _ = panic_listener
                        .shutdown_until(Instant::now() + panic_listener.shutdown_timeout());
                },
                move || {
                    control.record_failure(RuntimeUnixServerFailure::ReaperUnavailable);
                    let _ = listener.shutdown_until(Instant::now() + listener.shutdown_timeout());
                },
            )
        })
        .map_err(|_| ())
}

#[cfg(test)]
fn run_reaper<F>(
    receiver: Receiver<ReaperEvent>,
    snapshot: Arc<Mutex<ReaperSnapshot>>,
    mut worker_panicked: F,
) where
    F: FnMut(),
{
    let mut workers = ReaperWorkers::new(snapshot);
    run_reaper_loop(&receiver, &mut workers, &mut worker_panicked);
}

fn run_reaper_safely<F, G>(
    receiver: Receiver<ReaperEvent>,
    snapshot: Arc<Mutex<ReaperSnapshot>>,
    mut worker_panicked: F,
    mut reaper_failed: G,
) -> Result<(), ()>
where
    F: FnMut(),
    G: FnMut(),
{
    let mut workers = ReaperWorkers::new(Arc::clone(&snapshot));
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_reaper_loop(&receiver, &mut workers, &mut worker_panicked);
    }));
    if result.is_ok() {
        return Ok(());
    }

    let _ = catch_unwind(AssertUnwindSafe(&mut reaper_failed));
    let mut retained = workers.take_workers();
    let mut queued = 0;
    loop {
        match receiver.recv() {
            Ok(ReaperEvent::Started { worker, .. }) => {
                retained.push(worker);
                queued += 1;
            }
            Ok(ReaperEvent::Finished(_)) => {}
            Ok(ReaperEvent::AcceptStopped) | Err(_) => break,
        }
    }
    drop(receiver);
    join_workers_after_reaper_failure(retained, queued, &snapshot);
    Err(())
}

fn run_reaper_loop<F>(
    receiver: &Receiver<ReaperEvent>,
    workers: &mut ReaperWorkers,
    worker_panicked: &mut F,
) where
    F: FnMut(),
{
    let mut accept_stopped = false;
    while !accept_stopped || !workers.is_empty() {
        workers.join_finished(worker_panicked);
        if accept_stopped && workers.is_empty() {
            break;
        }
        match receive_reaper_event(receiver, workers.has_finished_notifications()) {
            ReaperReceive::Event(ReaperEvent::Started { token, worker }) => {
                workers.started(token, worker);
            }
            ReaperReceive::Event(ReaperEvent::Finished(token)) => workers.finished(token),
            ReaperReceive::Event(ReaperEvent::AcceptStopped) | ReaperReceive::Disconnected => {
                accept_stopped = true;
            }
            ReaperReceive::RetryFinishedWorkers => {}
        }
    }
}

fn receive_reaper_event(receiver: &Receiver<ReaperEvent>, poll: bool) -> ReaperReceive {
    if !poll {
        return match receiver.recv() {
            Ok(event) => ReaperReceive::Event(event),
            Err(_) => ReaperReceive::Disconnected,
        };
    }
    match receiver.recv_timeout(std::time::Duration::from_millis(1)) {
        Ok(event) => ReaperReceive::Event(event),
        Err(mpsc::RecvTimeoutError::Timeout) => ReaperReceive::RetryFinishedWorkers,
        Err(mpsc::RecvTimeoutError::Disconnected) => ReaperReceive::Disconnected,
    }
}

enum ReaperReceive {
    Event(ReaperEvent),
    RetryFinishedWorkers,
    Disconnected,
}

enum ReaperEvent {
    Started {
        token: u64,
        worker: Box<dyn ReapWorker>,
    },
    Finished(u64),
    AcceptStopped,
}

trait ReapWorker: Send {
    fn is_finished(&self) -> bool;
    fn join(self: Box<Self>) -> WorkerExit;
}

impl ReapWorker for RuntimeUnixConnectionWorker {
    fn is_finished(&self) -> bool {
        self.is_finished()
    }

    fn join(self: Box<Self>) -> WorkerExit {
        match (*self).join() {
            Ok(()) => WorkerExit::Normal,
            Err(RuntimeUnixConnectionWorkerError::Connection(_)) => WorkerExit::ConnectionError,
            Err(RuntimeUnixConnectionWorkerError::Panicked) => WorkerExit::Panicked,
        }
    }
}

#[derive(Clone, Copy)]
enum WorkerExit {
    Normal,
    ConnectionError,
    Panicked,
}

struct ReaperWorkers {
    workers: BTreeMap<u64, Box<dyn ReapWorker>>,
    finished_before_start: BTreeSet<u64>,
    finished: BTreeSet<u64>,
    snapshot: Arc<Mutex<ReaperSnapshot>>,
}

impl ReaperWorkers {
    fn new(snapshot: Arc<Mutex<ReaperSnapshot>>) -> Self {
        Self {
            workers: BTreeMap::new(),
            finished_before_start: BTreeSet::new(),
            finished: BTreeSet::new(),
            snapshot,
        }
    }

    fn started(&mut self, token: u64, worker: Box<dyn ReapWorker>) {
        let previous = self.workers.insert(token, worker);
        assert!(previous.is_none(), "Unix worker token must never be reused");
        let mut snapshot = self.snapshot();
        snapshot.started += 1;
        snapshot.remaining += 1;
        drop(snapshot);
        if self.finished_before_start.remove(&token) {
            self.finished.insert(token);
        }
    }

    fn finished(&mut self, token: u64) {
        if self.workers.contains_key(&token) {
            self.finished.insert(token);
        } else {
            self.finished_before_start.insert(token);
        }
    }

    fn join_finished<F>(&mut self, worker_panicked: &mut F)
    where
        F: FnMut(),
    {
        let ready = self
            .finished
            .iter()
            .copied()
            .filter(|token| {
                self.workers
                    .get(token)
                    .expect("finished Unix worker must be registered")
                    .is_finished()
            })
            .collect::<Vec<_>>();
        for token in ready {
            self.finished.remove(&token);
            let worker = self
                .workers
                .remove(&token)
                .expect("finished Unix worker must remain registered");
            let outcome = worker.join();
            let mut snapshot = self.snapshot();
            snapshot.joined += 1;
            snapshot.remaining -= 1;
            match outcome {
                WorkerExit::Normal => {}
                WorkerExit::ConnectionError => snapshot.connection_errors += 1,
                WorkerExit::Panicked => snapshot.panics += 1,
            }
            drop(snapshot);
            if matches!(outcome, WorkerExit::Panicked) {
                worker_panicked();
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.workers.is_empty() && self.finished_before_start.is_empty()
    }

    fn has_finished_notifications(&self) -> bool {
        !self.finished.is_empty()
    }

    fn take_workers(&mut self) -> Vec<Box<dyn ReapWorker>> {
        std::mem::take(&mut self.workers).into_values().collect()
    }

    fn snapshot(&self) -> std::sync::MutexGuard<'_, ReaperSnapshot> {
        self.snapshot
            .lock()
            .expect("Unix server reaper snapshot must not be poisoned")
    }
}

impl Drop for ReaperWorkers {
    fn drop(&mut self) {
        let workers = self.take_workers();
        join_workers_after_reaper_failure(workers, 0, &self.snapshot);
    }
}

fn join_workers_after_reaper_failure(
    workers: Vec<Box<dyn ReapWorker>>,
    queued: usize,
    snapshot: &Mutex<ReaperSnapshot>,
) {
    let mut joined = 0;
    let mut connection_errors = 0;
    let mut panics = 0;
    for worker in workers {
        joined += 1;
        match worker.join() {
            WorkerExit::Normal => {}
            WorkerExit::ConnectionError => connection_errors += 1,
            WorkerExit::Panicked => panics += 1,
        }
    }
    let mut snapshot = snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshot.started = snapshot.started.saturating_add(queued);
    snapshot.remaining = snapshot.remaining.saturating_add(queued);
    snapshot.joined = snapshot.joined.saturating_add(joined);
    snapshot.remaining = snapshot.remaining.saturating_sub(joined);
    snapshot.connection_errors = snapshot.connection_errors.saturating_add(connection_errors);
    snapshot.panics = snapshot.panics.saturating_add(panics);
}

#[derive(Default, Clone, Copy)]
struct ReaperSnapshot {
    started: usize,
    joined: usize,
    connection_errors: usize,
    panics: usize,
    remaining: usize,
}

struct ReaperCompletion {
    finished: Mutex<bool>,
    changed: Condvar,
}

impl ReaperCompletion {
    fn new() -> Self {
        Self {
            finished: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn wait_until(&self, deadline: Instant) -> bool {
        let mut finished = self
            .finished
            .lock()
            .expect("Unix server reaper completion must not be poisoned");
        while !*finished {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .changed
                .wait_timeout(finished, remaining)
                .expect("Unix server reaper completion must not be poisoned");
            finished = next;
            if timeout.timed_out() && !*finished {
                return false;
            }
        }
        true
    }

    fn finish(&self) {
        *self
            .finished
            .lock()
            .expect("Unix server reaper completion must not be poisoned") = true;
        self.changed.notify_all();
    }
}

struct ReaperCompletionGuard {
    completion: Arc<ReaperCompletion>,
}

impl ReaperCompletionGuard {
    fn new(completion: Arc<ReaperCompletion>) -> Self {
        Self { completion }
    }
}

impl Drop for ReaperCompletionGuard {
    fn drop(&mut self) {
        self.completion.finish();
    }
}

struct RunGuard<'a> {
    control: &'a RuntimeUnixServerControl,
    events: &'a SyncSender<ReaperEvent>,
}

impl<'a> RunGuard<'a> {
    fn new(control: &'a RuntimeUnixServerControl, events: &'a SyncSender<ReaperEvent>) -> Self {
        Self { control, events }
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.control.finish_run();
        self.control.send_accept_stopped(self.events);
    }
}

struct ShutdownGate {
    active: Mutex<bool>,
    changed: Condvar,
}

impl ShutdownGate {
    fn new() -> Self {
        Self {
            active: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn enter_until(&self, deadline: Instant) -> Option<ShutdownGuard<'_>> {
        let mut active = self
            .active
            .lock()
            .expect("Unix server shutdown gate must not be poisoned");
        while *active {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, timeout) = self
                .changed
                .wait_timeout(active, remaining)
                .expect("Unix server shutdown gate must not be poisoned");
            active = next;
            if timeout.timed_out() && *active {
                return None;
            }
        }
        *active = true;
        Some(ShutdownGuard { gate: self })
    }
}

struct ShutdownGuard<'a> {
    gate: &'a ShutdownGate,
}

impl Drop for ShutdownGuard<'_> {
    fn drop(&mut self) {
        *self
            .gate
            .active
            .lock()
            .expect("Unix server shutdown gate must not be poisoned") = false;
        self.gate.changed.notify_one();
    }
}

struct RuntimeUnixServerControl {
    state: Mutex<RuntimeUnixServerState>,
    changed: Condvar,
}

impl RuntimeUnixServerControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(RuntimeUnixServerState {
                run_started: false,
                run_active: false,
                shutdown_requested: false,
                accept_stopped_sent: false,
                failure: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn begin_run(&self) -> Result<(), RuntimeUnixServerRunError> {
        let mut state = self.lock();
        if state.run_started {
            return Err(RuntimeUnixServerRunError::AlreadyRun);
        }
        if state.shutdown_requested {
            return Err(RuntimeUnixServerRunError::ShuttingDown);
        }
        state.run_started = true;
        state.run_active = true;
        self.changed.notify_all();
        Ok(())
    }

    fn begin_shutdown(&self) {
        let mut state = self.lock();
        state.shutdown_requested = true;
        self.changed.notify_all();
    }

    fn record_failure(&self, failure: RuntimeUnixServerFailure) {
        let mut state = self.lock();
        state.shutdown_requested = true;
        state.failure.get_or_insert(failure);
        self.changed.notify_all();
    }

    fn failure(&self) -> Option<RuntimeUnixServerFailure> {
        self.lock().failure
    }

    fn run_active(&self) -> bool {
        self.lock().run_active
    }

    fn finish_run(&self) {
        let mut state = self.lock();
        assert!(
            state.run_active,
            "only the active Unix accept loop may finish"
        );
        state.run_active = false;
        self.changed.notify_all();
    }

    fn wait_for_run_until(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        while state.run_active {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("Unix server control must not be poisoned");
            state = next;
            if timeout.timed_out() && state.run_active {
                return false;
            }
        }
        true
    }

    fn wait_for_run(&self) {
        let mut state = self.lock();
        while state.run_active {
            state = self
                .changed
                .wait(state)
                .expect("Unix server control must not be poisoned");
        }
    }

    fn send_accept_stopped(&self, events: &SyncSender<ReaperEvent>) {
        let send = {
            let mut state = self.lock();
            if state.accept_stopped_sent {
                false
            } else {
                state.accept_stopped_sent = true;
                true
            }
        };
        if send {
            let _ = events.send(ReaperEvent::AcceptStopped);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeUnixServerState> {
        self.state
            .lock()
            .expect("Unix server control must not be poisoned")
    }
}

impl fmt::Debug for RuntimeUnixServerControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("RuntimeUnixServerControl")
            .field("run_started", &state.run_started)
            .field("run_active", &state.run_active)
            .field("shutdown_requested", &state.shutdown_requested)
            .field("failure", &state.failure)
            .finish()
    }
}

struct RuntimeUnixServerState {
    run_started: bool,
    run_active: bool,
    shutdown_requested: bool,
    accept_stopped_sent: bool,
    failure: Option<RuntimeUnixServerFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeUnixServerFailure {
    ListenerUnavailable,
    AccountReloadUnavailable,
    WorkerTokenExhausted,
    WorkerSpawnUnavailable,
    ReaperUnavailable,
    WorkerPanicked,
}

impl From<RuntimeUnixServerFailure> for RuntimeUnixServerRunError {
    fn from(failure: RuntimeUnixServerFailure) -> Self {
        match failure {
            RuntimeUnixServerFailure::ListenerUnavailable => Self::ListenerUnavailable,
            RuntimeUnixServerFailure::AccountReloadUnavailable => Self::AccountReloadUnavailable,
            RuntimeUnixServerFailure::WorkerTokenExhausted => Self::WorkerTokenExhausted,
            RuntimeUnixServerFailure::WorkerSpawnUnavailable => Self::WorkerSpawnUnavailable,
            RuntimeUnixServerFailure::ReaperUnavailable => Self::ReaperUnavailable,
            RuntimeUnixServerFailure::WorkerPanicked => Self::WorkerPanicked,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixStream},
        path::Path,
        time::Duration,
    };

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use tempfile::TempDir;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use turso_mysql::MySqlDatabaseCatalog;

    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::{
        AccountStoreCheckpoint, AccountStoreCheckpointAuthority, AccountStoreCheckpointRequest,
        AuthMoreData, AuthMoreDataKind, AuthOkPacket, CheckpointAuthorityId, CheckpointPersistence,
        CheckpointReadError, ClientHandshakeResponseConfig, DatabasePrivileges, GlobalPrivileges,
        InitialHandshake, OfflineAccountProvisioner, ProtectedPassword, ResultTerminatorPacket,
        RuntimeConfig, RuntimeLimits, RuntimeTimeouts, RuntimeUnixEndpointCleanup, TextRowPacket,
        TextRowValue, UnixSocketConfig, CACHING_SHA2_PASSWORD_PLUGIN, CLIENT_CONNECT_WITH_DB,
        CLIENT_DEPRECATE_EOF, COMMAND_SEQUENCE_ID, COM_PING, COM_QUERY, COM_QUIT,
        DEFAULT_UTF8MB4_COLLATION, MIN_WRITE_LIMIT, PACKET_HEADER_LEN,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };

    #[test]
    fn reaper_handles_completion_before_registration() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let (sender, receiver) = worker_event_channel(1);
        sender.send(ReaperEvent::Finished(7)).unwrap();
        sender
            .send(ReaperEvent::Started {
                token: 7,
                worker: Box::new(FakeWorker::finished(WorkerExit::Normal)),
            })
            .unwrap();
        sender.send(ReaperEvent::AcceptStopped).unwrap();

        run_reaper(receiver, Arc::clone(&snapshot), || {});

        let snapshot = *snapshot.lock().unwrap();
        assert_eq!(snapshot.started, 1);
        assert_eq!(snapshot.joined, 1);
        assert_eq!(snapshot.remaining, 0);
    }

    #[test]
    fn reaper_classifies_normal_error_and_panic_without_details() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let panic_count = AtomicUsize::new(0);
        let (sender, receiver) = worker_event_channel(3);
        for (token, outcome) in [
            (1, WorkerExit::Normal),
            (2, WorkerExit::ConnectionError),
            (3, WorkerExit::Panicked),
        ] {
            sender
                .send(ReaperEvent::Started {
                    token,
                    worker: Box::new(FakeWorker::finished(outcome)),
                })
                .unwrap();
            sender.send(ReaperEvent::Finished(token)).unwrap();
        }
        sender.send(ReaperEvent::AcceptStopped).unwrap();

        run_reaper(receiver, Arc::clone(&snapshot), || {
            panic_count.fetch_add(1, Ordering::Relaxed);
        });

        let snapshot = *snapshot.lock().unwrap();
        assert_eq!(snapshot.started, 3);
        assert_eq!(snapshot.joined, 3);
        assert_eq!(snapshot.connection_errors, 1);
        assert_eq!(snapshot.panics, 1);
        assert_eq!(snapshot.remaining, 0);
        assert_eq!(panic_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reaper_failure_drains_queued_worker_handles_before_stopping() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let failure_count = AtomicUsize::new(0);
        let (sender, receiver) = worker_event_channel(2);
        sender
            .send(ReaperEvent::Started {
                token: 1,
                worker: Box::new(FakeWorker::finished(WorkerExit::Panicked)),
            })
            .unwrap();
        sender.send(ReaperEvent::Finished(1)).unwrap();
        sender
            .send(ReaperEvent::Started {
                token: 2,
                worker: Box::new(FakeWorker::finished(WorkerExit::Normal)),
            })
            .unwrap();
        sender.send(ReaperEvent::Finished(2)).unwrap();
        sender.send(ReaperEvent::AcceptStopped).unwrap();

        assert!(run_reaper_safely(
            receiver,
            Arc::clone(&snapshot),
            || panic!("synthetic worker-panic callback failure"),
            || {
                failure_count.fetch_add(1, Ordering::Relaxed);
            },
        )
        .is_err());

        let snapshot = *snapshot.lock().unwrap();
        assert_eq!(snapshot.started, 2);
        assert_eq!(snapshot.joined, 2);
        assert_eq!(snapshot.panics, 1);
        assert_eq!(snapshot.remaining, 0);
        assert_eq!(failure_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn completion_notice_does_not_join_a_worker_before_thread_exit() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let finished = Arc::new(AtomicBool::new(false));
        let (inspected, inspection) = mpsc::sync_channel(1);
        let (joined, join) = mpsc::sync_channel(1);
        let (sender, receiver) = worker_event_channel(1);
        sender
            .send(ReaperEvent::Started {
                token: 1,
                worker: Box::new(DelayedFakeWorker {
                    finished: Arc::clone(&finished),
                    outcome: WorkerExit::Normal,
                    inspected,
                    joined,
                }),
            })
            .unwrap();
        sender.send(ReaperEvent::Finished(1)).unwrap();
        let reaper_snapshot = Arc::clone(&snapshot);
        let reaper = thread::spawn(move || run_reaper(receiver, reaper_snapshot, || {}));

        inspection
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(snapshot.lock().unwrap().joined, 0);
        assert!(!reaper.is_finished());

        finished.store(true, Ordering::Release);
        join.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!reaper.is_finished());
        sender.send(ReaperEvent::AcceptStopped).unwrap();
        reaper.join().unwrap();
        assert_eq!(snapshot.lock().unwrap().joined, 1);
    }

    #[test]
    fn accept_loop_can_start_only_once() {
        let control = RuntimeUnixServerControl::new();
        control.begin_run().unwrap();
        control.finish_run();
        assert_eq!(
            control.begin_run(),
            Err(RuntimeUnixServerRunError::AlreadyRun)
        );
    }

    #[test]
    fn only_exhausted_connection_limits_are_client_errors() {
        assert!(client_accept_error(
            &RuntimeUnixListenerError::ConnectionLimit(ConnectionLimitError::ConnectionsExhausted,),
        ));
        assert!(client_accept_error(
            &RuntimeUnixListenerError::ConnectionLimit(ConnectionLimitError::AdmissionsExhausted,),
        ));
        assert!(!client_accept_error(
            &RuntimeUnixListenerError::ConnectionLimit(ConnectionLimitError::Unavailable),
        ));
    }

    #[test]
    fn worker_event_queue_applies_backpressure_at_its_connection_bound() {
        let (sender, _receiver) = worker_event_channel(1);
        sender.send(ReaperEvent::Finished(1)).unwrap();
        sender.send(ReaperEvent::Finished(2)).unwrap();
        sender.send(ReaperEvent::Finished(3)).unwrap();

        assert!(matches!(
            sender.try_send(ReaperEvent::Finished(4)),
            Err(mpsc::TrySendError::Full(ReaperEvent::Finished(4)))
        ));
    }

    #[test]
    fn shutdown_before_run_prevents_start_and_stops_reaper() {
        let control = RuntimeUnixServerControl::new();
        let (sender, receiver) = worker_event_channel(1);
        control.begin_shutdown();
        control.send_accept_stopped(&sender);

        assert_eq!(
            control.begin_run(),
            Err(RuntimeUnixServerRunError::ShuttingDown)
        );
        assert!(matches!(receiver.recv(), Ok(ReaperEvent::AcceptStopped)));
    }

    #[test]
    fn reaper_join_timeout_retains_the_handle_for_retry() {
        let completion = Arc::new(ReaperCompletion::new());
        let worker_completion = Arc::clone(&completion);
        let (release, released) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            worker_completion.finish();
            released.recv().unwrap();
            Ok(())
        });
        let mut reaper = RuntimeUnixReaper {
            handle: Some(handle),
            terminal: None,
        };

        assert_eq!(
            reaper.join_until(
                &completion,
                Instant::now() + std::time::Duration::from_millis(10),
            ),
            RuntimeUnixWorkerReaperShutdown::TimedOut
        );
        assert!(reaper.handle.is_some());

        release.send(()).unwrap();
        assert_eq!(
            reaper.join_until(
                &completion,
                Instant::now() + std::time::Duration::from_secs(1),
            ),
            RuntimeUnixWorkerReaperShutdown::Stopped
        );
        assert_eq!(
            reaper.join_until(&completion, Instant::now()),
            RuntimeUnixWorkerReaperShutdown::Stopped
        );
    }

    #[test]
    fn failed_reaper_status_is_stable_across_shutdown_retries() {
        let completion = Arc::new(ReaperCompletion::new());
        let worker_completion = Arc::clone(&completion);
        let handle = thread::spawn(move || {
            worker_completion.finish();
            panic!("reaper panic details must not enter its status");
        });
        let mut reaper = RuntimeUnixReaper {
            handle: Some(handle),
            terminal: None,
        };

        assert_eq!(
            reaper.join_until(
                &completion,
                Instant::now() + std::time::Duration::from_secs(1),
            ),
            RuntimeUnixWorkerReaperShutdown::Failed
        );
        assert_eq!(
            reaper.join_until(&completion, Instant::now()),
            RuntimeUnixWorkerReaperShutdown::Failed
        );
    }

    struct FakeWorker {
        finished: Arc<AtomicBool>,
        outcome: WorkerExit,
    }

    impl FakeWorker {
        fn finished(outcome: WorkerExit) -> Self {
            Self {
                finished: Arc::new(AtomicBool::new(true)),
                outcome,
            }
        }
    }

    impl ReapWorker for FakeWorker {
        fn is_finished(&self) -> bool {
            self.finished.load(Ordering::Acquire)
        }

        fn join(self: Box<Self>) -> WorkerExit {
            self.outcome
        }
    }

    struct DelayedFakeWorker {
        finished: Arc<AtomicBool>,
        outcome: WorkerExit,
        inspected: mpsc::SyncSender<()>,
        joined: mpsc::SyncSender<()>,
    }

    impl ReapWorker for DelayedFakeWorker {
        fn is_finished(&self) -> bool {
            let _ = self.inspected.try_send(());
            self.finished.load(Ordering::Acquire)
        }

        fn join(self: Box<Self>) -> WorkerExit {
            self.joined.send(()).unwrap();
            self.outcome
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct TestCheckpointReader {
        checkpoint: AccountStoreCheckpoint,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl crate::AccountStoreCheckpointReader for TestCheckpointReader {
        fn request_checkpoint(
            &self,
            _authority: &CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
            Ok(AccountStoreCheckpointRequest::completed(
                Ok(self.checkpoint),
            ))
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
    fn server_runtime() -> (
        RuntimeConfig,
        Arc<dyn crate::AccountStoreCheckpointReader>,
        TempDir,
        TempDir,
        TempDir,
    ) {
        server_runtime_with_shutdown(Duration::from_secs(1))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn server_runtime_with_shutdown(
        shutdown_timeout: Duration,
    ) -> (
        RuntimeConfig,
        Arc<dyn crate::AccountStoreCheckpointReader>,
        TempDir,
        TempDir,
        TempDir,
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
        let grant = account.grant("testdb", DatabasePrivileges::new(true, true, false, false));
        let mut authority = TestAuthority::default();
        let provisioner = OfflineAccountProvisioner::initialize(
            account_root.path(),
            account.into_builder().with_grant(grant),
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
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                shutdown_timeout,
            )
            .unwrap(),
        )
        .unwrap();
        let reader = Arc::new(TestCheckpointReader { checkpoint });
        (config, reader, data_root, account_root, socket_directory)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn packet_codec() -> crate::PacketCodec {
        crate::PacketCodec::new(crate::MAX_COMMAND_PAYLOAD_LENGTH).unwrap()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn connect_client(endpoint: &Path) -> UnixStream {
        let client = UnixStream::connect(endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
        let mut header = [0; PACKET_HEADER_LEN];
        stream.read_exact(&mut header).unwrap();
        let payload_length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        assert!(payload_length <= crate::MAX_COMMAND_PAYLOAD_LENGTH);
        let mut frame = vec![0; PACKET_HEADER_LEN + payload_length];
        frame[..PACKET_HEADER_LEN].copy_from_slice(&header);
        stream.read_exact(&mut frame[PACKET_HEADER_LEN..]).unwrap();
        frame
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn authenticate(client: &mut UnixStream) {
        let codec = packet_codec();
        let handshake = InitialHandshake::decode(codec, &read_frame(client)).unwrap();
        assert_eq!(handshake.sequence_id, 0);
        assert_ne!(handshake.connection_id, 0);

        let response = ClientHandshakeResponseConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                | CLIENT_CONNECT_WITH_DB
                | CLIENT_DEPRECATE_EOF,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "alice",
            vec![0; 32],
            Some("testdb".to_owned()),
            Some(CACHING_SHA2_PASSWORD_PLUGIN.to_owned()),
            None,
        )
        .encode(codec, 1)
        .unwrap();
        client.write_all(&response).unwrap();

        let auth_more = AuthMoreData::decode(codec, &read_frame(client)).unwrap();
        assert_eq!(auth_more.sequence_id, 2);
        assert_eq!(auth_more.kind, AuthMoreDataKind::FullAuthenticationRequired);
        client
            .write_all(&codec.encode(3, b"secret\0").unwrap())
            .unwrap();

        assert_eq!(
            AuthOkPacket::decode(codec, &read_frame(client))
                .unwrap()
                .sequence_id,
            4
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn select_ping_and_quit(client: &mut UnixStream) {
        let codec = packet_codec();
        let mut query = vec![COM_QUERY];
        query.extend_from_slice(b"SELECT 1");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &query).unwrap())
            .unwrap();

        let count = crate::ColumnCountPacket::decode(codec, &read_frame(client)).unwrap();
        assert_eq!(count.sequence_id, 1);
        assert_eq!(count.column_count, 1);
        assert_eq!(
            crate::ColumnDefinitionPacket::decode(codec, &read_frame(client))
                .unwrap()
                .sequence_id,
            2
        );
        let row_frame = read_frame(client);
        let row = TextRowPacket::decode(codec, &row_frame, 1).unwrap();
        assert_eq!(row.sequence_id, 3);
        assert_eq!(row.values, vec![TextRowValue::Bytes(b"1")]);
        assert!(matches!(
            ResultTerminatorPacket::decode(
                codec,
                &read_frame(client),
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                    | CLIENT_CONNECT_WITH_DB
                    | CLIENT_DEPRECATE_EOF,
            )
            .unwrap(),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 4
        ));

        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_PING]).unwrap())
            .unwrap();
        assert_eq!(
            crate::ResponseOkPacket::decode(codec, &read_frame(client))
                .unwrap()
                .sequence_id,
            1
        );
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &[COM_QUIT]).unwrap())
            .unwrap();
        let mut eof = [0; 1];
        assert_eq!(client.read(&mut eof).unwrap(), 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn assert_clean_shutdown(
        server: &RuntimeUnixServer,
        accept_loop: thread::JoinHandle<Result<(), RuntimeUnixServerRunError>>,
        endpoint: &Path,
        workers: usize,
        connection_errors: usize,
    ) {
        let report = server.shutdown();
        assert!(accept_loop.join().unwrap().is_ok());
        assert!(report.drained());
        assert!(report.listener().drained());
        assert_eq!(
            report.listener().endpoint_cleanup(),
            RuntimeUnixEndpointCleanup::Removed
        );
        assert_eq!(report.accept_loop(), RuntimeUnixAcceptLoopShutdown::Stopped);
        assert_eq!(
            report.worker_reaper(),
            RuntimeUnixWorkerReaperShutdown::Stopped
        );
        assert_eq!(report.workers_started(), workers);
        assert_eq!(report.workers_joined(), workers);
        assert_eq!(report.connection_errors(), connection_errors);
        assert_eq!(report.worker_panics(), 0);
        assert_eq!(report.remaining_workers(), 0);
        assert!(!endpoint.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn wait_for_run_start(server: &RuntimeUnixServer) {
        let mut state = server.control.lock();
        while !state.run_active {
            state = server
                .control
                .changed
                .wait(state)
                .expect("test Unix server state must not be poisoned");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn concurrent_server_shutdown_respects_each_callers_deadline() {
        let (config, reader, _data_root, _account_root, _socket_directory) =
            server_runtime_with_shutdown(Duration::from_millis(20));
        let server = Arc::new(RuntimeUnixServer::bind(&config, reader).unwrap());
        let held = server
            .shutdown_gate
            .enter_until(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let (finished, result) = mpsc::sync_channel(1);
        let shutting_down = Arc::clone(&server);
        let caller = thread::spawn(move || {
            finished.send(shutting_down.shutdown()).unwrap();
        });

        let report = result
            .recv_timeout(Duration::from_millis(200))
            .expect("a concurrent shutdown caller must keep its own deadline");
        assert_eq!(
            report.worker_reaper(),
            RuntimeUnixWorkerReaperShutdown::TimedOut
        );
        drop(held);
        caller.join().unwrap();

        assert!(server.shutdown().drained());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn server_runs_full_auth_query_ping_quit_and_cleans_up_endpoint() {
        let (config, reader, _data_root, _account_root, _socket_directory) = server_runtime();
        let endpoint = config.unix_socket().unwrap().socket_path();
        let server = Arc::new(RuntimeUnixServer::bind(&config, reader).unwrap());
        let running_server = Arc::clone(&server);
        let accept_loop = thread::spawn(move || running_server.run());
        wait_for_run_start(&server);

        let mut client = connect_client(&endpoint);
        authenticate(&mut client);
        select_ping_and_quit(&mut client);
        drop(client);

        assert_clean_shutdown(&server, accept_loop, &endpoint, 1, 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn server_continues_after_truncated_client_and_reaps_its_error() {
        let (config, reader, _data_root, _account_root, _socket_directory) = server_runtime();
        let endpoint = config.unix_socket().unwrap().socket_path();
        let server = Arc::new(RuntimeUnixServer::bind(&config, reader).unwrap());
        let running_server = Arc::clone(&server);
        let accept_loop = thread::spawn(move || running_server.run());
        wait_for_run_start(&server);

        let mut truncated = connect_client(&endpoint);
        let _handshake = read_frame(&mut truncated);
        truncated.write_all(&[1, 0, 0, 1]).unwrap();
        drop(truncated);

        let mut client = connect_client(&endpoint);
        authenticate(&mut client);
        select_ping_and_quit(&mut client);
        drop(client);

        assert_clean_shutdown(&server, accept_loop, &endpoint, 2, 1);
    }
}
