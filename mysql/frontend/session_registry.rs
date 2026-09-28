//! The sessions logged in to one server, which `SHOW PROCESSLIST` lists and
//! `SHOW STATUS` counts.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Every session logged in to one server, and when the server opened.
pub struct MySqlSessionRegistry {
    opened: Instant,
    sessions: Mutex<BTreeMap<u32, SessionActivity>>,
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
        }
    }
}

impl MySqlSessionRegistry {
    /// Lists a session that has logged in, until the returned registration is
    /// dropped.
    ///
    /// Connection IDs are unique among the connections one server holds, so
    /// the ID cannot already be listed.
    pub fn register(
        self: &Arc<Self>,
        id: u32,
        account: String,
        host: String,
        database: Option<String>,
    ) -> MySqlSessionRegistration {
        let replaced = self.lock().insert(
            id,
            SessionActivity {
                account,
                host,
                database,
                running: None,
                since: Instant::now(),
            },
        );
        assert!(replaced.is_none(), "connection {id} is registered once");
        MySqlSessionRegistration {
            registry: Arc::clone(self),
            id,
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

    /// The sessions of one account, in the order of their IDs.
    pub fn sessions_of(&self, account: &str) -> Vec<MySqlSessionSnapshot> {
        let now = Instant::now();
        self.lock()
            .iter()
            .filter(|(_, activity)| activity.account == account)
            .map(|(id, activity)| MySqlSessionSnapshot {
                id: *id,
                account: activity.account.clone(),
                host: activity.host.clone(),
                database: activity.database.clone(),
                running: activity.running.clone(),
                seconds: now.saturating_duration_since(activity.since).as_secs(),
            })
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<u32, SessionActivity>> {
        self.sessions
            .lock()
            .expect("the session registry is never left half-changed")
    }

    fn update(&self, id: u32, change: impl FnOnce(&mut SessionActivity)) {
        let mut sessions = self.lock();
        let activity = sessions
            .get_mut(&id)
            .expect("a registration's session stays listed until it is dropped");
        change(activity);
    }
}

/// One session's place in the registry, which it leaves when this is dropped.
pub struct MySqlSessionRegistration {
    registry: Arc<MySqlSessionRegistry>,
    id: u32,
}

impl MySqlSessionRegistration {
    /// Notes that a command arrived, which restarts the session's clock.
    pub fn command_arrived(&self) {
        self.registry
            .update(self.id, |activity| activity.since = Instant::now());
    }

    /// Notes that the session began running `statement`.
    pub fn statement_began(&self, statement: RunningStatement) {
        self.registry.update(self.id, |activity| {
            activity.running = Some(statement);
            activity.since = Instant::now();
        });
    }

    /// Notes that the session finished a statement, and which database it
    /// has selected afterwards.
    pub fn statement_ended(&self, database: Option<&str>) {
        self.registry.update(self.id, |activity| {
            activity.running = None;
            activity.database = database.map(str::to_owned);
            activity.since = Instant::now();
        });
    }

    /// Notes the database the session selected.
    pub fn database_selected(&self, database: &str) {
        self.registry.update(self.id, |activity| {
            activity.database = Some(database.to_owned());
        });
    }
}

impl Drop for MySqlSessionRegistration {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.id);
    }
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
}
