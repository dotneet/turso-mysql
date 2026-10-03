//! The sessions logged in to one server, which `SHOW PROCESSLIST` lists and
//! `SHOW STATUS` counts.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Every session logged in to one server, and when the server opened.
pub struct MySqlSessionRegistry {
    opened: Instant,
    /// Each session keeps its activity behind a lock of its own, so a
    /// session noting what it runs never waits for another doing the same.
    /// A registration outliving its connection's ID — the listener lets an
    /// ID go when the stream closes, which can be before the session is
    /// dropped — holds the activity it was given, never the one that ID went
    /// to next.
    sessions: Mutex<BTreeMap<u32, Arc<Mutex<SessionActivity>>>>,
    /// How many commands the sessions sent, each statement of a query that
    /// holds several counted on its own, which `COM_STATISTICS` reports as
    /// `Questions`.
    questions: AtomicU64,
}

/// What one session is doing, as `SHOW PROCESSLIST` describes it.
#[derive(Debug, Clone)]
struct SessionActivity {
    account: String,
    host: String,
    database: Option<String>,
    running: Option<RunningStatement>,
    /// When the session last started or finished a command.
    since: Instant,
}

/// A statement a session is in the middle of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningStatement {
    /// `Query` for a statement sent as text, `Execute` for a prepared one.
    pub command: &'static str,
    pub text: String,
}

/// One row `SHOW PROCESSLIST` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlSessionSnapshot {
    pub id: u32,
    pub account: String,
    pub host: String,
    pub database: Option<String>,
    /// The statement it is running, or `None` while it waits for the next.
    pub running: Option<RunningStatement>,
    /// Whole seconds since it last started or finished a command.
    pub seconds: u64,
}

impl Default for MySqlSessionRegistry {
    fn default() -> Self {
        Self {
            opened: Instant::now(),
            sessions: Mutex::new(BTreeMap::new()),
            questions: AtomicU64::new(0),
        }
    }
}

impl MySqlSessionRegistry {
    /// Lists a session that has logged in, until the returned registration is
    /// dropped, in place of any session its ID was given to before.
    pub fn register(
        self: &Arc<Self>,
        id: u32,
        account: String,
        host: String,
        database: Option<String>,
    ) -> MySqlSessionRegistration {
        let activity = Arc::new(Mutex::new(SessionActivity {
            account,
            host,
            database,
            running: None,
            since: Instant::now(),
        }));
        self.lock().insert(id, Arc::clone(&activity));
        MySqlSessionRegistration {
            registry: Arc::clone(self),
            id,
            activity,
        }
    }

    /// How long ago this server opened its databases.
    pub fn uptime(&self) -> Duration {
        self.opened.elapsed()
    }

    /// How many sessions are logged in.
    pub fn logged_in(&self) -> usize {
        self.lock().len()
    }

    /// How many commands and statements every session sent since the server
    /// opened.
    pub fn questions(&self) -> u64 {
        self.questions.load(Ordering::Relaxed)
    }

    /// The sessions of one account, in the order of their IDs.
    pub fn sessions_of(&self, account: &str) -> Vec<MySqlSessionSnapshot> {
        let now = Instant::now();
        self.lock()
            .iter()
            .filter_map(|(id, activity)| {
                let activity = lock_activity(activity);
                (activity.account == account).then(|| MySqlSessionSnapshot {
                    id: *id,
                    account: activity.account.clone(),
                    host: activity.host.clone(),
                    database: activity.database.clone(),
                    running: activity.running.clone(),
                    seconds: now.saturating_duration_since(activity.since).as_secs(),
                })
            })
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<u32, Arc<Mutex<SessionActivity>>>> {
        self.sessions
            .lock()
            .expect("the session registry is never left half-changed")
    }
}

/// One session's place in the registry, which it leaves when this is dropped.
pub struct MySqlSessionRegistration {
    registry: Arc<MySqlSessionRegistry>,
    id: u32,
    activity: Arc<Mutex<SessionActivity>>,
}

impl MySqlSessionRegistration {
    /// Notes that a command arrived, which restarts the session's clock.
    pub fn command_arrived(&self) {
        lock_activity(&self.activity).since = Instant::now();
    }

    /// Counts one command, or one more statement of a query holding several.
    pub fn question_asked(&self) {
        self.registry.questions.fetch_add(1, Ordering::Relaxed);
    }

    /// Notes that the session began running `statement`.
    pub fn statement_began(&self, statement: RunningStatement) {
        let mut activity = lock_activity(&self.activity);
        activity.running = Some(statement);
        activity.since = Instant::now();
    }

    /// Notes that the session finished a statement, and which database it
    /// has selected afterwards.
    pub fn statement_ended(&self, database: Option<&str>) {
        let mut activity = lock_activity(&self.activity);
        activity.running = None;
        if activity.database.as_deref() != database {
            activity.database = database.map(str::to_owned);
        }
        activity.since = Instant::now();
    }

    /// Notes the database the session selected.
    pub fn database_selected(&self, database: &str) {
        lock_activity(&self.activity).database = Some(database.to_owned());
    }
}

impl Drop for MySqlSessionRegistration {
    fn drop(&mut self) {
        let mut sessions = self.registry.lock();
        if sessions
            .get(&self.id)
            .is_some_and(|activity| Arc::ptr_eq(activity, &self.activity))
        {
            sessions.remove(&self.id);
        }
    }
}

fn lock_activity(activity: &Mutex<SessionActivity>) -> MutexGuard<'_, SessionActivity> {
    activity
        .lock()
        .expect("a session's activity is never left half-changed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_listed_until_its_registration_is_dropped() {
        let registry = Arc::new(MySqlSessionRegistry::default());
        let first = registry.register(3, "app".into(), "localhost".into(), None);
        let other = registry.register(4, "reader".into(), "localhost".into(), None);
        first.statement_began(RunningStatement {
            command: "Query",
            text: "SELECT 1".into(),
        });
        let listed = registry.sessions_of("app");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, 3);
        assert_eq!(
            listed[0]
                .running
                .as_ref()
                .map(|running| running.text.as_str()),
            Some("SELECT 1")
        );
        first.statement_ended(Some("reports"));
        assert_eq!(
            registry.sessions_of("app")[0].database.as_deref(),
            Some("reports")
        );
        assert_eq!(registry.logged_in(), 2);
        drop(other);
        drop(first);
        assert_eq!(registry.logged_in(), 0);
    }

    /// The listener lets a connection's ID go when its stream closes, which
    /// can come before its session is dropped.
    #[test]
    fn a_registration_outliving_its_id_leaves_the_next_session_alone() {
        let registry = Arc::new(MySqlSessionRegistry::default());
        let stale = registry.register(5, "app".into(), "localhost".into(), None);
        let next = registry.register(5, "app".into(), "localhost".into(), Some("db".into()));
        stale.statement_began(RunningStatement {
            command: "Query",
            text: "SELECT 1".into(),
        });
        drop(stale);
        let listed = registry.sessions_of("app");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].running, None);
        assert_eq!(listed[0].database.as_deref(), Some("db"));
        drop(next);
        assert_eq!(registry.logged_in(), 0);
    }
}
