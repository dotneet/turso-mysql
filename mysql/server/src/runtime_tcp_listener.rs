//! Blocking TCP listener ownership for the future TLS protocol runtime.
//!
//! This boundary owns admission limits, accepted-stream lifetime, and bounded
//! shutdown. A later runtime owner supplies the account provider and invokes
//! [`RuntimeTcpConnection`] from the joinable worker callback.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Instant,
};

use crate::runtime_tcp_connection::{RuntimeTcpConnectionError, RuntimeTcpConnectionLimits};
use crate::{ConnectionLimitError, RuntimeLimits, RuntimeTimeouts, TlsServerConfig};

/// A blocking TCP listener that retains validated TLS material and every
/// accepted stream until its worker finishes.
pub(crate) struct RuntimeTcpListener {
    control: Arc<RuntimeTcpListenerControl>,
    wake_reader: UnixStream,
    tls_config: Arc<TlsServerConfig>,
    local_addr: SocketAddr,
    limits: RuntimeLimits,
    timeouts: RuntimeTimeouts,
}

/// A cloneable, non-blocking request to stop one TCP listener.
#[derive(Clone)]
pub(crate) struct RuntimeTcpListenerShutdown {
    control: Arc<RuntimeTcpListenerControl>,
}

impl RuntimeTcpListenerShutdown {
    /// Prevents later admission and wakes a blocked accept call.
    pub(crate) fn request_shutdown(&self) {
        self.control.request_shutdown();
    }
}

impl fmt::Debug for RuntimeTcpListenerShutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RuntimeTcpListenerShutdown { <redacted> }")
    }
}

impl RuntimeTcpListener {
    /// Binds one TCP endpoint after the TLS configuration has been validated.
    pub(crate) fn bind(
        bind_addr: SocketAddr,
        tls_config: TlsServerConfig,
        limits: RuntimeLimits,
        timeouts: RuntimeTimeouts,
    ) -> Result<Self, RuntimeTcpListenerError> {
        let listener =
            TcpListener::bind(bind_addr).map_err(|_| RuntimeTcpListenerError::BindUnavailable)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        let local_addr = listener
            .local_addr()
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        let (wake_reader, wake_writer) =
            UnixStream::pair().map_err(|_| RuntimeTcpListenerError::WakeUnavailable)?;
        let control = Arc::new(RuntimeTcpListenerControl::new(
            listener,
            wake_writer,
            limits,
        ));
        Ok(Self {
            control,
            wake_reader,
            tls_config: Arc::new(tls_config),
            local_addr,
            limits,
            timeouts,
        })
    }

    /// Returns the actual bound address, including an assigned ephemeral port.
    pub(crate) const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns whether shutdown has begun and no new stream can be returned.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.control.is_shutting_down()
    }

    /// Returns a lightweight handle that can request shutdown from another
    /// thread.
    pub(crate) fn shutdown_handle(&self) -> RuntimeTcpListenerShutdown {
        RuntimeTcpListenerShutdown {
            control: Arc::clone(&self.control),
        }
    }

    /// Blocks for one accepted TCP stream or a lifecycle error.
    pub(crate) fn accept(&self) -> Result<AcceptedTcpStream, RuntimeTcpListenerError> {
        let accept_waiter = self.control.start_accept()?;
        if self.is_shutting_down() {
            return Err(RuntimeTcpListenerError::ShuttingDown);
        }
        let stream = loop {
            match wait_for_listener_or_shutdown(
                &accept_waiter.listener,
                &self.wake_reader,
                &self.control,
            )? {
                ListenerWait::ShuttingDown => return Err(RuntimeTcpListenerError::ShuttingDown),
                ListenerWait::ListenerReady => {}
            }
            match accept_waiter.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(_) => return Err(RuntimeTcpListenerError::AcceptUnavailable),
            }
        };
        if self.is_shutting_down() {
            return Err(RuntimeTcpListenerError::ShuttingDown);
        }
        let permits = ConnectionPermits::acquire(&self.control.permits)
            .map_err(RuntimeTcpListenerError::ConnectionLimit)?;
        stream
            .set_nonblocking(false)
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        stream
            .set_read_timeout(Some(self.timeouts.authentication()))
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        stream
            .set_write_timeout(Some(self.timeouts.write()))
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        let registration = self.control.register_connection(&stream)?;
        let authentication_deadline = Instant::now() + self.timeouts.authentication();
        drop(accept_waiter);
        Ok(AcceptedTcpStream {
            stream,
            lease: ConnectionLease {
                permits,
                registration,
            },
            tls_config: Arc::clone(&self.tls_config),
            authentication_deadline,
            limits: RuntimeTcpConnectionLimits {
                max_queued_bytes: self.limits.max_write_bytes(),
                max_queued_frames: self.limits.max_write_frames(),
            },
            timeouts: self.timeouts,
        })
    }

    /// Gives an accepted stream to one joinable protocol worker.
    pub(crate) fn spawn_protocol<F>(
        &self,
        stream: AcceptedTcpStream,
        run: F,
    ) -> Result<RuntimeTcpConnectionWorker, RuntimeTcpConnectionSpawnError>
    where
        F: FnOnce(AcceptedTcpStream) -> Result<(), RuntimeTcpConnectionError> + Send + 'static,
    {
        let connection_id = stream.connection_id();
        let handle = thread::Builder::new()
            .name(format!("turso-mysql-tcp-{connection_id}"))
            .spawn(move || run(stream))
            .map_err(|_| RuntimeTcpConnectionSpawnError::SpawnUnavailable)?;
        Ok(RuntimeTcpConnectionWorker {
            connection_id,
            handle,
        })
    }

    /// Stops acceptance, signals active streams, and reports bounded drain.
    pub(crate) fn shutdown(&self) -> RuntimeTcpShutdownReport {
        self.shutdown_until(Instant::now() + self.timeouts.shutdown())
    }

    /// Repeats shutdown with a caller-owned absolute deadline.
    pub(crate) fn shutdown_until(&self, deadline: Instant) -> RuntimeTcpShutdownReport {
        match self.control.begin_shutdown() {
            ShutdownStart::Owner(owner) => {
                drop(owner.listener);
                let report = self.control.wait_for_drain(
                    deadline,
                    owner.connections_at_start,
                    owner.admissions_at_start,
                    owner.streams_signalled,
                );
                self.control.finish_shutdown(report);
                report
            }
            ShutdownStart::Wait => self
                .control
                .wait_for_shutdown_until(deadline)
                .unwrap_or_else(|| self.control.shutdown_progress_report()),
            ShutdownStart::Finished(report) => report,
        }
    }
}

impl fmt::Debug for RuntimeTcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeTcpListener")
            .field("control", &self.control)
            .field("wake_reader", &"<redacted>")
            .field("tls_config", &"<redacted>")
            .field("local_addr", &self.local_addr)
            .field("limits", &self.limits)
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl Drop for RuntimeTcpListener {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// One accepted TCP stream with active connection and admission permits.
pub(crate) struct AcceptedTcpStream {
    stream: TcpStream,
    lease: ConnectionLease,
    tls_config: Arc<TlsServerConfig>,
    authentication_deadline: Instant,
    limits: RuntimeTcpConnectionLimits,
    timeouts: RuntimeTimeouts,
}

impl AcceptedTcpStream {
    /// Returns the nonzero protocol connection identifier.
    pub(crate) fn connection_id(&self) -> u64 {
        self.lease.connection_id()
    }

    /// Starts protocol work if shutdown has not begun.
    pub(crate) fn begin_protocol_work(&self) -> Result<(), RuntimeTcpListenerError> {
        self.lease.begin_protocol_work()
    }

    /// Marks authentication complete and releases the admission permit.
    pub(crate) fn complete_admission(&mut self) -> Result<(), RuntimeTcpListenerError> {
        self.lease.complete_admission()
    }

    /// Returns the fixed deadline for completing authentication.
    pub(crate) const fn authentication_deadline(&self) -> Instant {
        self.authentication_deadline
    }

    /// Returns the validated, immutable TLS configuration retained by the listener.
    pub(crate) fn tls_config(&self) -> Arc<TlsServerConfig> {
        Arc::clone(&self.tls_config)
    }

    /// Returns the per-connection bounded response limits.
    pub(crate) const fn limits(&self) -> RuntimeTcpConnectionLimits {
        self.limits
    }

    /// Returns the selected lifecycle timeouts.
    pub(crate) const fn timeouts(&self) -> RuntimeTimeouts {
        self.timeouts
    }

    /// Clones the socket while this accepted-stream lease remains active.
    /// Shutdown closes the registered descriptor and therefore also unblocks
    /// the protocol owner's clone.
    pub(crate) fn try_clone_stream(&self) -> io::Result<TcpStream> {
        self.stream.try_clone()
    }
}

impl fmt::Debug for AcceptedTcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptedTcpStream")
            .field("stream", &"<redacted>")
            .field("connection_id", &"<redacted>")
            .field("admission_complete", &self.lease.admission_complete())
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl Read for AcceptedTcpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buffer)
    }
}

impl Write for AcceptedTcpStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// One joinable protocol worker for an accepted TCP stream.
#[must_use = "a TCP protocol worker must be joined so connection failure is observed"]
pub(crate) struct RuntimeTcpConnectionWorker {
    connection_id: u64,
    handle: thread::JoinHandle<Result<(), RuntimeTcpConnectionError>>,
}

impl RuntimeTcpConnectionWorker {
    /// Returns the nonzero ID assigned to this connection.
    pub(crate) const fn connection_id(&self) -> u64 {
        self.connection_id
    }

    /// Returns whether the worker has stopped.
    pub(crate) fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Waits for the worker and redacts panic payloads.
    pub(crate) fn join(self) -> Result<(), RuntimeTcpConnectionWorkerError> {
        match self.handle.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(RuntimeTcpConnectionWorkerError::Connection(error)),
            Err(_) => Err(RuntimeTcpConnectionWorkerError::Panicked),
        }
    }
}

impl fmt::Debug for RuntimeTcpConnectionWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeTcpConnectionWorker")
            .field("connection_id", &"<redacted>")
            .field("handle", &"<redacted>")
            .finish()
    }
}

/// A TCP worker ended without exposing transport or panic details.
#[derive(Debug)]
pub(crate) enum RuntimeTcpConnectionWorkerError {
    /// The protocol owner returned a typed failure.
    Connection(RuntimeTcpConnectionError),
    /// The protocol owner panicked.
    Panicked,
}

impl fmt::Display for RuntimeTcpConnectionWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(error) => write!(f, "TCP protocol worker failed: {error}"),
            Self::Panicked => f.write_str("TCP protocol worker panicked"),
        }
    }
}

impl Error for RuntimeTcpConnectionWorkerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Connection(error) => Some(error),
            Self::Panicked => None,
        }
    }
}

/// Accepting or spawning a TCP protocol owner failed without exposing addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeTcpConnectionSpawnError {
    /// The worker thread could not be started.
    SpawnUnavailable,
}

impl fmt::Display for RuntimeTcpConnectionSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TCP protocol worker could not start")
    }
}

impl Error for RuntimeTcpConnectionSpawnError {}

/// A TCP listener operation failed without exposing endpoint or peer details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeTcpListenerError {
    /// The TCP endpoint could not be bound.
    BindUnavailable,
    /// A blocking accept operation failed.
    AcceptUnavailable,
    /// The listener wake channel could not be created.
    WakeUnavailable,
    /// A stream timeout or blocking mode could not be configured.
    TransportConfiguration,
    /// Shutdown began before an accepted stream could be returned.
    ShuttingDown,
    /// The configured connection or authentication cap was reached.
    ConnectionLimit(ConnectionLimitError),
}

impl fmt::Display for RuntimeTcpListenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BindUnavailable => f.write_str("TCP listener bind failed"),
            Self::AcceptUnavailable => f.write_str("TCP listener accept failed"),
            Self::WakeUnavailable => f.write_str("TCP listener wake channel is unavailable"),
            Self::TransportConfiguration => f.write_str("TCP stream configuration failed"),
            Self::ShuttingDown => f.write_str("TCP listener is shutting down"),
            Self::ConnectionLimit(error) => write!(f, "TCP connection rejected: {error}"),
        }
    }
}

impl Error for RuntimeTcpListenerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConnectionLimit(error) => Some(error),
            Self::BindUnavailable
            | Self::AcceptUnavailable
            | Self::WakeUnavailable
            | Self::TransportConfiguration
            | Self::ShuttingDown => None,
        }
    }
}

/// The bounded result of one TCP-listener shutdown attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeTcpShutdownReport {
    connections_at_start: usize,
    admissions_at_start: usize,
    streams_signalled: usize,
    remaining_connections: usize,
    remaining_admissions: usize,
    remaining_accept_waiters: usize,
}

impl RuntimeTcpShutdownReport {
    /// Returns the number of active streams when shutdown began.
    pub(crate) const fn connections_at_start(&self) -> usize {
        self.connections_at_start
    }

    /// Returns the number of authenticating streams when shutdown began.
    pub(crate) const fn admissions_at_start(&self) -> usize {
        self.admissions_at_start
    }

    /// Returns the number of streams signalled for shutdown.
    pub(crate) const fn streams_signalled(&self) -> usize {
        self.streams_signalled
    }

    /// Returns the number of streams that remain owned.
    pub(crate) const fn remaining_connections(&self) -> usize {
        self.remaining_connections
    }

    /// Returns the number of authentication permits that remain active.
    pub(crate) const fn remaining_admissions(&self) -> usize {
        self.remaining_admissions
    }

    /// Returns the number of blocked accept calls that remain.
    pub(crate) const fn remaining_accept_waiters(&self) -> usize {
        self.remaining_accept_waiters
    }

    /// Returns whether acceptance and every registered stream drained.
    pub(crate) const fn drained(&self) -> bool {
        self.remaining_connections == 0
            && self.remaining_admissions == 0
            && self.remaining_accept_waiters == 0
    }
}

struct RuntimeTcpListenerControl {
    state: Mutex<RuntimeTcpListenerState>,
    changed: Condvar,
    permits: Arc<Mutex<PermitState>>,
    wake_writer: UnixStream,
}

impl RuntimeTcpListenerControl {
    fn new(listener: TcpListener, wake_writer: UnixStream, limits: RuntimeLimits) -> Self {
        Self {
            state: Mutex::new(RuntimeTcpListenerState {
                lifecycle: RuntimeTcpListenerLifecycle::Accepting,
                listener: Some(listener),
                accept_waiters: 0,
                next_connection_id: 1,
                connections: BTreeMap::new(),
                shutdown_counts: None,
                shutdown_owner: None,
            }),
            changed: Condvar::new(),
            permits: Arc::new(Mutex::new(PermitState::new(limits))),
            wake_writer,
        }
    }

    fn is_shutting_down(&self) -> bool {
        !matches!(
            self.lock().lifecycle,
            RuntimeTcpListenerLifecycle::Accepting
        )
    }

    fn start_accept(self: &Arc<Self>) -> Result<TcpAcceptWaiter, RuntimeTcpListenerError> {
        let mut state = self.lock();
        if !matches!(state.lifecycle, RuntimeTcpListenerLifecycle::Accepting) {
            return Err(RuntimeTcpListenerError::ShuttingDown);
        }
        let listener = state
            .listener
            .as_ref()
            .ok_or(RuntimeTcpListenerError::ShuttingDown)?
            .try_clone()
            .map_err(|_| RuntimeTcpListenerError::AcceptUnavailable)?;
        state.accept_waiters += 1;
        self.changed.notify_all();
        Ok(TcpAcceptWaiter {
            control: Arc::clone(self),
            listener,
        })
    }

    fn register_connection(
        self: &Arc<Self>,
        stream: &TcpStream,
    ) -> Result<ConnectionRegistration, RuntimeTcpListenerError> {
        let duplicate = stream
            .try_clone()
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        let mut state = self.lock();
        if !matches!(state.lifecycle, RuntimeTcpListenerLifecycle::Accepting) {
            return Err(RuntimeTcpListenerError::ShuttingDown);
        }
        let id = allocate_connection_id(&mut state);
        let previous = state.connections.insert(
            id,
            RegisteredConnection {
                stream: duplicate,
                admission_active: true,
            },
        );
        assert!(previous.is_none(), "fresh TCP connection ID must be unused");
        Ok(ConnectionRegistration {
            control: Arc::clone(self),
            id,
            admission_active: true,
        })
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeTcpListenerError> {
        if matches!(
            self.lock().lifecycle,
            RuntimeTcpListenerLifecycle::Accepting
        ) {
            Ok(())
        } else {
            Err(RuntimeTcpListenerError::ShuttingDown)
        }
    }

    fn request_shutdown(&self) {
        let mut state = self.lock();
        if matches!(state.lifecycle, RuntimeTcpListenerLifecycle::Accepting) {
            self.request_shutdown_locked(&mut state);
            let _ = self.wake_writer.shutdown(Shutdown::Both);
        }
    }

    fn begin_shutdown(&self) -> ShutdownStart {
        let mut state = self.lock();
        match &state.lifecycle {
            RuntimeTcpListenerLifecycle::Stopped(report) => ShutdownStart::Finished(*report),
            RuntimeTcpListenerLifecycle::Draining => match state.shutdown_owner.take() {
                Some(owner) => ShutdownStart::Owner(owner),
                None => ShutdownStart::Wait,
            },
            RuntimeTcpListenerLifecycle::Accepting => {
                self.request_shutdown_locked(&mut state);
                let _ = self.wake_writer.shutdown(Shutdown::Both);
                ShutdownStart::Owner(
                    state
                        .shutdown_owner
                        .take()
                        .expect("fresh TCP shutdown retains its owner state"),
                )
            }
        }
    }

    fn request_shutdown_locked(&self, state: &mut RuntimeTcpListenerState) {
        state.lifecycle = RuntimeTcpListenerLifecycle::Draining;
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
        state.shutdown_counts = Some(RuntimeTcpShutdownCounts {
            connections_at_start,
            admissions_at_start,
            streams_signalled,
        });
        state.shutdown_owner = Some(ShutdownOwner {
            listener: state.listener.take(),
            connections_at_start,
            admissions_at_start,
            streams_signalled,
        });
        self.changed.notify_all();
    }

    fn wait_for_drain(
        &self,
        deadline: Instant,
        connections_at_start: usize,
        admissions_at_start: usize,
        streams_signalled: usize,
    ) -> RuntimeTcpShutdownReport {
        let mut state = self.lock();
        while state.accept_waiters != 0 || !state.connections.is_empty() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next_state, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("TCP listener shutdown state must not be poisoned");
            state = next_state;
            if timeout.timed_out() {
                break;
            }
        }
        RuntimeTcpShutdownReport {
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
        }
    }

    fn finish_shutdown(&self, report: RuntimeTcpShutdownReport) {
        let mut state = self.lock();
        assert!(
            matches!(state.lifecycle, RuntimeTcpListenerLifecycle::Draining),
            "only the TCP shutdown owner may publish a report"
        );
        state.lifecycle = RuntimeTcpListenerLifecycle::Stopped(report);
        self.changed.notify_all();
    }

    fn wait_for_shutdown_until(&self, deadline: Instant) -> Option<RuntimeTcpShutdownReport> {
        let mut state = self.lock();
        loop {
            match &state.lifecycle {
                RuntimeTcpListenerLifecycle::Stopped(report) => return Some(*report),
                RuntimeTcpListenerLifecycle::Draining => {
                    let remaining = deadline.checked_duration_since(Instant::now())?;
                    let (next, timeout) = self
                        .changed
                        .wait_timeout(state, remaining)
                        .expect("TCP listener shutdown state must not be poisoned");
                    state = next;
                    if timeout.timed_out()
                        && matches!(state.lifecycle, RuntimeTcpListenerLifecycle::Draining)
                    {
                        return None;
                    }
                }
                RuntimeTcpListenerLifecycle::Accepting => {
                    unreachable!("only a shutdown caller waits for TCP shutdown")
                }
            }
        }
    }

    fn shutdown_progress_report(&self) -> RuntimeTcpShutdownReport {
        let state = self.lock();
        if let RuntimeTcpListenerLifecycle::Stopped(report) = &state.lifecycle {
            return *report;
        }
        let counts = state
            .shutdown_counts
            .expect("draining TCP listener retains shutdown counts");
        RuntimeTcpShutdownReport {
            connections_at_start: counts.connections_at_start,
            admissions_at_start: counts.admissions_at_start,
            streams_signalled: counts.streams_signalled,
            remaining_connections: state.connections.len(),
            remaining_admissions: state
                .connections
                .values()
                .filter(|connection| connection.admission_active)
                .count(),
            remaining_accept_waiters: state.accept_waiters,
        }
    }

    fn complete_admission(&self, id: u64) {
        let mut state = self.lock();
        let connection = state
            .connections
            .get_mut(&id)
            .expect("live TCP connection registration must be present");
        connection.admission_active = false;
        self.changed.notify_all();
    }

    fn remove_connection(&self, id: u64) {
        let mut state = self.lock();
        let removed = state.connections.remove(&id);
        assert!(
            removed.is_some(),
            "live TCP connection registration must be present"
        );
        self.changed.notify_all();
    }

    fn remove_accept_waiter(&self) {
        let mut state = self.lock();
        assert!(
            state.accept_waiters > 0,
            "live TCP accept waiter must be counted"
        );
        state.accept_waiters -= 1;
        self.changed.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeTcpListenerState> {
        self.state
            .lock()
            .expect("TCP listener shutdown state must not be poisoned")
    }
}

impl fmt::Debug for RuntimeTcpListenerControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("RuntimeTcpListenerControl")
            .field("lifecycle", &state.lifecycle)
            .field("accept_waiters", &state.accept_waiters)
            .field("connections", &state.connections.len())
            .finish()
    }
}

struct RuntimeTcpListenerState {
    lifecycle: RuntimeTcpListenerLifecycle,
    listener: Option<TcpListener>,
    accept_waiters: usize,
    next_connection_id: u64,
    connections: BTreeMap<u64, RegisteredConnection>,
    shutdown_counts: Option<RuntimeTcpShutdownCounts>,
    shutdown_owner: Option<ShutdownOwner>,
}

#[derive(Debug, Clone, Copy)]
enum RuntimeTcpListenerLifecycle {
    Accepting,
    Draining,
    Stopped(RuntimeTcpShutdownReport),
}

struct RegisteredConnection {
    stream: TcpStream,
    admission_active: bool,
}

#[derive(Clone, Copy)]
struct RuntimeTcpShutdownCounts {
    connections_at_start: usize,
    admissions_at_start: usize,
    streams_signalled: usize,
}

enum ShutdownStart {
    Owner(ShutdownOwner),
    Wait,
    Finished(RuntimeTcpShutdownReport),
}

struct ShutdownOwner {
    listener: Option<TcpListener>,
    connections_at_start: usize,
    admissions_at_start: usize,
    streams_signalled: usize,
}

struct TcpAcceptWaiter {
    control: Arc<RuntimeTcpListenerControl>,
    listener: TcpListener,
}

impl Drop for TcpAcceptWaiter {
    fn drop(&mut self) {
        self.control.remove_accept_waiter();
    }
}

struct ConnectionRegistration {
    control: Arc<RuntimeTcpListenerControl>,
    id: u64,
    admission_active: bool,
}

impl ConnectionRegistration {
    fn connection_id(&self) -> u64 {
        self.id
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeTcpListenerError> {
        self.control.begin_protocol_work()
    }

    fn complete_admission(&mut self) -> Result<(), RuntimeTcpListenerError> {
        if self.admission_active {
            self.control.complete_admission(self.id);
            self.admission_active = false;
        }
        Ok(())
    }

    fn admission_complete(&self) -> bool {
        !self.admission_active
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
    fn connection_id(&self) -> u64 {
        self.registration.connection_id()
    }

    fn begin_protocol_work(&self) -> Result<(), RuntimeTcpListenerError> {
        self.registration.begin_protocol_work()
    }

    fn complete_admission(&mut self) -> Result<(), RuntimeTcpListenerError> {
        self.permits.complete_admission()?;
        self.registration.complete_admission()
    }

    fn admission_complete(&self) -> bool {
        self.permits.admission_complete()
    }
}

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

    fn complete_admission(&mut self) -> Result<(), RuntimeTcpListenerError> {
        if !self.admission_active {
            return Ok(());
        }
        let mut counts = self.state.lock().map_err(|_| {
            RuntimeTcpListenerError::ConnectionLimit(ConnectionLimitError::Unavailable)
        })?;
        assert!(
            counts.admissions > 0,
            "live TCP admission permit must be counted"
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
            "live TCP connection permit must be counted"
        );
        counts.connections -= 1;
        if self.admission_active {
            assert!(
                counts.admissions > 0,
                "live TCP admission permit must be counted"
            );
            counts.admissions -= 1;
        }
    }
}

enum ListenerWait {
    ListenerReady,
    ShuttingDown,
}

fn wait_for_listener_or_shutdown(
    listener: &TcpListener,
    wake_reader: &UnixStream,
    control: &RuntimeTcpListenerControl,
) -> Result<ListenerWait, RuntimeTcpListenerError> {
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
        // SAFETY: both descriptors remain owned for this call and the array
        // contains exactly two writable pollfd values.
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if result < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(RuntimeTcpListenerError::AcceptUnavailable);
        }
        if descriptors[1].revents != 0 || control.is_shutting_down() {
            return Ok(ListenerWait::ShuttingDown);
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            return Ok(ListenerWait::ListenerReady);
        }
        return Err(RuntimeTcpListenerError::AcceptUnavailable);
    }
}

fn allocate_connection_id(state: &mut RuntimeTcpListenerState) -> u64 {
    assert_ne!(
        state.next_connection_id, 0,
        "the next TCP connection ID must be nonzero"
    );
    let attempts = state
        .connections
        .len()
        .checked_add(1)
        .expect("active TCP connection count must fit in usize");
    for _ in 0..attempts {
        let id = state.next_connection_id;
        state.next_connection_id = if id == u64::MAX { 1 } else { id + 1 };
        if !state.connections.contains_key(&id) {
            return id;
        }
    }
    unreachable!("a TCP connection permit guarantees an unused connection ID")
}

#[cfg(test)]
mod tests {
    use std::{net::TcpStream, thread, time::Duration};

    use super::*;
    use crate::{RuntimeLimits, RuntimeTimeouts, MIN_WRITE_LIMIT};

    fn limits() -> RuntimeLimits {
        RuntimeLimits::new(2, 1, MIN_WRITE_LIMIT, 4).expect("test limits")
    }

    fn timeouts() -> RuntimeTimeouts {
        RuntimeTimeouts::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
        .expect("test timeouts")
    }

    fn listener() -> RuntimeTcpListener {
        RuntimeTcpListener::bind(
            "127.0.0.1:0".parse().expect("test address"),
            crate::runtime_tls::test_server_config(),
            limits(),
            timeouts(),
        )
        .expect("test TCP listener")
    }

    #[test]
    fn accepts_a_stream_with_bounded_admission_and_redacted_debug() {
        let listener = listener();
        let client = TcpStream::connect(listener.local_addr()).expect("test client");
        let accepted = listener.accept().expect("accepted stream");
        assert_ne!(accepted.connection_id(), 0);
        assert!(!accepted.authentication_deadline().le(&Instant::now()));
        assert_eq!(accepted.limits().max_queued_frames, 4);
        assert!(format!("{accepted:?}").contains("<redacted>"));
        assert!(format!("{listener:?}").contains("<redacted>"));
        drop(accepted);
        drop(client);
        assert!(listener.shutdown().drained());
    }

    #[test]
    fn shutdown_wakes_accept_and_stops_future_admission() {
        let listener = Arc::new(listener());
        let waiting = Arc::clone(&listener);
        let accept = thread::spawn(move || waiting.accept());
        thread::sleep(Duration::from_millis(10));
        listener.shutdown_handle().request_shutdown();
        assert!(matches!(
            accept.join().expect("accept thread"),
            Err(RuntimeTcpListenerError::ShuttingDown)
        ));
        assert!(matches!(
            listener.accept(),
            Err(RuntimeTcpListenerError::ShuttingDown)
        ));
        assert!(listener.shutdown().drained());
    }

    #[test]
    fn joinable_worker_is_signalled_by_shutdown_and_releases_its_lease() {
        let listener = Arc::new(listener());
        let server = Arc::clone(&listener);
        let worker_thread = thread::spawn(move || {
            let accepted = server.accept().expect("accepted stream");
            server
                .spawn_protocol(accepted, |mut accepted| {
                    let mut buffer = [0; 1];
                    let _ = accepted.read(&mut buffer);
                    Ok(())
                })
                .expect("worker")
        });
        let client = TcpStream::connect(listener.local_addr()).expect("test client");
        let worker = worker_thread.join().expect("worker creation thread");
        let report = listener.shutdown();
        assert_eq!(report.connections_at_start(), 1);
        assert_eq!(report.streams_signalled(), 1);
        assert!(report.drained());
        assert!(worker.join().is_ok());
        drop(client);
    }
}
