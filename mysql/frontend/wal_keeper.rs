//! Empties a database's WAL on a thread of its own.
//!
//! A write large enough leaves the WAL holding more than the engine's own
//! checkpoint empties, and emptying it copies every frame into the database
//! and syncs it: measured, about two seconds after an UPDATE of 128,000 rows.
//! Done inside the statement that crossed the bound, that time lands on one
//! client's answer. So a session only asks, and this thread does the work over
//! a connection of its own, which leaves the session free to run its next
//! statement meanwhile.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;

use turso_core::{CheckpointMode, Database, LimboError, Result};

use crate::database_users::DatabaseUser;

/// The thread one catalog empties its databases' WALs on, stopped and joined
/// when the catalog goes.
pub(crate) struct WalKeeper {
    handle: WalKeeperHandle,
    thread: Option<JoinHandle<()>>,
}

/// What a session asks the keeper through.
#[derive(Clone)]
pub(crate) struct WalKeeperHandle {
    requests: Sender<Request>,
    /// A failure the thread met that no session has been told of yet.
    failure: Arc<Mutex<Option<LimboError>>>,
    /// How many WALs the thread has emptied.
    #[cfg(test)]
    pub(crate) truncated: Arc<std::sync::atomic::AtomicUsize>,
}

enum Request {
    /// The database is held weakly, so a request never keeps one open. The
    /// user counts the keeper as using a catalog's database while it empties
    /// the WAL, which writes the database file, so that a drop never runs
    /// beside it; a database dropped, or being dropped, is left alone.
    Truncate(Weak<Database>, Option<DatabaseUser>),
    #[cfg(test)]
    Answer(Sender<()>),
    Stop,
}

impl WalKeeper {
    pub(crate) fn start() -> Self {
        let (requests, received) = mpsc::channel();
        let handle = WalKeeperHandle {
            requests,
            failure: Arc::default(),
            #[cfg(test)]
            truncated: Arc::default(),
        };
        let thread = std::thread::Builder::new()
            .name("turso-mysql-wal-keeper".to_owned())
            .spawn({
                let handle = handle.clone();
                move || keep(received, handle)
            })
            .expect("the WAL keeper thread must start");
        Self {
            handle,
            thread: Some(thread),
        }
    }

    pub(crate) fn handle(&self) -> WalKeeperHandle {
        self.handle.clone()
    }
}

impl Drop for WalKeeper {
    fn drop(&mut self) {
        // A session still holding a handle keeps the channel open, so the
        // thread is told to stop rather than left to notice.
        let _ = self.handle.requests.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the WAL keeper thread must not panic");
        }
    }
}

impl WalKeeperHandle {
    /// Asks for one database's WAL to be emptied, and answers a failure the
    /// thread met since a session was last told, if there was one.
    pub(crate) fn ask_to_truncate(
        &self,
        database: &Weak<Database>,
        user: Option<DatabaseUser>,
    ) -> Result<()> {
        if let Some(failure) = self
            .failure
            .lock()
            .expect("WAL keeper failure mutex poisoned")
            .take()
        {
            return Err(failure);
        }
        // The thread outlives every session of its catalog, so a request only
        // goes unread once the catalog is closing, when there is nothing left
        // to empty the WAL for.
        let _ = self
            .requests
            .send(Request::Truncate(database.clone(), user));
        Ok(())
    }

    /// Waits for every request sent before this one to be done.
    #[cfg(test)]
    pub(crate) fn wait_for_the_requests_before(&self) {
        let (answered, answer) = mpsc::channel();
        self.requests
            .send(Request::Answer(answered))
            .expect("the WAL keeper thread must be running");
        answer.recv().expect("the WAL keeper thread must answer");
    }
}

fn keep(received: Receiver<Request>, handle: WalKeeperHandle) {
    while let Ok(request) = received.recv() {
        // Every statement past the bound asks until the WAL is emptied, so
        // the requests that piled up meanwhile are read together and each
        // database is emptied once.
        let mut databases: Vec<(Weak<Database>, Option<DatabaseUser>)> = Vec::new();
        let mut stop = false;
        #[cfg(test)]
        let mut answers = Vec::new();
        for request in std::iter::once(request).chain(received.try_iter()) {
            match request {
                Request::Truncate(database, user) => {
                    if !databases.iter().any(|(kept, _)| kept.ptr_eq(&database)) {
                        databases.push((database, user));
                    }
                }
                #[cfg(test)]
                Request::Answer(answer) => answers.push(answer),
                Request::Stop => stop = true,
            }
        }
        for (database, user) in databases {
            let Some(database) = database.upgrade() else {
                continue;
            };
            if user
                .as_ref()
                .is_some_and(|user| user.start_using(std::time::Duration::ZERO).is_err())
            {
                continue;
            }
            let truncated = truncate(&database);
            drop(user);
            match truncated {
                Ok(()) => {
                    #[cfg(test)]
                    handle
                        .truncated
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                Err(error) => {
                    *handle
                        .failure
                        .lock()
                        .expect("WAL keeper failure mutex poisoned") = Some(error);
                }
            }
        }
        #[cfg(test)]
        for answer in answers {
            let _ = answer.send(());
        }
        if stop {
            return;
        }
    }
}

/// Empties one database's WAL over a connection of its own. Another session
/// reading at the same moment keeps the WAL busy, and a later request tries
/// again.
fn truncate(database: &Arc<Database>) -> Result<()> {
    let connection = database.connect()?;
    let result = match connection.checkpoint(CheckpointMode::Truncate {
        upper_bound_inclusive: None,
    }) {
        Ok(_) | Err(LimboError::Busy) | Err(LimboError::BusySnapshot) => Ok(()),
        Err(error) => Err(error),
    };
    connection.close()?;
    result
}
