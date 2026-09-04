//! Blocking TCP listener ownership for the TLS protocol runtime.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use turso_mysql::MySqlDatabaseCatalog;

use crate::runtime_account_reload_supervisor::{
    RuntimeAccountReloadSupervisor, RuntimeAccountReloadSupervisorJoinError,
};
use crate::runtime_tcp_connection::{RuntimeTcpConnection, RuntimeTcpConnectionError};
use crate::{
    AccountStoreCheckpointReader, ConnectionLimitError, RuntimeAccountReload, RuntimeAccountStore,
    RuntimeAccountStoreError, RuntimeConfig, RuntimeLimits, RuntimeTimeouts, TlsMaterialError,
    TlsServerConfig,
};

/// A blocking TCP listener that retains validated TLS material and every
/// accepted stream until its worker finishes.
pub struct RuntimeTcpListener {
    control: Arc<RuntimeTcpListenerControl>,
    wake_reader: UnixStream,
    config: RuntimeConfig,
    accounts: Arc<RuntimeAccountStore>,
    reload_supervisor: Mutex<Option<RuntimeAccountReloadSupervisor>>,
    catalog: Arc<MySqlDatabaseCatalog>,
    tls_config: Arc<TlsServerConfig>,
    local_addr: SocketAddr,
}

impl RuntimeTcpListener {
    /// Opens runtime state, validates TLS material, and binds one TCP endpoint.
    pub fn bind(
        config: &RuntimeConfig,
        checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
    ) -> Result<Self, RuntimeTcpListenerError> {
        let tcp = config
            .tcp()
            .ok_or(RuntimeTcpListenerError::TcpListenerRequired)?;
        let tls_config =
            TlsServerConfig::load(tcp.tls()).map_err(RuntimeTcpListenerError::TlsMaterial)?;
        Self::bind_with_tls(config, checkpoint_reader, tls_config)
    }

    pub(crate) fn bind_with_tls(
        config: &RuntimeConfig,
        checkpoint_reader: Arc<dyn AccountStoreCheckpointReader>,
        tls_config: TlsServerConfig,
    ) -> Result<Self, RuntimeTcpListenerError> {
        let tcp = config
            .tcp()
            .ok_or(RuntimeTcpListenerError::TcpListenerRequired)?;
        let accounts = Arc::new(
            RuntimeAccountStore::open(config, checkpoint_reader)
                .map_err(RuntimeTcpListenerError::AccountStore)?,
        );
        let catalog = MySqlDatabaseCatalog::open(config.data_root())
            .map_err(|_| RuntimeTcpListenerError::CatalogUnavailable)?;
        match accounts.reload_once() {
            RuntimeAccountReload::Healthy(_) => {}
            RuntimeAccountReload::Degraded(error) => {
                return Err(RuntimeTcpListenerError::AccountStore(error));
            }
        }

        let listener =
            TcpListener::bind(tcp.bind()).map_err(|_| RuntimeTcpListenerError::BindUnavailable)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        let local_addr = listener
            .local_addr()
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        let (wake_reader, wake_writer) =
            UnixStream::pair().map_err(|_| RuntimeTcpListenerError::WakeUnavailable)?;
        let reload_supervisor =
            RuntimeAccountReloadSupervisor::spawn(Arc::clone(&accounts), config.reload_interval())
                .map_err(|_| {
                    RuntimeTcpListenerError::AccountStore(
                        RuntimeAccountStoreError::SupervisorUnavailable,
                    )
                })?;
        let control = Arc::new(RuntimeTcpListenerControl::new(
            listener,
            wake_writer,
            config.limits(),
        ));
        Ok(Self {
            control,
            wake_reader,
            config: config.clone(),
            accounts,
            reload_supervisor: Mutex::new(Some(reload_supervisor)),
            catalog,
            tls_config: Arc::new(tls_config),
            local_addr,
        })
    }

    /// Returns the actual bound address, including an assigned ephemeral port.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns whether shutdown has begun and no new stream can be returned.
    pub fn is_shutting_down(&self) -> bool {
        self.control.is_shutting_down()
    }

    /// Returns whether the transport may admit a new authentication attempt.
    pub fn is_ready_for_new_connections(&self) -> bool {
        !self.is_shutting_down() && self.accounts.is_ready_for_new_connections()
    }

    pub(crate) fn wait_until_ready_or_shutdown(&self) -> bool {
        !self.is_shutting_down()
            && self.accounts.wait_until_ready_or_shutdown()
            && !self.is_shutting_down()
    }

    /// Performs one serialized account-store reload.
    pub fn reload_accounts_once(&self) -> RuntimeAccountReload {
        self.accounts.reload_once()
    }

    /// Blocks for one accepted TCP stream or a lifecycle error.
    pub(crate) fn accept(&self) -> Result<AcceptedTcpStream, RuntimeTcpListenerError> {
        let accept_waiter = self.control.start_accept()?;
        if self.is_shutting_down() {
            return Err(RuntimeTcpListenerError::ShuttingDown);
        }
        if !self.accounts.is_ready_for_new_connections() {
            return Err(RuntimeTcpListenerError::AccountNotReady);
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
        if !self.is_ready_for_new_connections() {
            return Err(if self.is_shutting_down() {
                RuntimeTcpListenerError::ShuttingDown
            } else {
                RuntimeTcpListenerError::AccountNotReady
            });
        }
        let permits = ConnectionPermits::acquire(&self.control.permits)
            .map_err(RuntimeTcpListenerError::ConnectionLimit)?;
        stream
            .set_nonblocking(false)
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        stream
            .set_read_timeout(Some(self.config.timeouts().tls()))
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        stream
            .set_write_timeout(Some(self.config.timeouts().write()))
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        let registration = self
            .accounts
            .while_ready_for_new_connection(|| self.control.register_connection(&stream))
            .ok_or(RuntimeTcpListenerError::AccountNotReady)??;
        let tls_deadline = Instant::now() + self.config.timeouts().tls();
        drop(accept_waiter);
        Ok(AcceptedTcpStream {
            stream,
            lease: ConnectionLease {
                permits,
                registration,
            },
            accounts: Arc::clone(&self.accounts),
            catalog: Arc::clone(&self.catalog),
            tls_config: Arc::clone(&self.tls_config),
            tls_deadline,
            limits: self.config.limits(),
            timeouts: self.config.timeouts(),
        })
    }

    pub(crate) fn spawn_protocol<F>(
        &self,
        stream: AcceptedTcpStream,
        completion: F,
    ) -> Result<RuntimeTcpConnectionWorker, RuntimeTcpConnectionSpawnError>
    where
        F: FnOnce() + Send + 'static,
    {
        let connection_id = stream.connection_id();
        let handle = thread::Builder::new()
            .name(format!("turso-mysql-tcp-{connection_id}"))
            .spawn(move || {
                let _completion = TcpWorkerCompletionGuard::new(completion);
                RuntimeTcpConnection::new(stream)?.run()
            })
            .map_err(|_| RuntimeTcpConnectionSpawnError::SpawnUnavailable)?;
        Ok(RuntimeTcpConnectionWorker { handle })
    }

    pub(crate) fn shutdown_handle(&self) -> RuntimeTcpListenerShutdown {
        RuntimeTcpListenerShutdown {
            control: Arc::clone(&self.control),
            accounts: Arc::clone(&self.accounts),
        }
    }

    /// Stops acceptance, signals active streams, and reports bounded drain.
    pub fn shutdown(&self) -> RuntimeTcpShutdownReport {
        self.shutdown_until(Instant::now() + self.config.timeouts().shutdown())
    }

    pub(crate) const fn shutdown_timeout(&self) -> Duration {
        self.config.timeouts().shutdown()
    }

    /// Repeats shutdown with a caller-owned absolute deadline.
    pub(crate) fn shutdown_until(&self, deadline: Instant) -> RuntimeTcpShutdownReport {
        self.accounts.begin_shutdown();
        let owner = match self.control.begin_shutdown() {
            ShutdownStart::Owner(owner) => owner,
            ShutdownStart::Wait => {
                let Some(report) = self.control.wait_for_shutdown_until(deadline) else {
                    return self.control.shutdown_progress_report();
                };
                return self.retry_reload_supervisor_shutdown(report, deadline);
            }
            ShutdownStart::Finished(report) => {
                return self.retry_reload_supervisor_shutdown(report, deadline);
            }
        };
        drop(owner.listener);
        let reload_supervisor = self.stop_reload_supervisor(deadline);
        let report = self.control.wait_for_drain(
            deadline,
            owner.connections_at_start,
            owner.admissions_at_start,
            owner.streams_signalled,
            reload_supervisor,
        );
        self.control.finish_shutdown(report);
        report
    }
}

/// A cloneable, non-blocking request to stop one TCP listener.
#[derive(Clone)]
pub(crate) struct RuntimeTcpListenerShutdown {
    control: Arc<RuntimeTcpListenerControl>,
    accounts: Arc<RuntimeAccountStore>,
}

impl RuntimeTcpListenerShutdown {
    pub(crate) fn request_shutdown(&self) {
        self.control.request_shutdown();
        self.accounts.begin_shutdown();
    }
}

struct TcpWorkerCompletionGuard<F: FnOnce()> {
    completion: Option<F>,
}

impl<F: FnOnce()> TcpWorkerCompletionGuard<F> {
    fn new(completion: F) -> Self {
        Self {
            completion: Some(completion),
        }
    }
}

impl<F: FnOnce()> Drop for TcpWorkerCompletionGuard<F> {
    fn drop(&mut self) {
        (self
            .completion
            .take()
            .expect("TCP completion callback must remain present until its guard drops"))();
    }
}

impl fmt::Debug for RuntimeTcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeTcpListener")
            .field("control", &self.control)
            .field("wake_reader", &"<redacted>")
            .field("config", &"<retained>")
            .field("accounts", &"<retained>")
            .field("reload_supervisor", &"<retained>")
            .field("catalog", &"<retained>")
            .field("tls_config", &"<redacted>")
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

impl Drop for RuntimeTcpListener {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl RuntimeTcpListener {
    fn retry_reload_supervisor_shutdown(
        &self,
        report: RuntimeTcpShutdownReport,
        deadline: Instant,
    ) -> RuntimeTcpShutdownReport {
        if !matches!(
            report.reload_supervisor,
            RuntimeTcpReloadSupervisorShutdown::TimedOut
        ) {
            return report;
        }
        let reload_supervisor = self.stop_reload_supervisor(deadline);
        self.control
            .update_reload_supervisor_shutdown(reload_supervisor)
    }

    fn stop_reload_supervisor(&self, deadline: Instant) -> RuntimeTcpReloadSupervisorShutdown {
        let mut slot = self
            .reload_supervisor
            .lock()
            .expect("TCP account reload supervisor state must not be poisoned");
        let Some(supervisor) = slot.as_mut() else {
            return RuntimeTcpReloadSupervisorShutdown::Stopped;
        };
        supervisor.request_stop();
        match supervisor.join_until(deadline) {
            Ok(()) => {
                *slot = None;
                RuntimeTcpReloadSupervisorShutdown::Stopped
            }
            Err(RuntimeAccountReloadSupervisorJoinError::TimedOut) => {
                RuntimeTcpReloadSupervisorShutdown::TimedOut
            }
            Err(RuntimeAccountReloadSupervisorJoinError::Worker(_)) => {
                *slot = None;
                RuntimeTcpReloadSupervisorShutdown::Failed
            }
        }
    }
}

/// One accepted TCP stream with active connection and admission permits.
pub(crate) struct AcceptedTcpStream {
    stream: TcpStream,
    lease: ConnectionLease,
    accounts: Arc<RuntimeAccountStore>,
    catalog: Arc<MySqlDatabaseCatalog>,
    tls_config: Arc<TlsServerConfig>,
    tls_deadline: Instant,
    limits: RuntimeLimits,
    timeouts: RuntimeTimeouts,
}

impl AcceptedTcpStream {
    /// Returns the nonzero protocol connection identifier.
    pub(crate) fn connection_id(&self) -> u32 {
        self.lease.connection_id()
    }

    /// Starts protocol work if shutdown has not begun.
    pub(crate) fn begin_protocol_work(&self) -> Result<(), RuntimeTcpListenerError> {
        self.lease.begin_protocol_work()
    }

    /// Marks authentication complete and switches future reads to the idle timeout.
    pub(crate) fn complete_admission(&mut self) -> Result<(), RuntimeTcpListenerError> {
        self.set_read_timeout(self.timeouts.idle())
            .map_err(|_| RuntimeTcpListenerError::TransportConfiguration)?;
        self.lease.complete_admission()
    }

    /// Returns the fixed deadline for completing the mandatory TLS transition.
    pub(crate) const fn tls_deadline(&self) -> Instant {
        self.tls_deadline
    }

    /// Clones the account store retained for the protocol owner.
    pub(crate) fn account_store(&self) -> Arc<RuntimeAccountStore> {
        Arc::clone(&self.accounts)
    }

    /// Clones the database catalog retained for the protocol owner.
    pub(crate) fn catalog(&self) -> Arc<MySqlDatabaseCatalog> {
        Arc::clone(&self.catalog)
    }

    /// Returns the validated, immutable TLS configuration retained by the listener.
    pub(crate) fn tls_config(&self) -> Arc<TlsServerConfig> {
        Arc::clone(&self.tls_config)
    }

    /// Returns the per-connection bounded response limits.
    pub(crate) const fn limits(&self) -> RuntimeLimits {
        self.limits
    }

    /// Returns the selected lifecycle timeouts.
    pub(crate) const fn timeouts(&self) -> RuntimeTimeouts {
        self.timeouts
    }

    /// Applies one blocking read timeout for the protocol owner's current phase.
    pub(crate) fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.stream.set_read_timeout(Some(timeout))
    }

    /// Applies one blocking write timeout for the protocol owner's current phase.
    pub(crate) fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.stream.set_write_timeout(Some(timeout))
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
    handle: thread::JoinHandle<Result<(), RuntimeTcpConnectionError>>,
}

impl RuntimeTcpConnectionWorker {
    /// Returns whether the protocol owner has stopped.
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

/// Spawning a TCP protocol owner failed without exposing addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeTcpConnectionSpawnError {
    /// The worker thread could not be started.
    SpawnUnavailable,
}

impl fmt::Display for RuntimeTcpConnectionSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnUnavailable => f.write_str("TCP protocol worker could not start"),
        }
    }
}

impl Error for RuntimeTcpConnectionSpawnError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SpawnUnavailable => None,
        }
    }
}

/// A TCP listener operation failed without exposing endpoint or peer details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTcpListenerError {
    /// This listener requires a configured TCP endpoint.
    TcpListenerRequired,
    /// Account state did not match the external checkpoint.
    AccountStore(RuntimeAccountStoreError),
    /// A failed reload blocks new authentication attempts.
    AccountNotReady,
    /// The database catalog could not be opened.
    CatalogUnavailable,
    /// TLS certificate or private-key material was rejected.
    TlsMaterial(TlsMaterialError),
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
            Self::TcpListenerRequired => f.write_str("TCP listener configuration is required"),
            Self::AccountStore(error) => write!(f, "TCP account store failed: {error}"),
            Self::AccountNotReady => f.write_str("TCP account store is not ready"),
            Self::CatalogUnavailable => f.write_str("TCP database catalog is unavailable"),
            Self::TlsMaterial(error) => write!(f, "TCP TLS material failed: {error}"),
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
            Self::AccountStore(error) => Some(error),
            Self::TlsMaterial(error) => Some(error),
            Self::TcpListenerRequired
            | Self::AccountNotReady
            | Self::CatalogUnavailable
            | Self::BindUnavailable
            | Self::AcceptUnavailable
            | Self::WakeUnavailable
            | Self::TransportConfiguration
            | Self::ShuttingDown => None,
        }
    }
}

/// The result of stopping the listener-owned account reload worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeTcpReloadSupervisorShutdown {
    /// The worker stopped and its thread was joined.
    Stopped,
    /// The shared shutdown deadline elapsed while the worker was still running.
    TimedOut,
    /// The worker ended with a redacted failure.
    Failed,
}

/// The bounded result of one TCP-listener shutdown attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeTcpShutdownReport {
    connections_at_start: usize,
    admissions_at_start: usize,
    streams_signalled: usize,
    remaining_connections: usize,
    remaining_admissions: usize,
    remaining_accept_waiters: usize,
    reload_supervisor: RuntimeTcpReloadSupervisorShutdown,
}

impl RuntimeTcpShutdownReport {
    /// Returns the number of active streams when shutdown began.
    pub const fn connections_at_start(&self) -> usize {
        self.connections_at_start
    }

    /// Returns the number of authenticating streams when shutdown began.
    pub const fn admissions_at_start(&self) -> usize {
        self.admissions_at_start
    }

    /// Returns the number of streams signalled for shutdown.
    pub const fn streams_signalled(&self) -> usize {
        self.streams_signalled
    }

    /// Returns the number of streams that remain owned.
    pub const fn remaining_connections(&self) -> usize {
        self.remaining_connections
    }

    /// Returns the number of authentication permits that remain active.
    pub const fn remaining_admissions(&self) -> usize {
        self.remaining_admissions
    }

    /// Returns the number of blocked accept calls that remain.
    pub const fn remaining_accept_waiters(&self) -> usize {
        self.remaining_accept_waiters
    }

    /// Returns whether the account reload worker stopped within the deadline.
    pub const fn reload_supervisor(&self) -> RuntimeTcpReloadSupervisorShutdown {
        self.reload_supervisor
    }

    /// Returns whether acceptance and every registered stream drained.
    pub const fn drained(&self) -> bool {
        self.remaining_connections == 0
            && self.remaining_admissions == 0
            && self.remaining_accept_waiters == 0
            && matches!(
                self.reload_supervisor,
                RuntimeTcpReloadSupervisorShutdown::Stopped
            )
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

    fn request_shutdown(&self) {
        let mut state = self.lock();
        if matches!(state.lifecycle, RuntimeTcpListenerLifecycle::Accepting) {
            self.request_shutdown_locked(&mut state);
            let _ = self.wake_writer.shutdown(Shutdown::Both);
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
        reload_supervisor: RuntimeTcpReloadSupervisorShutdown,
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
            reload_supervisor,
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

    fn update_reload_supervisor_shutdown(
        &self,
        reload_supervisor: RuntimeTcpReloadSupervisorShutdown,
    ) -> RuntimeTcpShutdownReport {
        let mut state = self.lock();
        let RuntimeTcpListenerLifecycle::Stopped(report) = &mut state.lifecycle else {
            unreachable!("only a completed TCP shutdown report can be updated")
        };
        if matches!(
            report.reload_supervisor,
            RuntimeTcpReloadSupervisorShutdown::TimedOut
        ) {
            report.reload_supervisor = reload_supervisor;
        }
        *report
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
            reload_supervisor: RuntimeTcpReloadSupervisorShutdown::TimedOut,
        }
    }

    fn complete_admission(&self, id: u32) {
        let mut state = self.lock();
        let connection = state
            .connections
            .get_mut(&id)
            .expect("live TCP connection registration must be present");
        connection.admission_active = false;
        self.changed.notify_all();
    }

    fn remove_connection(&self, id: u32) {
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
    next_connection_id: u32,
    connections: BTreeMap<u32, RegisteredConnection>,
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
    id: u32,
    admission_active: bool,
}

impl ConnectionRegistration {
    fn connection_id(&self) -> u32 {
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

fn allocate_connection_id(state: &mut RuntimeTcpListenerState) -> u32 {
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
        state.next_connection_id = if id == u32::MAX { 1 } else { id + 1 };
        if !state.connections.contains_key(&id) {
            return id;
        }
    }
    unreachable!("a TCP connection permit guarantees an unused connection ID")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        io::{Read, Write},
        net::TcpStream,
        os::unix::fs::PermissionsExt,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    use rustls::pki_types::{ServerName, UnixTime};
    use tempfile::TempDir;
    use turso_mysql::MySqlDatabaseCatalog;

    use super::*;
    use crate::{
        AccountStoreCheckpoint, AccountStoreCheckpointAuthority, AccountStoreCheckpointRequest,
        AuthMoreData, AuthMoreDataKind, AuthOkPacket, CheckpointAuthorityId, CheckpointPersistence,
        CheckpointReadError, ClientHandshakeResponseConfig, ClientSslRequestConfig,
        ColumnCountPacket, ColumnDefinitionPacket, DatabasePrivileges, GlobalPrivileges,
        InitialHandshake, OfflineAccountProvisioner, PacketCodec, ProtectedPassword,
        ResponseOkPacket, ResultTerminatorPacket, RuntimeConfig, RuntimeLimits, RuntimeTimeouts,
        TcpConfig, TextRowPacket, TextRowValue, TlsConfig, CACHING_SHA2_PASSWORD_PLUGIN,
        CLIENT_DEPRECATE_EOF, CLIENT_HANDSHAKE_SEQUENCE_ID, CLIENT_SSL, COMMAND_SEQUENCE_ID,
        COM_INIT_DB, COM_PING, COM_QUERY, COM_QUIT, DEFAULT_UTF8MB4_COLLATION,
        MAX_COMMAND_PAYLOAD_LENGTH, MIN_WRITE_LIMIT, PACKET_HEADER_LEN,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };

    struct TestCheckpointReader {
        results: Mutex<VecDeque<Result<AccountStoreCheckpoint, CheckpointReadError>>>,
    }

    impl TestCheckpointReader {
        fn new(
            results: impl IntoIterator<Item = Result<AccountStoreCheckpoint, CheckpointReadError>>,
        ) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
            }
        }
    }

    impl AccountStoreCheckpointReader for TestCheckpointReader {
        fn request_checkpoint(
            &self,
            _authority: &CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
            let result = self
                .results
                .lock()
                .expect("test checkpoint queue")
                .pop_front()
                .unwrap_or(Err(CheckpointReadError::Missing));
            Ok(AccountStoreCheckpointRequest::completed(result))
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

    struct ProtocolRuntime {
        listener: Arc<RuntimeTcpListener>,
        _data_root: TempDir,
        _account_root: TempDir,
    }

    fn protocol_runtime(tls_timeout: Duration) -> ProtocolRuntime {
        let data_root = private_directory();
        let account_root = private_directory();
        let mut password = b"secret".to_vec();
        let account = crate::provision_account(
            "alice",
            ProtectedPassword::new(password.as_mut_slice()),
            true,
            GlobalPrivileges::new(true, false),
        )
        .expect("test account");
        let grant = account.grant("testdb", DatabasePrivileges::new(true, true, false, false));
        let mut authority = TestAuthority::default();
        let provisioner = OfflineAccountProvisioner::initialize(
            account_root.path(),
            account.into_builder().with_grant(grant),
            &mut authority,
        )
        .expect("test account store");
        let checkpoint = provisioner.checkpoint().expect("test checkpoint");
        drop(provisioner);

        let catalog = MySqlDatabaseCatalog::open(data_root.path()).expect("test catalog");
        catalog.create("testdb").expect("test database");
        drop(catalog);

        let tls = TlsConfig::new(
            data_root.path().join("unused-certificate.pem"),
            data_root.path().join("unused-private-key.pem"),
        )
        .expect("test TLS paths");
        let config = RuntimeConfig::new(
            Some(TcpConfig::new(
                "127.0.0.1:0".parse().expect("test address"),
                tls,
            )),
            None,
            data_root.path().canonicalize().expect("data root"),
            account_root.path().canonicalize().expect("account root"),
            CheckpointAuthorityId::new("runtime-checkpoints").expect("authority ID"),
            Duration::from_secs(1),
            RuntimeLimits::new(4, 4, MIN_WRITE_LIMIT, 16).expect("test limits"),
            RuntimeTimeouts::new(
                Duration::from_secs(1),
                tls_timeout,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(1),
                Duration::from_secs(2),
            )
            .expect("test timeouts")
            .with_query_timeout(Duration::from_secs(1))
            .expect("query timeout"),
        )
        .expect("test runtime config");
        let reader = Arc::new(TestCheckpointReader::new([Ok(checkpoint), Ok(checkpoint)]));
        let listener = RuntimeTcpListener::bind_with_tls(
            &config,
            reader,
            crate::runtime_tls::test_server_config(),
        )
        .expect("test TCP listener");
        ProtocolRuntime {
            listener: Arc::new(listener),
            _data_root: data_root,
            _account_root: account_root,
        }
    }

    fn private_directory() -> TempDir {
        let target =
            fs::canonicalize(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
                .expect("target directory");
        let directory = tempfile::tempdir_in(target).expect("fixture directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("fixture permissions");
        directory
    }

    fn packet_codec() -> PacketCodec {
        PacketCodec::new(MAX_COMMAND_PAYLOAD_LENGTH).expect("test codec")
    }

    fn read_frame(stream: &mut impl Read) -> Vec<u8> {
        let mut header = [0; PACKET_HEADER_LEN];
        stream.read_exact(&mut header).expect("frame header");
        let payload_length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        assert!(payload_length <= MAX_COMMAND_PAYLOAD_LENGTH);
        let mut frame = vec![0; PACKET_HEADER_LEN + payload_length];
        frame[..PACKET_HEADER_LEN].copy_from_slice(&header);
        stream
            .read_exact(&mut frame[PACKET_HEADER_LEN..])
            .expect("frame payload");
        frame
    }

    fn start_worker(listener: &RuntimeTcpListener) -> (TcpStream, RuntimeTcpConnectionWorker) {
        let client = TcpStream::connect(listener.local_addr()).expect("test client");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("client read timeout");
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("client write timeout");
        let stream = listener.accept().expect("accepted TCP stream");
        let worker = listener
            .spawn_protocol(stream, || {})
            .expect("TCP protocol worker");
        (client, worker)
    }

    #[derive(Debug)]
    struct AcceptAnyServer;

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![rustls::SignatureScheme::ECDSA_NISTP256_SHA256]
        }
    }

    fn client_tls_config() -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
                .expect("client TLS versions")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
                .with_no_client_auth(),
        )
    }

    fn authenticate_over_tls(
        mut client: TcpStream,
    ) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let codec = packet_codec();
        let greeting =
            InitialHandshake::decode(codec, &read_frame(&mut client)).expect("initial handshake");
        assert_ne!(greeting.connection_id, 0);
        assert_ne!(greeting.capability_flags() & CLIENT_SSL, 0);
        let capabilities =
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL | CLIENT_DEPRECATE_EOF;
        let ssl_request = ClientSslRequestConfig::new(capabilities, 0, DEFAULT_UTF8MB4_COLLATION)
            .encode(codec, CLIENT_HANDSHAKE_SEQUENCE_ID)
            .expect("SSLRequest");
        client.write_all(&ssl_request).expect("SSLRequest write");

        let server_name = ServerName::try_from("localhost").expect("server name");
        let mut connection =
            rustls::ClientConnection::new(client_tls_config(), server_name).expect("TLS client");
        while connection.is_handshaking() {
            connection.complete_io(&mut client).expect("TLS handshake");
        }
        let mut client = rustls::StreamOwned::new(connection, client);
        let response = ClientHandshakeResponseConfig::new(
            capabilities,
            0,
            DEFAULT_UTF8MB4_COLLATION,
            "alice",
            vec![0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN.to_owned()),
            None,
        )
        .encode(codec, 2)
        .expect("client handshake response");
        client.write_all(&response).expect("authentication start");
        let auth_more = AuthMoreData::decode(codec, &read_frame(&mut client))
            .expect("full authentication request");
        assert_eq!(auth_more.kind, AuthMoreDataKind::FullAuthenticationRequired);
        client
            .write_all(&codec.encode(4, b"secret\0").expect("password frame"))
            .expect("password write");
        AuthOkPacket::decode(codec, &read_frame(&mut client)).expect("authentication OK");
        client
    }

    #[test]
    fn accepts_a_stream_with_bounded_admission_and_redacted_debug() {
        let runtime = protocol_runtime(Duration::from_secs(1));
        let client = TcpStream::connect(runtime.listener.local_addr()).expect("test client");
        let accepted = runtime.listener.accept().expect("accepted stream");
        assert_ne!(accepted.connection_id(), 0);
        assert!(!accepted.tls_deadline().le(&Instant::now()));
        assert_eq!(accepted.limits().max_write_frames(), 16);
        assert!(format!("{accepted:?}").contains("<redacted>"));
        assert!(format!("{:?}", runtime.listener).contains("<redacted>"));
        drop(accepted);
        drop(client);
        assert!(runtime.listener.shutdown().drained());
    }

    #[test]
    fn shutdown_wakes_accept_and_stops_future_admission() {
        let runtime = protocol_runtime(Duration::from_secs(1));
        let waiting = Arc::clone(&runtime.listener);
        let accept = thread::spawn(move || waiting.accept());
        thread::sleep(Duration::from_millis(10));
        assert!(runtime.listener.shutdown().drained());
        assert!(matches!(
            accept.join().expect("accept thread"),
            Err(RuntimeTcpListenerError::ShuttingDown)
        ));
        assert!(matches!(
            runtime.listener.accept(),
            Err(RuntimeTcpListenerError::ShuttingDown)
        ));
        assert!(runtime.listener.shutdown().drained());
    }

    #[test]
    fn listener_owned_reload_blocks_new_connections_and_joins_on_shutdown() {
        let runtime = protocol_runtime(Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(3);
        while runtime.listener.is_ready_for_new_connections() {
            assert!(
                Instant::now() < deadline,
                "periodic account reload did not run"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let client = TcpStream::connect(runtime.listener.local_addr()).expect("test client");
        assert!(matches!(
            runtime.listener.accept(),
            Err(RuntimeTcpListenerError::AccountNotReady)
        ));
        drop(client);
        let report = runtime.listener.shutdown();
        assert_eq!(
            report.reload_supervisor(),
            RuntimeTcpReloadSupervisorShutdown::Stopped
        );
        assert!(report.drained());
    }

    #[test]
    fn tls_authentication_init_db_query_ping_and_quit_use_the_typed_owner() {
        let runtime = protocol_runtime(Duration::from_secs(1));
        let (client, worker) = start_worker(&runtime.listener);
        let mut client = authenticate_over_tls(client);
        let codec = packet_codec();

        let mut init_db = vec![COM_INIT_DB];
        init_db.extend_from_slice(b"testdb");
        client
            .write_all(
                &codec
                    .encode(COMMAND_SEQUENCE_ID, &init_db)
                    .expect("init db"),
            )
            .expect("init db write");
        assert_eq!(
            ResponseOkPacket::decode(codec, &read_frame(&mut client))
                .expect("init db OK")
                .sequence_id,
            1
        );

        let mut query = vec![COM_QUERY];
        query.extend_from_slice(b"SELECT 1");
        client
            .write_all(&codec.encode(COMMAND_SEQUENCE_ID, &query).expect("query"))
            .expect("query write");
        assert_eq!(
            ColumnCountPacket::decode(codec, &read_frame(&mut client))
                .expect("column count")
                .column_count,
            1
        );
        assert_eq!(
            ColumnDefinitionPacket::decode(codec, &read_frame(&mut client))
                .expect("column definition")
                .sequence_id,
            2
        );
        let row_frame = read_frame(&mut client);
        let row = TextRowPacket::decode(codec, &row_frame, 1).expect("text row");
        assert_eq!(row.values, [TextRowValue::Bytes(b"1")]);
        assert!(matches!(
            ResultTerminatorPacket::decode(
                codec,
                &read_frame(&mut client),
                REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES
                    | CLIENT_SSL
                    | CLIENT_DEPRECATE_EOF,
            )
            .expect("result terminator"),
            ResultTerminatorPacket::Ok(packet) if packet.sequence_id == 4
        ));

        client
            .write_all(
                &codec
                    .encode(COMMAND_SEQUENCE_ID, &[COM_PING])
                    .expect("ping"),
            )
            .expect("ping write");
        assert_eq!(
            ResponseOkPacket::decode(codec, &read_frame(&mut client))
                .expect("ping OK")
                .sequence_id,
            1
        );
        client
            .write_all(
                &codec
                    .encode(COMMAND_SEQUENCE_ID, &[COM_QUIT])
                    .expect("quit"),
            )
            .expect("quit write");
        drop(client);
        assert!(worker.join().is_ok());
        assert!(runtime.listener.shutdown().drained());
    }

    #[test]
    fn tls_handshake_uses_one_absolute_deadline() {
        let runtime = protocol_runtime(Duration::from_millis(100));
        let (mut client, worker) = start_worker(&runtime.listener);
        let codec = packet_codec();
        let greeting =
            InitialHandshake::decode(codec, &read_frame(&mut client)).expect("initial handshake");
        assert_ne!(greeting.capability_flags() & CLIENT_SSL, 0);
        let ssl_request = ClientSslRequestConfig::new(
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES | CLIENT_SSL,
            0,
            DEFAULT_UTF8MB4_COLLATION,
        )
        .encode(codec, CLIENT_HANDSHAKE_SEQUENCE_ID)
        .expect("SSLRequest");
        client.write_all(&ssl_request).expect("SSLRequest write");

        assert!(matches!(
            worker.join(),
            Err(RuntimeTcpConnectionWorkerError::Connection(
                RuntimeTcpConnectionError::TlsDeadlineExceeded
            ))
        ));
        drop(client);
        assert!(runtime.listener.shutdown().drained());
    }

    #[test]
    fn valid_plaintext_handshake_response_is_rejected_before_authentication() {
        let runtime = protocol_runtime(Duration::from_secs(1));
        let (mut client, worker) = start_worker(&runtime.listener);
        let codec = packet_codec();
        InitialHandshake::decode(codec, &read_frame(&mut client)).expect("initial handshake");
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
        .encode(codec, CLIENT_HANDSHAKE_SEQUENCE_ID)
        .expect("plaintext handshake response");
        client
            .write_all(&response)
            .expect("plaintext response write");

        assert!(matches!(
            worker.join(),
            Err(RuntimeTcpConnectionWorkerError::Connection(
                RuntimeTcpConnectionError::PlaintextRejected(_)
            ))
        ));
        drop(client);
        assert!(runtime.listener.shutdown().drained());
    }

    #[test]
    fn ready_flush_releases_admission_and_terminal_close_releases_the_lease() {
        let runtime = protocol_runtime(Duration::from_secs(1));
        let (client, worker) = start_worker(&runtime.listener);
        let client = authenticate_over_tls(client);

        let report = runtime.listener.shutdown();
        assert_eq!(report.connections_at_start(), 1);
        assert_eq!(report.admissions_at_start(), 0);
        assert_eq!(report.streams_signalled(), 1);
        assert!(report.drained());
        assert!(matches!(
            worker.join(),
            Err(RuntimeTcpConnectionWorkerError::Connection(_))
        ));
        drop(client);
    }
}
