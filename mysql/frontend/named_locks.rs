//! MySQL's named locks, the ones `GET_LOCK` takes.
//!
//! They belong to no database and no transaction: a session takes one by
//! name, holds it across commits and rollbacks, and lets it go by name, all at
//! once, or by closing. Rails, Prisma and Flyway each take one around a
//! migration so that two processes do not migrate at the same time.
//!
//! Measured on MySQL 8.4.11, and kept here: names are matched whatever their
//! case; a session may take a lock it holds again and must let it go as many
//! times; and a session that would wait for a lock held by one already
//! waiting on it is told so at once rather than left to time out.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Every named lock the sessions of one server hold.
#[derive(Debug, Default)]
pub struct MySqlNamedLocks {
    state: Mutex<LockTable>,
    released: Condvar,
    next_holder: AtomicU64,
}

#[derive(Debug, Default)]
struct LockTable {
    /// Who holds each lock, by its name folded to lowercase.
    held: HashMap<String, Held>,
    /// The lock each waiting session waits for.
    waiting: HashMap<u64, String>,
}

#[derive(Debug, Clone, Copy)]
struct Held {
    holder: u64,
    /// How many times the holder took it, each of which it lets go of once.
    times: u64,
}

/// How long `GET_LOCK` waits for a lock another session holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlLockWait {
    For(Duration),
    /// A negative timeout, which MySQL waits on without end.
    Forever,
}

/// Why a session could not take a lock it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlNamedLockError {
    /// The session holding it waits, directly or not, for one this session
    /// holds, so neither would ever get what it waits for.
    Deadlock,
}

impl MySqlNamedLocks {
    /// Opens a session's hold on these locks, which lets go of every lock it
    /// took when it is dropped.
    pub fn session(self: &Arc<Self>) -> MySqlNamedLockSession {
        MySqlNamedLockSession {
            locks: self.clone(),
            holder: self.next_holder.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// One session's hold on the server's named locks.
#[derive(Debug)]
pub struct MySqlNamedLockSession {
    locks: Arc<MySqlNamedLocks>,
    holder: u64,
}

impl MySqlNamedLockSession {
    /// Takes the named lock, waiting as long as `wait` says for another
    /// session to let go of it. Answers whether it was taken.
    pub fn get(&self, name: &str, wait: MySqlLockWait) -> Result<bool, MySqlNamedLockError> {
        let key = folded(name);
        let deadline = match wait {
            MySqlLockWait::For(wait) => Some(Instant::now() + wait),
            MySqlLockWait::Forever => None,
        };
        let mut table = self.locks.state.lock().unwrap();
        loop {
            match table.held.get(&key).map(|held| held.holder) {
                None => {
                    table.held.insert(
                        key,
                        Held {
                            holder: self.holder,
                            times: 1,
                        },
                    );
                    table.waiting.remove(&self.holder);
                    return Ok(true);
                }
                Some(holder) if holder == self.holder => {
                    table
                        .held
                        .get_mut(&key)
                        .expect("the lock was just found held")
                        .times += 1;
                    table.waiting.remove(&self.holder);
                    return Ok(true);
                }
                Some(holder) => {
                    if table.waits_for(holder, self.holder) {
                        table.waiting.remove(&self.holder);
                        return Err(MySqlNamedLockError::Deadlock);
                    }
                }
            }
            let remaining = match deadline {
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(remaining) if !remaining.is_zero() => Some(remaining),
                    _ => {
                        table.waiting.remove(&self.holder);
                        return Ok(false);
                    }
                },
                None => None,
            };
            table.waiting.insert(self.holder, key.clone());
            table = match remaining {
                Some(remaining) => {
                    self.locks
                        .released
                        .wait_timeout(table, remaining)
                        .unwrap()
                        .0
                }
                None => self.locks.released.wait(table).unwrap(),
            };
        }
    }

    /// Lets go of the named lock once. Answers `None` when no session holds
    /// it, and `Some(false)` when another session does.
    pub fn release(&self, name: &str) -> Option<bool> {
        let key = folded(name);
        let mut table = self.locks.state.lock().unwrap();
        let held = table.held.get_mut(&key)?;
        if held.holder != self.holder {
            return Some(false);
        }
        held.times -= 1;
        if held.times == 0 {
            table.held.remove(&key);
            self.locks.released.notify_all();
        }
        Some(true)
    }

    /// Lets go of every lock this session holds, answering how many times it
    /// had taken them.
    pub fn release_all(&self) -> u64 {
        let mut table = self.locks.state.lock().unwrap();
        let mut released = 0;
        table.held.retain(|_, held| {
            if held.holder != self.holder {
                return true;
            }
            released += held.times;
            false
        });
        if released > 0 {
            self.locks.released.notify_all();
        }
        released
    }

    /// Answers whether no session holds the named lock.
    pub fn is_free(&self, name: &str) -> bool {
        !self
            .locks
            .state
            .lock()
            .unwrap()
            .held
            .contains_key(&folded(name))
    }
}

impl Drop for MySqlNamedLockSession {
    fn drop(&mut self) {
        self.release_all();
        self.locks
            .state
            .lock()
            .unwrap()
            .waiting
            .remove(&self.holder);
    }
}

impl LockTable {
    /// Whether `from` waits, directly or through other waiting sessions, for
    /// a lock `target` holds.
    fn waits_for(&self, from: u64, target: u64) -> bool {
        let mut current = from;
        // Each step moves to the holder of the lock the current session waits
        // for; a chain longer than the waiting sessions has gone round.
        for _ in 0..=self.waiting.len() {
            if current == target {
                return true;
            }
            let Some(name) = self.waiting.get(&current) else {
                return false;
            };
            let Some(held) = self.held.get(name) else {
                return false;
            };
            current = held.holder;
        }
        false
    }
}

fn folded(name: &str) -> String {
    name.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn a_lock_is_held_by_one_session_and_taken_again_by_it() {
        let locks = Arc::new(MySqlNamedLocks::default());
        let one = locks.session();
        let two = locks.session();
        let now = MySqlLockWait::For(Duration::ZERO);

        assert_eq!(one.get("prisma_migrate", now), Ok(true));
        assert_eq!(one.get("prisma_migrate", now), Ok(true));
        // Measured on MySQL 8.4.11: names are matched whatever their case.
        assert_eq!(two.get("PRISMA_MIGRATE", now), Ok(false));
        assert!(!two.is_free("prisma_migrate"));
        assert_eq!(two.release("prisma_migrate"), Some(false));
        assert_eq!(two.release("nothing"), None);

        // Taken twice, so let go of twice.
        assert_eq!(one.release("prisma_migrate"), Some(true));
        assert_eq!(two.get("prisma_migrate", now), Ok(false));
        assert_eq!(one.release("prisma_migrate"), Some(true));
        assert_eq!(two.get("prisma_migrate", now), Ok(true));
    }

    #[test]
    fn letting_go_of_everything_counts_each_time_a_lock_was_taken() {
        let locks = Arc::new(MySqlNamedLocks::default());
        let one = locks.session();
        let now = MySqlLockWait::For(Duration::ZERO);
        assert_eq!(one.get("a", now), Ok(true));
        assert_eq!(one.get("a", now), Ok(true));
        assert_eq!(one.get("b", now), Ok(true));
        assert_eq!(one.release_all(), 3);
        assert!(one.is_free("a"));
    }

    #[test]
    fn a_closed_session_lets_go_of_its_locks() {
        let locks = Arc::new(MySqlNamedLocks::default());
        let now = MySqlLockWait::For(Duration::ZERO);
        let one = locks.session();
        assert_eq!(one.get("m", now), Ok(true));
        drop(one);
        assert_eq!(locks.session().get("m", now), Ok(true));
    }

    #[test]
    fn a_waiting_session_takes_the_lock_once_it_is_let_go() {
        let locks = Arc::new(MySqlNamedLocks::default());
        let one = locks.session();
        assert_eq!(one.get("m", MySqlLockWait::For(Duration::ZERO)), Ok(true));
        let waiter = {
            let locks = locks.clone();
            thread::spawn(move || {
                locks
                    .session()
                    .get("m", MySqlLockWait::For(Duration::from_secs(10)))
            })
        };
        while locks.state.lock().unwrap().waiting.is_empty() {
            thread::yield_now();
        }
        assert_eq!(one.release("m"), Some(true));
        assert_eq!(waiter.join().unwrap(), Ok(true));
    }

    #[test]
    fn a_wait_that_runs_out_answers_that_the_lock_was_not_taken() {
        let locks = Arc::new(MySqlNamedLocks::default());
        let one = locks.session();
        let two = locks.session();
        assert_eq!(one.get("m", MySqlLockWait::For(Duration::ZERO)), Ok(true));
        let started = Instant::now();
        assert_eq!(
            two.get("m", MySqlLockWait::For(Duration::from_millis(50))),
            Ok(false)
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(locks.state.lock().unwrap().waiting.is_empty());
    }

    /// Measured on MySQL 8.4.11: with one session holding `x` and waiting for
    /// `w`, the other, holding `w`, asking for `x` is told at once, and keeps
    /// the lock it holds.
    #[test]
    fn a_wait_that_could_never_end_is_refused_at_once() {
        let locks = Arc::new(MySqlNamedLocks::default());
        let one = locks.session();
        let two = locks.session();
        let now = MySqlLockWait::For(Duration::ZERO);
        assert_eq!(two.get("w", now), Ok(true));
        assert_eq!(one.get("x", now), Ok(true));
        let waiter = {
            let locks = locks.clone();
            thread::spawn(move || {
                let two = two;
                let got = two.get("x", MySqlLockWait::Forever);
                (got, locks)
            })
        };
        while locks.state.lock().unwrap().waiting.is_empty() {
            thread::yield_now();
        }
        assert_eq!(
            one.get("w", MySqlLockWait::Forever),
            Err(MySqlNamedLockError::Deadlock)
        );
        assert!(!one.is_free("x"));
        assert_eq!(one.release_all(), 1);
        assert_eq!(waiter.join().unwrap().0, Ok(true));
    }
}
