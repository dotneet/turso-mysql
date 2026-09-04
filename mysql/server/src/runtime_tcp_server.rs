//! Process-style ownership for the blocking mandatory-TLS TCP runtime.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    net::SocketAddr,
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
    RuntimeTcpConnectionSpawnError, RuntimeTcpConnectionWorker, RuntimeTcpConnectionWorkerError,
    RuntimeTcpListener, RuntimeTcpListenerError, RuntimeTcpShutdownReport,
};

/// A blocking mandatory-TLS TCP server with joinable ownership of every worker.
pub struct RuntimeTcpServer {
    listener: Arc<RuntimeTcpListener>,
    control: Arc<RuntimeTcpServerControl>,
    events: SyncSender<ReaperEvent>,
    reaper: Mutex<RuntimeTcpReaper>,
    reaper_completion: Arc<ReaperCompletion>,
    reaper_snapshot: Arc<Mutex<ReaperSnapshot>>,
    lost_workers: Mutex<Vec<Box<dyn ReapWorker>>>,
    shutdown_gate: ShutdownGate,
}

/// A cloneable, non-blocking request to stop one TCP server.
///
/// Requesting shutdown wakes a blocked accept loop. Waiting for connections
/// and joining workers remain owned by [`RuntimeTcpServer::shutdown`].
#[derive(Clone)]
pub struct RuntimeTcpServerShutdown {
    listener: crate::runtime_tcp_listener::RuntimeTcpListenerShutdown,
    control: Arc<RuntimeTcpServerControl>,
}

impl RuntimeTcpServerShutdown {
    /// Prevents later admission and wakes the blocking accept loop.
    pub fn request_shutdown(&self) {
        self.control.begin_shutdown();
        self.listener.request_shutdown();
    }
}

impl fmt::Debug for RuntimeTcpServerShutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RuntimeTcpServerShutdown { <redacted> }")
    }
}

impl RuntimeTcpServer {
    /// Binds the TCP listener and starts its sole connection-worker reaper.
    pub fn bind(
        config: &RuntimeConfig,
        checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
    ) -> Result<Self, RuntimeTcpServerBindError> {
        let listener = RuntimeTcpListener::bind(config, checkpoint_reader)
            .map_err(RuntimeTcpServerBindError::Listener)?;
        Self::from_listener(listener, config.limits().max_connections())
    }

    #[cfg(test)]
    fn bind_with_tls(
        config: &RuntimeConfig,
        checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
        tls_config: crate::TlsServerConfig,
    ) -> Result<Self, RuntimeTcpServerBindError> {
        let listener = RuntimeTcpListener::bind_with_tls(config, checkpoint_reader, tls_config)
            .map_err(RuntimeTcpServerBindError::Listener)?;
        Self::from_listener(listener, config.limits().max_connections())
    }

    fn from_listener(
        listener: RuntimeTcpListener,
        max_connections: usize,
    ) -> Result<Self, RuntimeTcpServerBindError> {
        let listener = Arc::new(listener);
        let control = Arc::new(RuntimeTcpServerControl::new());
        let reaper_completion = Arc::new(ReaperCompletion::new());
        let reaper_snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let (events, receiver) = worker_event_channel(max_connections);
        let reaper = spawn_reaper(
            receiver,
            Arc::clone(&listener),
            Arc::clone(&control),
            Arc::clone(&reaper_completion),
            Arc::clone(&reaper_snapshot),
        )
        .map_err(|()| RuntimeTcpServerBindError::ReaperUnavailable)?;

        Ok(Self {
            listener,
            control,
            events,
            reaper: Mutex::new(RuntimeTcpReaper {
                handle: Some(reaper),
                terminal: None,
            }),
            reaper_completion,
            reaper_snapshot,
            lost_workers: Mutex::new(Vec::new()),
            shutdown_gate: ShutdownGate::new(),
        })
    }

    /// Runs the accept loop in the calling thread. A server may be run only once.
    pub fn run(&self) -> Result<(), RuntimeTcpServerRunError> {
        self.control.begin_run()?;
        let _run = RunGuard::new(&self.control, &self.events);
        let mut next_worker_token = Some(1_u64);

        loop {
            match self.listener.accept() {
                Ok(stream) => {
                    let token = next_worker_token.ok_or_else(|| {
                        self.fail_closed(RuntimeTcpServerFailure::WorkerTokenExhausted)
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
                        return Err(self.handle_lost_reaper(error.0));
                    }
                }
                Err(RuntimeTcpListenerError::AccountNotReady) => {
                    if !self.listener.wait_until_ready_or_shutdown() {
                        if self.listener.is_shutting_down() {
                            return self.stopped_run_result();
                        }
                        return Err(
                            self.fail_closed(RuntimeTcpServerFailure::AccountReloadUnavailable)
                        );
                    }
                }
                Err(RuntimeTcpListenerError::ShuttingDown) => {
                    return self.stopped_run_result();
                }
                Err(error) if client_accept_error(&error) => {}
                Err(_) => {
                    return Err(self.fail_closed(RuntimeTcpServerFailure::ListenerUnavailable));
                }
            }
        }
    }

    /// Returns a lightweight handle that can request shutdown from another thread.
    pub fn shutdown_handle(&self) -> RuntimeTcpServerShutdown {
        RuntimeTcpServerShutdown {
            listener: self.listener.shutdown_handle(),
            control: Arc::clone(&self.control),
        }
    }

    /// Returns the actual bound address, including an assigned ephemeral port.
    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr()
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
    pub fn shutdown(&self) -> RuntimeTcpServerShutdownReport {
        let deadline = Instant::now() + self.listener.shutdown_timeout();
        self.shutdown_handle().request_shutdown();
        let Some(_shutdown) = self.shutdown_gate.enter_until(deadline) else {
            let listener = self.listener.shutdown_until(deadline);
            let accept_loop = if self.control.run_active() {
                RuntimeTcpAcceptLoopShutdown::TimedOut
            } else {
                RuntimeTcpAcceptLoopShutdown::Stopped
            };
            return self.shutdown_report(
                listener,
                accept_loop,
                RuntimeTcpWorkerReaperShutdown::TimedOut,
            );
        };
        let listener = self.listener.shutdown_until(deadline);
        let accept_loop = if self.control.wait_for_run_until(deadline) {
            let _ = self
                .control
                .send_accept_stopped_until(&self.events, deadline);
            RuntimeTcpAcceptLoopShutdown::Stopped
        } else {
            RuntimeTcpAcceptLoopShutdown::TimedOut
        };
        self.join_lost_workers_until(deadline);
        let worker_reaper = self.join_reaper_until(deadline);
        self.shutdown_report(listener, accept_loop, worker_reaper)
    }

    fn stopped_run_result(&self) -> Result<(), RuntimeTcpServerRunError> {
        match self.control.failure() {
            Some(failure) => Err(failure.into()),
            None => Ok(()),
        }
    }

    fn handle_spawn_error(
        &self,
        error: RuntimeTcpConnectionSpawnError,
    ) -> RuntimeTcpServerRunError {
        match error {
            RuntimeTcpConnectionSpawnError::SpawnUnavailable => {
                self.fail_closed(RuntimeTcpServerFailure::WorkerSpawnUnavailable)
            }
        }
    }

    fn handle_lost_reaper(&self, event: ReaperEvent) -> RuntimeTcpServerRunError {
        let failure = self.fail_closed(RuntimeTcpServerFailure::ReaperUnavailable);
        debug_assert_eq!(failure, RuntimeTcpServerRunError::ReaperUnavailable);
        if let ReaperEvent::Started { worker, .. } = event {
            retain_lost_worker(&self.lost_workers, worker, &self.reaper_snapshot);
        }
        failure
    }

    fn fail_closed(&self, failure: RuntimeTcpServerFailure) -> RuntimeTcpServerRunError {
        self.control.record_failure(failure);
        let _ = self
            .listener
            .shutdown_until(Instant::now() + self.listener.shutdown_timeout());
        failure.into()
    }

    fn join_reaper_until(&self, deadline: Instant) -> RuntimeTcpWorkerReaperShutdown {
        let mut reaper = self
            .reaper
            .lock()
            .expect("TCP server reaper state must not be poisoned");
        reaper.join_until(&self.reaper_completion, deadline)
    }

    fn join_lost_workers_until(&self, deadline: Instant) {
        join_retained_workers_until(&self.lost_workers, &self.reaper_snapshot, deadline);
    }

    fn shutdown_report(
        &self,
        listener: RuntimeTcpShutdownReport,
        accept_loop: RuntimeTcpAcceptLoopShutdown,
        worker_reaper: RuntimeTcpWorkerReaperShutdown,
    ) -> RuntimeTcpServerShutdownReport {
        let snapshot = *self
            .reaper_snapshot
            .lock()
            .expect("TCP server reaper snapshot must not be poisoned");
        RuntimeTcpServerShutdownReport {
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

impl fmt::Debug for RuntimeTcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeTcpServer")
            .field("listener", &self.listener)
            .field("control", &self.control)
            .field("reaper", &"<retained>")
            .finish()
    }
}

impl Drop for RuntimeTcpServer {
    fn drop(&mut self) {
        let _ = self.shutdown();
        self.control.wait_for_run();
        self.control.send_accept_stopped(&self.events);
        if let Some(handle) = self
            .reaper
            .get_mut()
            .expect("TCP server reaper state must not be poisoned")
            .handle
            .take()
        {
            let _ = handle.join();
        }
        join_all_retained_workers(&self.lost_workers, &self.reaper_snapshot);
    }
}

/// Starting a supervised TCP server failed without exposing runtime paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTcpServerBindError {
    /// The underlying listener could not be opened.
    Listener(RuntimeTcpListenerError),
    /// The connection-worker reaper thread could not be started.
    ReaperUnavailable,
}

impl fmt::Display for RuntimeTcpServerBindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listener(error) => write!(f, "TCP server listener failed: {error}"),
            Self::ReaperUnavailable => f.write_str("TCP server worker reaper could not start"),
        }
    }
}

impl Error for RuntimeTcpServerBindError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Listener(error) => Some(error),
            Self::ReaperUnavailable => None,
        }
    }
}

/// A blocking accept loop stopped for a redacted terminal reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTcpServerRunError {
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

impl fmt::Display for RuntimeTcpServerRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AlreadyRun => "TCP server accept loop has already run",
            Self::ShuttingDown => "TCP server is shutting down",
            Self::ListenerUnavailable => "TCP server listener became unavailable",
            Self::AccountReloadUnavailable => "TCP server account reload owner stopped",
            Self::WorkerTokenExhausted => "TCP server worker tokens are exhausted",
            Self::WorkerSpawnUnavailable => "TCP server connection worker could not start",
            Self::ReaperUnavailable => "TCP server worker reaper became unavailable",
            Self::WorkerPanicked => "TCP server connection worker panicked",
        })
    }
}

impl Error for RuntimeTcpServerRunError {}

/// Whether the blocking accept loop stopped within the shared deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeTcpAcceptLoopShutdown {
    /// The accept loop returned or never started.
    Stopped,
    /// The accept loop outlived the shared shutdown deadline.
    TimedOut,
}

/// Whether the connection-worker reaper stopped within the shared deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeTcpWorkerReaperShutdown {
    /// The reaper stopped and its thread was joined.
    Stopped,
    /// The reaper outlived the shared shutdown deadline and remains joinable.
    TimedOut,
    /// The reaper panicked; its payload was discarded.
    Failed,
}

struct RuntimeTcpReaper {
    handle: Option<thread::JoinHandle<Result<(), ()>>>,
    terminal: Option<RuntimeTcpWorkerReaperShutdown>,
}

impl RuntimeTcpReaper {
    fn join_until(
        &mut self,
        completion: &ReaperCompletion,
        deadline: Instant,
    ) -> RuntimeTcpWorkerReaperShutdown {
        if let Some(terminal) = self.terminal {
            return terminal;
        }
        let Some(handle) = self.handle.as_ref() else {
            unreachable!("a TCP reaper without a handle must retain its terminal status")
        };
        if !completion.wait_until(deadline) && !handle.is_finished() {
            return RuntimeTcpWorkerReaperShutdown::TimedOut;
        }
        while !self
            .handle
            .as_ref()
            .expect("a completing TCP reaper must retain its handle")
            .is_finished()
        {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return RuntimeTcpWorkerReaperShutdown::TimedOut;
            };
            thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
        }
        let handle = self
            .handle
            .take()
            .expect("finished TCP server reaper must remain joinable");
        let terminal = match handle.join() {
            Ok(Ok(())) => RuntimeTcpWorkerReaperShutdown::Stopped,
            Ok(Err(())) | Err(_) => RuntimeTcpWorkerReaperShutdown::Failed,
        };
        self.terminal = Some(terminal);
        terminal
    }
}

/// The bounded result of stopping one supervised TCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeTcpServerShutdownReport {
    listener: RuntimeTcpShutdownReport,
    accept_loop: RuntimeTcpAcceptLoopShutdown,
    worker_reaper: RuntimeTcpWorkerReaperShutdown,
    workers_started: usize,
    workers_joined: usize,
    connection_errors: usize,
    worker_panics: usize,
    remaining_workers: usize,
}

impl RuntimeTcpServerShutdownReport {
    /// Returns the listener-owned shutdown result.
    pub const fn listener(&self) -> &RuntimeTcpShutdownReport {
        &self.listener
    }

    /// Returns whether the accept loop stopped within the shared deadline.
    pub const fn accept_loop(&self) -> RuntimeTcpAcceptLoopShutdown {
        self.accept_loop
    }

    /// Returns whether the worker reaper stopped within the shared deadline.
    pub const fn worker_reaper(&self) -> RuntimeTcpWorkerReaperShutdown {
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
            && matches!(self.accept_loop, RuntimeTcpAcceptLoopShutdown::Stopped)
            && matches!(self.worker_reaper, RuntimeTcpWorkerReaperShutdown::Stopped)
            && self.remaining_workers == 0
    }
}

fn client_accept_error(error: &RuntimeTcpListenerError) -> bool {
    matches!(
        error,
        RuntimeTcpListenerError::ConnectionLimit(
            ConnectionLimitError::ConnectionsExhausted | ConnectionLimitError::AdmissionsExhausted
        )
    )
}

fn worker_event_channel(
    max_connections: usize,
) -> (SyncSender<ReaperEvent>, Receiver<ReaperEvent>) {
    let capacity = max_connections
        .checked_mul(2)
        .and_then(|capacity| capacity.checked_add(1))
        .expect("validated TCP connection limit must fit its worker event queue");
    mpsc::sync_channel(capacity)
}

fn spawn_reaper(
    receiver: Receiver<ReaperEvent>,
    listener: Arc<RuntimeTcpListener>,
    control: Arc<RuntimeTcpServerControl>,
    completion: Arc<ReaperCompletion>,
    snapshot: Arc<Mutex<ReaperSnapshot>>,
) -> Result<thread::JoinHandle<Result<(), ()>>, ()> {
    thread::Builder::new()
        .name("turso-mysql-tcp-reaper".to_owned())
        .spawn(move || {
            let _finished = ReaperCompletionGuard::new(completion);
            let panic_control = Arc::clone(&control);
            let panic_listener = Arc::clone(&listener);
            run_reaper_safely(
                receiver,
                snapshot,
                move || {
                    panic_control.record_failure(RuntimeTcpServerFailure::WorkerPanicked);
                    let _ = panic_listener
                        .shutdown_until(Instant::now() + panic_listener.shutdown_timeout());
                },
                move || {
                    control.record_failure(RuntimeTcpServerFailure::ReaperUnavailable);
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
            ReaperReceive::Event(ReaperEvent::AcceptStopped) => {
                accept_stopped = true;
            }
            ReaperReceive::Disconnected => {
                accept_stopped = true;
                workers.finish_all_after_disconnect();
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

impl ReapWorker for RuntimeTcpConnectionWorker {
    fn is_finished(&self) -> bool {
        self.is_finished()
    }

    fn join(self: Box<Self>) -> WorkerExit {
        match (*self).join() {
            Ok(()) => WorkerExit::Normal,
            Err(RuntimeTcpConnectionWorkerError::Connection(_)) => WorkerExit::ConnectionError,
            Err(RuntimeTcpConnectionWorkerError::Panicked) => WorkerExit::Panicked,
        }
    }
}

#[derive(Clone, Copy)]
enum WorkerExit {
    Normal,
    ConnectionError,
    Panicked,
}

fn record_worker_exit(snapshot: &Mutex<ReaperSnapshot>, outcome: WorkerExit) {
    let mut snapshot = snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshot.joined = snapshot.joined.saturating_add(1);
    snapshot.remaining = snapshot.remaining.saturating_sub(1);
    match outcome {
        WorkerExit::Normal => {}
        WorkerExit::ConnectionError => {
            snapshot.connection_errors = snapshot.connection_errors.saturating_add(1);
        }
        WorkerExit::Panicked => snapshot.panics = snapshot.panics.saturating_add(1),
    }
}

fn retain_lost_worker(
    workers: &Mutex<Vec<Box<dyn ReapWorker>>>,
    worker: Box<dyn ReapWorker>,
    snapshot: &Arc<Mutex<ReaperSnapshot>>,
) {
    {
        let mut snapshot = snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.started = snapshot.started.saturating_add(1);
        snapshot.remaining = snapshot.remaining.saturating_add(1);
    }
    workers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(worker);
}

fn join_retained_workers_until(
    workers: &Mutex<Vec<Box<dyn ReapWorker>>>,
    snapshot: &Mutex<ReaperSnapshot>,
    deadline: Instant,
) {
    let mut workers = workers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut index = 0;
    while index < workers.len() {
        if Instant::now() >= deadline {
            return;
        }
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            record_worker_exit(snapshot, worker.join());
        } else {
            index += 1;
        }
    }
}

fn join_all_retained_workers(
    workers: &Mutex<Vec<Box<dyn ReapWorker>>>,
    snapshot: &Mutex<ReaperSnapshot>,
) {
    let workers = std::mem::take(
        &mut *workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for worker in workers {
        record_worker_exit(snapshot, worker.join());
    }
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
        assert!(previous.is_none(), "TCP worker token must never be reused");
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
                    .expect("finished TCP worker must be registered")
                    .is_finished()
            })
            .collect::<Vec<_>>();
        for token in ready {
            self.finished.remove(&token);
            let worker = self
                .workers
                .remove(&token)
                .expect("finished TCP worker must remain registered");
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

    fn finish_all_after_disconnect(&mut self) {
        self.finished.extend(self.workers.keys().copied());
        self.finished_before_start.clear();
    }

    fn take_workers(&mut self) -> Vec<Box<dyn ReapWorker>> {
        std::mem::take(&mut self.workers).into_values().collect()
    }

    fn snapshot(&self) -> std::sync::MutexGuard<'_, ReaperSnapshot> {
        self.snapshot
            .lock()
            .expect("TCP server reaper snapshot must not be poisoned")
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
    {
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
            .expect("TCP server reaper completion must not be poisoned");
        while !*finished {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .changed
                .wait_timeout(finished, remaining)
                .expect("TCP server reaper completion must not be poisoned");
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
            .expect("TCP server reaper completion must not be poisoned") = true;
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
    control: &'a RuntimeTcpServerControl,
    events: &'a SyncSender<ReaperEvent>,
}

impl<'a> RunGuard<'a> {
    fn new(control: &'a RuntimeTcpServerControl, events: &'a SyncSender<ReaperEvent>) -> Self {
        Self { control, events }
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.control.finish_run();
        let _ = self.control.try_send_accept_stopped(self.events);
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
            .expect("TCP server shutdown gate must not be poisoned");
        while *active {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, timeout) = self
                .changed
                .wait_timeout(active, remaining)
                .expect("TCP server shutdown gate must not be poisoned");
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
            .expect("TCP server shutdown gate must not be poisoned") = false;
        self.gate.changed.notify_one();
    }
}

struct RuntimeTcpServerControl {
    state: Mutex<RuntimeTcpServerState>,
    changed: Condvar,
}

impl RuntimeTcpServerControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(RuntimeTcpServerState {
                run_started: false,
                run_active: false,
                shutdown_requested: false,
                accept_stopped_sent: false,
                failure: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn begin_run(&self) -> Result<(), RuntimeTcpServerRunError> {
        let mut state = self.lock();
        if state.run_started {
            return Err(RuntimeTcpServerRunError::AlreadyRun);
        }
        if state.shutdown_requested {
            return Err(RuntimeTcpServerRunError::ShuttingDown);
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

    fn record_failure(&self, failure: RuntimeTcpServerFailure) {
        let mut state = self.lock();
        state.shutdown_requested = true;
        state.failure.get_or_insert(failure);
        self.changed.notify_all();
    }

    fn failure(&self) -> Option<RuntimeTcpServerFailure> {
        self.lock().failure
    }

    fn run_active(&self) -> bool {
        self.lock().run_active
    }

    fn finish_run(&self) {
        let mut state = self.lock();
        assert!(
            state.run_active,
            "only the active TCP accept loop may finish"
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
                .expect("TCP server control must not be poisoned");
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
                .expect("TCP server control must not be poisoned");
        }
    }

    fn send_accept_stopped_until(
        &self,
        events: &SyncSender<ReaperEvent>,
        deadline: Instant,
    ) -> bool {
        loop {
            if self.try_send_accept_stopped(events) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
        }
    }

    fn try_send_accept_stopped(&self, events: &SyncSender<ReaperEvent>) -> bool {
        let mut state = self.lock();
        if state.accept_stopped_sent {
            return true;
        }
        match events.try_send(ReaperEvent::AcceptStopped) {
            Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => {
                state.accept_stopped_sent = true;
                true
            }
            Err(mpsc::TrySendError::Full(_)) => false,
        }
    }

    fn send_accept_stopped(&self, events: &SyncSender<ReaperEvent>) {
        while !self.try_send_accept_stopped(events) {
            thread::yield_now();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeTcpServerState> {
        self.state
            .lock()
            .expect("TCP server control must not be poisoned")
    }
}

impl fmt::Debug for RuntimeTcpServerControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("RuntimeTcpServerControl")
            .field("run_started", &state.run_started)
            .field("run_active", &state.run_active)
            .field("shutdown_requested", &state.shutdown_requested)
            .field("failure", &state.failure)
            .finish()
    }
}

struct RuntimeTcpServerState {
    run_started: bool,
    run_active: bool,
    shutdown_requested: bool,
    accept_stopped_sent: bool,
    failure: Option<RuntimeTcpServerFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeTcpServerFailure {
    ListenerUnavailable,
    AccountReloadUnavailable,
    WorkerTokenExhausted,
    WorkerSpawnUnavailable,
    ReaperUnavailable,
    WorkerPanicked,
}

impl From<RuntimeTcpServerFailure> for RuntimeTcpServerRunError {
    fn from(failure: RuntimeTcpServerFailure) -> Self {
        match failure {
            RuntimeTcpServerFailure::ListenerUnavailable => Self::ListenerUnavailable,
            RuntimeTcpServerFailure::AccountReloadUnavailable => Self::AccountReloadUnavailable,
            RuntimeTcpServerFailure::WorkerTokenExhausted => Self::WorkerTokenExhausted,
            RuntimeTcpServerFailure::WorkerSpawnUnavailable => Self::WorkerSpawnUnavailable,
            RuntimeTcpServerFailure::ReaperUnavailable => Self::ReaperUnavailable,
            RuntimeTcpServerFailure::WorkerPanicked => Self::WorkerPanicked,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Read,
        net::TcpStream,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };

    use tempfile::TempDir;
    use turso_mysql::MySqlDatabaseCatalog;

    use super::*;
    use crate::{
        AccountStoreCheckpoint, AccountStoreCheckpointAuthority, AccountStoreCheckpointRequest,
        CheckpointAuthorityId, CheckpointPersistence, CheckpointReadError, DatabasePrivileges,
        GlobalPrivileges, OfflineAccountProvisioner, ProtectedPassword, RuntimeLimits,
        RuntimeTimeouts, TcpConfig, TlsConfig, MIN_WRITE_LIMIT,
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

        wait_for_joined_workers(&snapshot, 2);
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

        inspection.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(snapshot.lock().unwrap().joined, 0);
        assert!(!reaper.is_finished());

        finished.store(true, Ordering::Release);
        join.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!reaper.is_finished());
        sender.send(ReaperEvent::AcceptStopped).unwrap();
        reaper.join().unwrap();
        assert_eq!(snapshot.lock().unwrap().joined, 1);
    }

    #[test]
    fn disconnected_channel_still_reaps_every_registered_worker() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let (sender, receiver) = worker_event_channel(1);
        sender
            .send(ReaperEvent::Started {
                token: 1,
                worker: Box::new(FakeWorker::finished(WorkerExit::ConnectionError)),
            })
            .unwrap();
        drop(sender);

        run_reaper(receiver, Arc::clone(&snapshot), || {});

        let snapshot = *snapshot.lock().unwrap();
        assert_eq!(snapshot.started, 1);
        assert_eq!(snapshot.joined, 1);
        assert_eq!(snapshot.connection_errors, 1);
        assert_eq!(snapshot.remaining, 0);
    }

    #[test]
    fn lost_reaper_channel_retains_worker_for_bounded_shutdown_retry() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let retained = Mutex::new(Vec::new());
        let (sender, receiver) = worker_event_channel(1);
        drop(receiver);
        let finished = Arc::new(AtomicBool::new(false));
        let (inspected, inspection) = mpsc::sync_channel(1);
        let (joined, join) = mpsc::sync_channel(1);
        let event = sender
            .send(ReaperEvent::Started {
                token: 1,
                worker: Box::new(DelayedFakeWorker {
                    finished: Arc::clone(&finished),
                    outcome: WorkerExit::ConnectionError,
                    inspected,
                    joined,
                }),
            })
            .unwrap_err()
            .0;
        let ReaperEvent::Started { worker, .. } = event else {
            unreachable!("the disconnected reaper must return the unsent worker")
        };

        retain_lost_worker(&retained, worker, &snapshot);
        join_retained_workers_until(
            &retained,
            &snapshot,
            Instant::now() + Duration::from_millis(10),
        );
        inspection.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(retained.lock().unwrap().len(), 1);
        assert!(join.try_recv().is_err());
        assert_eq!(snapshot.lock().unwrap().remaining, 1);

        finished.store(true, Ordering::Release);
        join_retained_workers_until(
            &retained,
            &snapshot,
            Instant::now() + Duration::from_secs(1),
        );
        join.recv_timeout(Duration::from_secs(1)).unwrap();
        let snapshot = *snapshot.lock().unwrap();
        assert_eq!(snapshot.started, 1);
        assert_eq!(snapshot.joined, 1);
        assert_eq!(snapshot.connection_errors, 1);
        assert_eq!(snapshot.remaining, 0);
    }

    #[test]
    fn slow_worker_keeps_reaper_join_bounded_and_retryable() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot::default()));
        let completion = Arc::new(ReaperCompletion::new());
        let worker_completion = Arc::clone(&completion);
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
        sender.send(ReaperEvent::AcceptStopped).unwrap();
        let reaper_snapshot = Arc::clone(&snapshot);
        let handle = thread::spawn(move || {
            let _completion = ReaperCompletionGuard::new(worker_completion);
            run_reaper(receiver, reaper_snapshot, || {});
            Ok(())
        });
        let mut reaper = RuntimeTcpReaper {
            handle: Some(handle),
            terminal: None,
        };

        inspection.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            reaper.join_until(&completion, Instant::now() + Duration::from_millis(10)),
            RuntimeTcpWorkerReaperShutdown::TimedOut
        );
        assert!(reaper.handle.is_some());

        finished.store(true, Ordering::Release);
        join.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            reaper.join_until(&completion, Instant::now() + Duration::from_secs(1)),
            RuntimeTcpWorkerReaperShutdown::Stopped
        );
        assert_eq!(snapshot.lock().unwrap().remaining, 0);
    }

    #[test]
    fn accept_loop_can_start_only_once() {
        let control = RuntimeTcpServerControl::new();
        control.begin_run().unwrap();
        control.finish_run();
        assert_eq!(
            control.begin_run(),
            Err(RuntimeTcpServerRunError::AlreadyRun)
        );
    }

    #[test]
    fn only_exhausted_connection_limits_are_client_errors() {
        assert!(client_accept_error(
            &RuntimeTcpListenerError::ConnectionLimit(ConnectionLimitError::ConnectionsExhausted,)
        ));
        assert!(client_accept_error(
            &RuntimeTcpListenerError::ConnectionLimit(ConnectionLimitError::AdmissionsExhausted,)
        ));
        assert!(!client_accept_error(
            &RuntimeTcpListenerError::ConnectionLimit(ConnectionLimitError::Unavailable)
        ));
        assert!(!client_accept_error(
            &RuntimeTcpListenerError::TransportConfiguration
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
    fn accept_stop_notification_respects_deadline_and_remains_retryable() {
        let control = RuntimeTcpServerControl::new();
        let (sender, receiver) = worker_event_channel(1);
        sender.send(ReaperEvent::Finished(1)).unwrap();
        sender.send(ReaperEvent::Finished(2)).unwrap();
        sender.send(ReaperEvent::Finished(3)).unwrap();

        assert!(!control
            .send_accept_stopped_until(&sender, Instant::now() + Duration::from_millis(10),));
        assert!(matches!(receiver.recv(), Ok(ReaperEvent::Finished(1))));
        assert!(
            control.send_accept_stopped_until(&sender, Instant::now() + Duration::from_secs(1),)
        );
    }

    #[test]
    fn shutdown_before_run_prevents_start_and_stops_reaper() {
        let control = RuntimeTcpServerControl::new();
        let (sender, receiver) = worker_event_channel(1);
        control.begin_shutdown();
        assert!(control.try_send_accept_stopped(&sender));

        assert_eq!(
            control.begin_run(),
            Err(RuntimeTcpServerRunError::ShuttingDown)
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
        let mut reaper = RuntimeTcpReaper {
            handle: Some(handle),
            terminal: None,
        };

        assert_eq!(
            reaper.join_until(&completion, Instant::now() + Duration::from_millis(10)),
            RuntimeTcpWorkerReaperShutdown::TimedOut
        );
        assert!(reaper.handle.is_some());

        release.send(()).unwrap();
        assert_eq!(
            reaper.join_until(&completion, Instant::now() + Duration::from_secs(1)),
            RuntimeTcpWorkerReaperShutdown::Stopped
        );
        assert_eq!(
            reaper.join_until(&completion, Instant::now()),
            RuntimeTcpWorkerReaperShutdown::Stopped
        );
    }

    #[test]
    fn drop_join_path_waits_for_a_retained_worker() {
        let snapshot = Arc::new(Mutex::new(ReaperSnapshot {
            started: 1,
            remaining: 1,
            ..ReaperSnapshot::default()
        }));
        let (release, released) = mpsc::sync_channel(1);
        let (join_started, join_start) = mpsc::sync_channel(1);
        let retained = Arc::new(Mutex::new(vec![Box::new(BlockingFakeWorker {
            release: released,
            join_started,
            outcome: WorkerExit::ConnectionError,
        }) as Box<dyn ReapWorker>]));
        let drop_snapshot = Arc::clone(&snapshot);
        let drop_retained = Arc::clone(&retained);
        let (drop_done, drop_returned) = mpsc::sync_channel(1);
        let drop_join = thread::spawn(move || {
            join_all_retained_workers(&drop_retained, &drop_snapshot);
            drop_done.send(()).unwrap();
        });

        join_start.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(drop_returned.try_recv().is_err());
        release.send(()).unwrap();
        drop_returned.recv_timeout(Duration::from_secs(1)).unwrap();
        drop_join.join().unwrap();
        let snapshot = *snapshot.lock().unwrap();
        assert_eq!(snapshot.joined, 1);
        assert_eq!(snapshot.connection_errors, 1);
        assert_eq!(snapshot.remaining, 0);
    }

    #[test]
    fn failed_reaper_status_is_stable_across_shutdown_retries() {
        let completion = Arc::new(ReaperCompletion::new());
        let worker_completion = Arc::clone(&completion);
        let handle = thread::spawn(move || {
            worker_completion.finish();
            panic!("reaper panic details must not enter its status");
        });
        let mut reaper = RuntimeTcpReaper {
            handle: Some(handle),
            terminal: None,
        };

        assert_eq!(
            reaper.join_until(&completion, Instant::now() + Duration::from_secs(1)),
            RuntimeTcpWorkerReaperShutdown::Failed
        );
        assert_eq!(
            reaper.join_until(&completion, Instant::now()),
            RuntimeTcpWorkerReaperShutdown::Failed
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

    struct BlockingFakeWorker {
        release: mpsc::Receiver<()>,
        join_started: mpsc::SyncSender<()>,
        outcome: WorkerExit,
    }

    impl ReapWorker for BlockingFakeWorker {
        fn is_finished(&self) -> bool {
            false
        }

        fn join(self: Box<Self>) -> WorkerExit {
            self.join_started.send(()).unwrap();
            self.release.recv().unwrap();
            self.outcome
        }
    }

    struct TestCheckpointReader {
        checkpoint: AccountStoreCheckpoint,
    }

    impl AccountStoreCheckpointReader for TestCheckpointReader {
        fn request_checkpoint(
            &self,
            _authority: &CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
            Ok(AccountStoreCheckpointRequest::completed(
                Ok(self.checkpoint),
            ))
        }
    }

    #[derive(Default)]
    struct TestAuthority {
        checkpoint: Option<AccountStoreCheckpoint>,
    }

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

    struct ServerRuntime {
        server: Arc<RuntimeTcpServer>,
        _data_root: TempDir,
        _account_root: TempDir,
    }

    fn server_runtime() -> ServerRuntime {
        server_runtime_with_shutdown(Duration::from_secs(2))
    }

    fn server_runtime_with_shutdown(shutdown_timeout: Duration) -> ServerRuntime {
        let data_root = private_directory();
        let account_root = private_directory();
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

        let tls = TlsConfig::new(
            data_root.path().join("unused-certificate.pem"),
            data_root.path().join("unused-private-key.pem"),
        )
        .unwrap();
        let config = RuntimeConfig::new(
            Some(TcpConfig::new("127.0.0.1:0".parse().unwrap(), tls)),
            None,
            data_root.path().canonicalize().unwrap(),
            account_root.path().canonicalize().unwrap(),
            CheckpointAuthorityId::new("runtime-checkpoints").unwrap(),
            Duration::from_secs(60),
            RuntimeLimits::new(4, 4, MIN_WRITE_LIMIT, 16).unwrap(),
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
        let server = RuntimeTcpServer::bind_with_tls(
            &config,
            reader,
            crate::runtime_tls::test_server_config(),
        )
        .unwrap();
        ServerRuntime {
            server: Arc::new(server),
            _data_root: data_root,
            _account_root: account_root,
        }
    }

    fn private_directory() -> TempDir {
        let target =
            fs::canonicalize(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
                .unwrap();
        let directory = tempfile::tempdir_in(target).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn wait_for_joined_workers(snapshot: &Mutex<ReaperSnapshot>, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if snapshot.lock().unwrap().joined == expected {
                return;
            }
            assert!(Instant::now() < deadline, "late worker joins timed out");
            thread::yield_now();
        }
    }

    fn wait_for_run_start(server: &RuntimeTcpServer) {
        let mut state = server.control.lock();
        while !state.run_active {
            state = server
                .control
                .changed
                .wait(state)
                .expect("test TCP server state must not be poisoned");
        }
    }

    fn connect_and_close_after_greeting(server: &RuntimeTcpServer) {
        let mut client = TcpStream::connect(server.local_addr()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = [0; crate::PACKET_HEADER_LEN];
        client.read_exact(&mut header).unwrap();
        let payload_length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        assert!(payload_length <= crate::MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH);
        let mut payload = vec![0; payload_length];
        client.read_exact(&mut payload).unwrap();
    }

    #[test]
    fn shutdown_handle_wakes_a_blocked_accept_loop() {
        let runtime = server_runtime();
        let shutdown = runtime.server.shutdown_handle();
        let running_server = Arc::clone(&runtime.server);
        let accept_loop = thread::spawn(move || running_server.run());
        wait_for_run_start(&runtime.server);

        shutdown.request_shutdown();
        assert!(accept_loop.join().unwrap().is_ok());

        let report = runtime.server.shutdown();
        assert!(report.drained());
        assert_eq!(report.workers_started(), 0);
        assert_eq!(report.workers_joined(), 0);
    }

    #[test]
    fn concurrent_server_shutdown_respects_each_callers_deadline() {
        let runtime = server_runtime_with_shutdown(Duration::from_millis(20));
        let held = runtime
            .server
            .shutdown_gate
            .enter_until(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let (finished, result) = mpsc::sync_channel(1);
        let shutting_down = Arc::clone(&runtime.server);
        let caller = thread::spawn(move || {
            finished.send(shutting_down.shutdown()).unwrap();
        });

        let report = result
            .recv_timeout(Duration::from_millis(200))
            .expect("a concurrent TCP shutdown caller must keep its own deadline");
        assert_eq!(
            report.worker_reaper(),
            RuntimeTcpWorkerReaperShutdown::TimedOut
        );
        drop(held);
        caller.join().unwrap();

        assert!(runtime.server.shutdown().drained());
    }

    #[test]
    fn repeated_shutdown_requests_before_run_prevent_start() {
        let runtime = server_runtime();
        let shutdown = runtime.server.shutdown_handle();

        shutdown.request_shutdown();
        shutdown.request_shutdown();

        assert_eq!(
            runtime.server.run(),
            Err(RuntimeTcpServerRunError::ShuttingDown)
        );
        assert!(runtime.server.shutdown().drained());
    }

    #[test]
    fn server_continues_after_clients_close_and_reaps_every_worker() {
        let runtime = server_runtime();
        let running_server = Arc::clone(&runtime.server);
        let accept_loop = thread::spawn(move || running_server.run());
        wait_for_run_start(&runtime.server);

        connect_and_close_after_greeting(&runtime.server);
        connect_and_close_after_greeting(&runtime.server);

        let report = runtime.server.shutdown();
        assert!(accept_loop.join().unwrap().is_ok());
        assert!(report.drained());
        assert!(report.listener().drained());
        assert_eq!(report.accept_loop(), RuntimeTcpAcceptLoopShutdown::Stopped);
        assert_eq!(
            report.worker_reaper(),
            RuntimeTcpWorkerReaperShutdown::Stopped
        );
        assert_eq!(report.workers_started(), 2);
        assert_eq!(report.workers_joined(), 2);
        assert_eq!(report.connection_errors(), 2);
        assert_eq!(report.worker_panics(), 0);
        assert_eq!(report.remaining_workers(), 0);
    }
}
