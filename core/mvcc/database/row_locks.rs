use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::{RowID, TxID};
use crate::storage::lock_release::LockReleaseSignal;
use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLockMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLockWaitPolicy {
    Wait,
    NoWait,
    SkipLocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockingRead {
    pub mode: RowLockMode,
    pub policy: RowLockWaitPolicy,
    pub tables: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowLockWaitEnd {
    HoldersEnded,
    TimedOut,
    ChosenAsDeadlockVictim,
    Interrupted,
}

#[derive(Default)]
pub(crate) struct RowLocks {
    enabled: AtomicBool,
    table: Mutex<LockTable>,
    transaction_ended: LockReleaseSignal,
}

impl std::fmt::Debug for RowLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowLocks")
            .field("enabled", &self.enabled())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct LockTable {
    rows: BTreeMap<RowID, Holders>,
    unique_keys: BTreeMap<RowID, TxID>,
    held: HashMap<TxID, Vec<HeldLock>>,
    waits_for: HashMap<TxID, Vec<TxID>>,
    deadlock_victims: HashSet<TxID>,
}

enum HeldLock {
    Row(RowID),
    UniqueKey(RowID),
}

#[derive(Default)]
struct Holders {
    exclusive: Option<TxID>,
    shared: Vec<TxID>,
}

const LONGEST_SLEEP_BETWEEN_CHECKS: Duration = Duration::from_millis(100);

impl RowLocks {
    pub(crate) fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn lock(&self, tx_id: TxID, row: &RowID, mode: RowLockMode) -> Vec<TxID> {
        let mut table = self.table.lock();
        let holders = table.rows.entry(row.clone()).or_default();
        let conflicting = holders.others_in_the_way(tx_id, mode);
        if !conflicting.is_empty() {
            return conflicting;
        }
        if holders.add(tx_id, mode) {
            table
                .held
                .entry(tx_id)
                .or_default()
                .push(HeldLock::Row(row.clone()));
        }
        conflicting
    }

    pub(crate) fn holders_in_the_way_of_a_write(&self, tx_id: TxID, row: &RowID) -> Vec<TxID> {
        let table = self.table.lock();
        table
            .rows
            .get(row)
            .map(|holders| holders.others_in_the_way(tx_id, RowLockMode::Exclusive))
            .unwrap_or_default()
    }

    pub(crate) fn lock_unique_key(&self, tx_id: TxID, key: &RowID) -> Option<TxID> {
        let mut table = self.table.lock();
        match table.unique_keys.get(key) {
            Some(holder) if *holder == tx_id => None,
            Some(holder) => Some(*holder),
            None => {
                table.unique_keys.insert(key.clone(), tx_id);
                table
                    .held
                    .entry(tx_id)
                    .or_default()
                    .push(HeldLock::UniqueKey(key.clone()));
                None
            }
        }
    }

    pub(crate) fn release(&self, tx_id: TxID) {
        let mut table = self.table.lock();
        for lock in table.held.remove(&tx_id).unwrap_or_default() {
            match lock {
                HeldLock::Row(row) => {
                    let now_free = table.rows.get_mut(&row).is_some_and(|holders| {
                        holders.remove(tx_id);
                        holders.is_empty()
                    });
                    if now_free {
                        table.rows.remove(&row);
                    }
                }
                HeldLock::UniqueKey(key) => {
                    table.unique_keys.remove(&key);
                }
            }
        }
        table.waits_for.remove(&tx_id);
        table.deadlock_victims.remove(&tx_id);
        drop(table);
        self.transaction_ended.released();
    }

    pub(crate) fn start_waiting(
        &self,
        waiter: TxID,
        holders: &[TxID],
        written: impl Fn(TxID) -> u64,
    ) -> Option<TxID> {
        let mut table = self.table.lock();
        let cycle = table.cycle_through(waiter, holders);
        if cycle.is_empty() {
            table.waits_for.insert(waiter, holders.to_vec());
            return None;
        }
        let weight =
            |tx_id: TxID| written(tx_id) + table.held.get(&tx_id).map_or(0, Vec::len) as u64;
        let waiter_weight = weight(waiter);
        let lightest = cycle
            .iter()
            .copied()
            .filter(|member| *member != waiter)
            .map(|member| (weight(member), member))
            .min();
        let victim = match lightest {
            Some((other_weight, other)) if other_weight < waiter_weight => other,
            _ => waiter,
        };
        if victim == waiter {
            return Some(victim);
        }
        table.waits_for.insert(waiter, holders.to_vec());
        table.deadlock_victims.insert(victim);
        drop(table);
        self.transaction_ended.released();
        Some(victim)
    }

    pub(crate) fn stop_waiting(&self, waiter: TxID) {
        let mut table = self.table.lock();
        table.waits_for.remove(&waiter);
        table.deadlock_victims.remove(&waiter);
    }

    pub(crate) fn wait(
        &self,
        waiter: Option<TxID>,
        deadline: Instant,
        holders_alive: impl Fn() -> bool,
        interrupted: impl Fn() -> bool,
    ) -> RowLockWaitEnd {
        loop {
            let seen = self.transaction_ended.releases();
            if waiter.is_some_and(|waiter| self.chosen_as_deadlock_victim(waiter)) {
                return RowLockWaitEnd::ChosenAsDeadlockVictim;
            }
            if !holders_alive() {
                return RowLockWaitEnd::HoldersEnded;
            }
            if interrupted() {
                return RowLockWaitEnd::Interrupted;
            }
            let now = Instant::now();
            if now >= deadline {
                return RowLockWaitEnd::TimedOut;
            }
            let sleep = (deadline - now).min(LONGEST_SLEEP_BETWEEN_CHECKS);
            if self
                .transaction_ended
                .wait_for_release_after(seen, sleep)
                .is_none()
            {
                crate::thread::yield_now();
            }
        }
    }

    fn chosen_as_deadlock_victim(&self, tx_id: TxID) -> bool {
        self.table.lock().deadlock_victims.contains(&tx_id)
    }
}

impl LockTable {
    fn cycle_through(&self, waiter: TxID, holders: &[TxID]) -> Vec<TxID> {
        let mut visited = HashSet::default();
        let mut path = Vec::new();
        for holder in holders {
            if self.reaches(*holder, waiter, &mut visited, &mut path) {
                return path;
            }
        }
        Vec::new()
    }

    fn reaches(
        &self,
        from: TxID,
        target: TxID,
        visited: &mut HashSet<TxID>,
        path: &mut Vec<TxID>,
    ) -> bool {
        if from == target {
            return true;
        }
        if !visited.insert(from) {
            return false;
        }
        path.push(from);
        for next in self.waits_for.get(&from).into_iter().flatten() {
            if self.reaches(*next, target, visited, path) {
                return true;
            }
        }
        path.pop();
        false
    }
}

impl Holders {
    fn others_in_the_way(&self, tx_id: TxID, mode: RowLockMode) -> Vec<TxID> {
        if let Some(holder) = self.exclusive {
            return if holder == tx_id {
                Vec::new()
            } else {
                vec![holder]
            };
        }
        match mode {
            RowLockMode::Shared => Vec::new(),
            RowLockMode::Exclusive => self
                .shared
                .iter()
                .copied()
                .filter(|holder| *holder != tx_id)
                .collect(),
        }
    }

    fn add(&mut self, tx_id: TxID, mode: RowLockMode) -> bool {
        let already_held = self.exclusive == Some(tx_id) || self.shared.contains(&tx_id);
        match mode {
            RowLockMode::Exclusive => {
                self.shared.retain(|holder| *holder != tx_id);
                self.exclusive = Some(tx_id);
            }
            RowLockMode::Shared if !already_held => self.shared.push(tx_id),
            RowLockMode::Shared => {}
        }
        !already_held
    }

    fn remove(&mut self, tx_id: TxID) {
        if self.exclusive == Some(tx_id) {
            self.exclusive = None;
        }
        self.shared.retain(|holder| *holder != tx_id);
    }

    fn is_empty(&self) -> bool {
        self.exclusive.is_none() && self.shared.is_empty()
    }
}
