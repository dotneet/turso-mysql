use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::{MVTableId, RowID, RowKey, TxID};
use crate::storage::lock_release::LockReleaseSignal;
use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLockMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowLockLevel {
    ReadCommitted,
    #[default]
    RepeatableRead,
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
pub(crate) enum WaitKind {
    RowLock,
    MetadataLock,
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
    inserted: BTreeMap<RowID, TxID>,
    gaps: BTreeMap<GapEnd, Vec<Gap>>,
    held: HashMap<TxID, Vec<HeldLock>>,
    released_before_the_end: HashMap<TxID, u64>,
    waits_for: HashMap<TxID, (WaitKind, Vec<TxID>)>,
    deadlock_victims: HashSet<TxID>,
}

enum HeldLock {
    Row(RowID),
    UniqueKey(RowID),
    Inserted(RowID),
    Gap(GapEnd),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GapEnd {
    table_id: MVTableId,
    high: Option<RowKey>,
}

impl Ord for GapEnd {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.table_id
            .cmp(&other.table_id)
            .then_with(|| match (&self.high, &other.high) {
                (Some(mine), Some(theirs)) => mine.cmp(theirs),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    }
}

impl PartialOrd for GapEnd {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct Gap {
    low: Option<RowKey>,
    holder: TxID,
    found: FoundBounds,
}

#[derive(Clone)]
struct FoundBounds {
    low: Option<RowKey>,
    high: Option<RowKey>,
}

impl Gap {
    fn starts_below(&self, key: &RowKey) -> bool {
        self.low.as_ref().is_none_or(|low| low < key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowLockAttempt {
    Locked { newly_held: bool },
    HeldBy(Vec<TxID>),
    ChangedSinceRead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GapKey {
    Met,
    BelowTheScan,
    AboveTheScan,
    Ignored,
}

pub(crate) struct GapBetween {
    pub(crate) table_id: MVTableId,
    pub(crate) low: Option<RowKey>,
    pub(crate) high: Option<RowKey>,
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

    pub(crate) fn lock(
        &self,
        tx_id: TxID,
        row: &RowID,
        mode: RowLockMode,
    ) -> Result<bool, Vec<TxID>> {
        let mut table = self.table.lock();
        let holders = table.rows.entry(row.clone()).or_default();
        let conflicting = holders.others_in_the_way(tx_id, mode);
        if !conflicting.is_empty() {
            if holders.is_empty() {
                table.rows.remove(row);
            }
            return Err(conflicting);
        }
        let newly_held = holders.add(tx_id, mode);
        if newly_held {
            table
                .held
                .entry(tx_id)
                .or_default()
                .push(HeldLock::Row(row.clone()));
        }
        Ok(newly_held)
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

    pub(crate) fn unlock(&self, tx_id: TxID, row: &RowID) {
        let mut table = self.table.lock();
        let Some(holders) = table.rows.get_mut(row) else {
            return;
        };
        if !holders.holds(tx_id) {
            return;
        }
        holders.remove(tx_id);
        if holders.is_empty() {
            table.rows.remove(row);
        }
        if let Some(held) = table.held.get_mut(&tx_id) {
            if let Some(position) = held
                .iter()
                .rposition(|lock| matches!(lock, HeldLock::Row(held_row) if held_row == row))
            {
                held.swap_remove(position);
            }
        }
        *table.released_before_the_end.entry(tx_id).or_default() += 1;
        drop(table);
        self.transaction_ended.released();
    }

    pub(crate) fn releases_before_the_end(&self, holders: &[TxID]) -> u64 {
        let table = self.table.lock();
        holders
            .iter()
            .map(|holder| {
                table
                    .released_before_the_end
                    .get(holder)
                    .copied()
                    .unwrap_or(0)
            })
            .sum()
    }

    pub(crate) fn lock_insert(&self, tx_id: TxID, key: &RowID) -> Vec<TxID> {
        let mut table = self.table.lock();
        let holders = table.gap_holders_around(tx_id, key);
        if !holders.is_empty() {
            return holders;
        }
        if !table.inserted.contains_key(key) {
            table.inserted.insert(key.clone(), tx_id);
            table
                .held
                .entry(tx_id)
                .or_default()
                .push(HeldLock::Inserted(key.clone()));
        }
        holders
    }

    pub(crate) fn forget_inserts(&self, tx_id: TxID, keys: &[RowID]) {
        let mut table = self.table.lock();
        let forgotten: Vec<&RowID> = keys
            .iter()
            .filter(|key| table.inserted.get(*key) == Some(&tx_id))
            .collect();
        if forgotten.is_empty() {
            return;
        }
        for key in &forgotten {
            table.inserted.remove(*key);
        }
        for key in &forgotten {
            table.widen_the_gaps_an_undone_insert_bounded(key);
        }
        if let Some(held) = table.held.get_mut(&tx_id) {
            held.retain(
                |lock| !matches!(lock, HeldLock::Inserted(key) if forgotten.contains(&key)),
            );
        }
        *table.released_before_the_end.entry(tx_id).or_default() += 1;
        drop(table);
        self.transaction_ended.released();
    }

    pub(crate) fn lock_gap(
        &self,
        tx_id: TxID,
        gap: GapBetween,
        place: impl Fn(&RowKey) -> GapKey,
        keeps_the_gap: bool,
    ) -> Vec<TxID> {
        let mut table = self.table.lock();
        let found = FoundBounds {
            low: gap.low.clone(),
            high: gap.high.clone(),
        };
        let mut low = gap.low;
        let mut high = gap.high;
        let mut met = Vec::new();
        for (key, inserter) in table.inserted_between(gap.table_id, low.clone(), high.clone()) {
            if inserter == tx_id {
                continue;
            }
            match place(&key.row_id) {
                GapKey::Met => {
                    if !met.contains(&inserter) {
                        met.push(inserter);
                    }
                }
                GapKey::BelowTheScan => low = Some(key.row_id.clone()),
                GapKey::AboveTheScan => {
                    if high.as_ref().is_none_or(|high| &key.row_id < high) {
                        high = Some(key.row_id.clone());
                    }
                }
                GapKey::Ignored => {}
            }
        }
        if !met.is_empty() || !keeps_the_gap {
            return met;
        }
        table.add_gap(tx_id, gap.table_id, low, high, found);
        met
    }

    pub(crate) fn release(&self, tx_id: TxID, committed: bool) {
        let mut table = self.table.lock();
        let mut undone_inserts = Vec::new();
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
                HeldLock::Inserted(key) => {
                    if table.inserted.get(&key) == Some(&tx_id) {
                        table.inserted.remove(&key);
                        undone_inserts.push(key);
                    }
                }
                HeldLock::Gap(end) => {
                    table.remove_gap(tx_id, &end);
                }
            }
        }
        if !committed {
            for key in &undone_inserts {
                table.widen_the_gaps_an_undone_insert_bounded(key);
            }
        }
        table.released_before_the_end.remove(&tx_id);
        table.waits_for.remove(&tx_id);
        table.deadlock_victims.remove(&tx_id);
        drop(table);
        self.transaction_ended.released();
    }

    pub(crate) fn start_waiting(
        &self,
        waiter: TxID,
        kind: WaitKind,
        holders: &[TxID],
        weight: impl Fn(TxID) -> u64,
    ) -> Option<TxID> {
        let mut table = self.table.lock();
        let cycle = table.cycle_through(waiter, kind, holders);
        if cycle.is_empty() {
            table.waits_for.insert(waiter, (kind, holders.to_vec()));
            return None;
        }
        let victim = match kind {
            WaitKind::RowLock => table.lightest_by_rows(waiter, &cycle, weight),
            WaitKind::MetadataLock => std::iter::once(waiter)
                .chain(cycle.iter().copied())
                .min_by_key(|member| weight(*member))
                .expect("a cycle has the waiter in it"),
        };
        if victim == waiter {
            return Some(victim);
        }
        table.waits_for.insert(waiter, (kind, holders.to_vec()));
        table.deadlock_victims.insert(victim);
        drop(table);
        self.transaction_ended.released();
        Some(victim)
    }

    pub(crate) fn wake_waiters(&self) {
        self.transaction_ended.released();
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
    fn inserted_between(
        &self,
        table_id: MVTableId,
        low: Option<RowKey>,
        high: Option<RowKey>,
    ) -> Vec<(RowID, TxID)> {
        let start = match low {
            Some(low) => Bound::Excluded(RowID::new(table_id, low)),
            None => Bound::Included(RowID::new(table_id, RowKey::Int(i64::MIN))),
        };
        self.inserted
            .range((start, Bound::Unbounded))
            .take_while(|(key, _)| {
                key.table_id == table_id && high.as_ref().is_none_or(|high| &key.row_id < high)
            })
            .map(|(key, inserter)| (key.clone(), *inserter))
            .collect()
    }

    fn gap_holders_around(&self, tx_id: TxID, key: &RowID) -> Vec<TxID> {
        let after_the_key = GapEnd {
            table_id: key.table_id,
            high: Some(key.row_id.clone()),
        };
        let mut holders = Vec::new();
        for (end, gaps) in self
            .gaps
            .range((Bound::Excluded(after_the_key), Bound::Unbounded))
        {
            if end.table_id != key.table_id {
                break;
            }
            for gap in gaps {
                if gap.holder != tx_id
                    && gap.starts_below(&key.row_id)
                    && !holders.contains(&gap.holder)
                {
                    holders.push(gap.holder);
                }
            }
        }
        holders
    }

    fn add_gap(
        &mut self,
        tx_id: TxID,
        table_id: MVTableId,
        low: Option<RowKey>,
        high: Option<RowKey>,
        found: FoundBounds,
    ) {
        let (low, found) = self.join_the_gap_below(tx_id, table_id, low, found);
        self.put_gap(
            GapEnd { table_id, high },
            Gap {
                low,
                holder: tx_id,
                found,
            },
        );
    }

    fn put_gap(&mut self, end: GapEnd, gap: Gap) {
        let gaps = self.gaps.entry(end.clone()).or_default();
        if let Some(held) = gaps.iter_mut().find(|held| held.holder == gap.holder) {
            if lower(&gap.low, &held.low) {
                held.low = gap.low;
            }
            if lower(&gap.found.low, &held.found.low) {
                held.found.low = gap.found.low;
            }
            return;
        }
        let holder = gap.holder;
        gaps.push(gap);
        self.held
            .entry(holder)
            .or_default()
            .push(HeldLock::Gap(end));
    }

    fn widen_the_gaps_an_undone_insert_bounded(&mut self, key: &RowID) {
        let ended_at_it = GapEnd {
            table_id: key.table_id,
            high: Some(key.row_id.clone()),
        };
        for gap in self.gaps.remove(&ended_at_it).unwrap_or_default() {
            let high = self
                .inserted_between(
                    key.table_id,
                    Some(key.row_id.clone()),
                    gap.found.high.clone(),
                )
                .first()
                .map(|(next, _)| next.row_id.clone())
                .or_else(|| gap.found.high.clone());
            let holder = gap.holder;
            let end = GapEnd {
                table_id: key.table_id,
                high,
            };
            if let Some(held) = self.held.get_mut(&holder) {
                held.retain(
                    |lock| !matches!(lock, HeldLock::Gap(held_end) if *held_end == ended_at_it),
                );
            }
            self.put_gap(end, gap);
        }
        let starts_at_it = Some(key.row_id.clone());
        let mut lows = Vec::new();
        for (end, gaps) in self
            .gaps
            .range((Bound::Excluded(ended_at_it), Bound::Unbounded))
        {
            if end.table_id != key.table_id {
                break;
            }
            for gap in gaps.iter().filter(|gap| gap.low == starts_at_it) {
                lows.push((end.clone(), gap.holder, gap.found.low.clone()));
            }
        }
        for (end, holder, found_low) in lows {
            let low = self
                .inserted_between(key.table_id, found_low.clone(), starts_at_it.clone())
                .last()
                .map(|(below, _)| below.row_id.clone())
                .or(found_low);
            if let Some(gap) = self
                .gaps
                .get_mut(&end)
                .and_then(|gaps| gaps.iter_mut().find(|gap| gap.holder == holder))
            {
                gap.low = low;
            }
        }
    }

    fn join_the_gap_below(
        &mut self,
        tx_id: TxID,
        table_id: MVTableId,
        low: Option<RowKey>,
        found: FoundBounds,
    ) -> (Option<RowKey>, FoundBounds) {
        let Some(record) = low else {
            return (None, found);
        };
        let record_id = RowID::new(table_id, record.clone());
        let holds_the_record = self.rows.get(&record_id).is_some_and(|holders| {
            holders.exclusive == Some(tx_id) || holders.shared.contains(&tx_id)
        });
        if !holds_the_record {
            return (Some(record), found);
        }
        let below = GapEnd {
            table_id,
            high: Some(record.clone()),
        };
        let Some(joined) = self.remove_gap(tx_id, &below) else {
            return (Some(record), found);
        };
        if let Some(held) = self.held.get_mut(&tx_id) {
            if let Some(position) = held
                .iter()
                .rposition(|lock| matches!(lock, HeldLock::Gap(end) if *end == below))
            {
                held.swap_remove(position);
            }
        }
        (
            joined.low,
            FoundBounds {
                low: joined.found.low,
                high: found.high,
            },
        )
    }

    fn remove_gap(&mut self, tx_id: TxID, end: &GapEnd) -> Option<Gap> {
        let gaps = self.gaps.get_mut(end)?;
        let position = gaps.iter().position(|gap| gap.holder == tx_id)?;
        let removed = gaps.swap_remove(position);
        if gaps.is_empty() {
            self.gaps.remove(end);
        }
        Some(removed)
    }

    fn lightest_by_rows(
        &self,
        waiter: TxID,
        cycle: &[TxID],
        written: impl Fn(TxID) -> u64,
    ) -> TxID {
        let weight =
            |tx_id: TxID| written(tx_id) + self.held.get(&tx_id).map_or(0, Vec::len) as u64;
        let waiter_weight = weight(waiter);
        let lightest = cycle
            .iter()
            .copied()
            .filter(|member| *member != waiter)
            .map(|member| (weight(member), member))
            .min();
        match lightest {
            Some((other_weight, other)) if other_weight < waiter_weight => other,
            _ => waiter,
        }
    }

    fn cycle_through(&self, waiter: TxID, kind: WaitKind, holders: &[TxID]) -> Vec<TxID> {
        let mut visited = HashSet::default();
        let mut path = Vec::new();
        for holder in holders {
            if self.reaches(*holder, waiter, kind, &mut visited, &mut path) {
                return path;
            }
        }
        Vec::new()
    }

    fn reaches(
        &self,
        from: TxID,
        target: TxID,
        kind: WaitKind,
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
        if let Some((waits_on, holders)) = self.waits_for.get(&from) {
            if *waits_on == kind {
                for next in holders {
                    if self.reaches(*next, target, kind, visited, path) {
                        return true;
                    }
                }
            }
        }
        path.pop();
        false
    }
}

fn lower(bound: &Option<RowKey>, than: &Option<RowKey>) -> bool {
    match (bound, than) {
        (Some(bound), Some(than)) => bound < than,
        (None, Some(_)) => true,
        (_, None) => false,
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
        let already_held = self.holds(tx_id);
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

    fn holds(&self, tx_id: TxID) -> bool {
        self.exclusive == Some(tx_id) || self.shared.contains(&tx_id)
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
