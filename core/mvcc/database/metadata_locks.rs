use rustc_hash::FxHashMap as HashMap;

use super::TxID;
use crate::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::{Mutex, MutexGuard, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetadataLockMode {
    SharedRead,
    SharedWrite,
    SharedReadOnly,
    SharedNoReadWrite,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataLockRequest {
    pub(crate) owner: TxID,
    pub(crate) table: String,
    pub(crate) mode: MetadataLockMode,
    pub(crate) owner_held_nothing_before: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataLockWaitEnd {
    Granted,
    TimedOut,
    ChosenAsDeadlockVictim,
    Interrupted,
}

const TABLE_USE_DEADLOCK_WEIGHT: u64 = 10;
const DEFINITION_CHANGE_DEADLOCK_WEIGHT: u64 = 100;
const OWNER_SHARDS: usize = 64;

/// Table metadata locks, kept the way MySQL's MDL keeps them apart: the
/// shared read and write locks every statement takes are kept per owner in
/// `owners`, where statements of different owners do not meet, and are
/// granted there without looking at `state` while no lock that locks a
/// definition is granted, waited for or being asked for anywhere. Those
/// locks, and every waiting request, are kept in `state`; asking for one
/// holds every owner shard while it reads the shared locks.
pub(crate) struct MetadataLocks {
    state: Mutex<LockState>,
    owners: [Mutex<HashMap<TxID, OwnerLocks>>; OWNER_SHARDS],
    definition_locks_present: AtomicUsize,
    definition_changed_in: RwLock<HashMap<String, u64>>,
}

impl Default for MetadataLocks {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            owners: std::array::from_fn(|_| Mutex::new(HashMap::default())),
            definition_locks_present: AtomicUsize::new(0),
            definition_changed_in: RwLock::default(),
        }
    }
}

impl std::fmt::Debug for MetadataLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetadataLocks").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct LockState {
    tables: HashMap<String, TableLocks>,
    tables_of_owner: HashMap<TxID, Vec<String>>,
    definition_locks_being_asked_for: usize,
}

#[derive(Default)]
struct TableLocks {
    granted: Vec<(TxID, MetadataLockMode)>,
    waiting: Vec<(TxID, MetadataLockMode)>,
}

#[derive(Default)]
struct OwnerLocks {
    uses: Vec<(String, MetadataLockMode)>,
    has_locks_in_state: bool,
}

type OwnerShard<'a> = MutexGuard<'a, HashMap<TxID, OwnerLocks>>;

impl MetadataLocks {
    pub(crate) fn lock(&self, owner: TxID, table: &str, mode: MetadataLockMode) -> Vec<TxID> {
        if !mode.locks_the_definition() && self.use_without_the_state(owner, table, mode) {
            return Vec::new();
        }
        let mut state = self.state.lock();
        if mode.locks_the_definition() {
            return self.lock_the_definition(&mut state, owner, table, mode);
        }
        if self.holds_as_strong(&state, owner, table, mode) {
            return Vec::new();
        }
        let in_the_way = state
            .tables
            .get(table)
            .map(|locks| locks.owners_in_the_way(owner, mode, &[]))
            .unwrap_or_default();
        if !in_the_way.is_empty() {
            return in_the_way;
        }
        self.stop_waiting_in_state(&mut state, owner, table);
        self.owner_shard(owner)
            .entry(owner)
            .or_default()
            .uses
            .push((table.to_string(), mode));
        Vec::new()
    }

    pub(crate) fn owners_in_the_way(
        &self,
        owner: TxID,
        table: &str,
        mode: MetadataLockMode,
    ) -> Vec<TxID> {
        let state = self.state.lock();
        if self.holds_as_strong(&state, owner, table, mode) {
            return Vec::new();
        }
        let uses = if mode.locks_the_definition() {
            owners_of_conflicting_uses(&self.all_owner_shards(), owner, table, mode)
        } else {
            Vec::new()
        };
        match state.tables.get(table) {
            Some(locks) => locks.owners_in_the_way(owner, mode, &uses),
            None => TableLocks::default().owners_in_the_way(owner, mode, &uses),
        }
    }

    pub(crate) fn add_waiting(&self, owner: TxID, table: &str, mode: MetadataLockMode) {
        let mut state = self.state.lock();
        let locks = state.tables.entry(table.to_string()).or_default();
        if locks.waiting.contains(&(owner, mode)) {
            return;
        }
        locks.waiting.push((owner, mode));
        state.note_owner_uses(owner, table);
        self.owner_shard(owner)
            .entry(owner)
            .or_default()
            .has_locks_in_state = true;
        self.count_definition_locks(&state);
    }

    pub(crate) fn remove_waiting(&self, owner: TxID, table: &str) {
        let mut state = self.state.lock();
        self.stop_waiting_in_state(&mut state, owner, table);
    }

    pub(crate) fn holds_any(&self, owner: TxID) -> bool {
        let has_locks_in_state = match self.owner_shard(owner).get(&owner) {
            None => return false,
            Some(locks) if !locks.uses.is_empty() => return true,
            Some(locks) => locks.has_locks_in_state,
        };
        has_locks_in_state && self.state.lock().holds_any_granted(owner)
    }

    pub(crate) fn release(&self, owner: TxID) -> bool {
        let Some(locks) = self.owner_shard(owner).remove(&owner) else {
            return false;
        };
        if locks.has_locks_in_state {
            let mut state = self.state.lock();
            state.release(owner);
            self.count_definition_locks(&state);
        }
        true
    }

    pub(crate) fn move_locks(&self, from: TxID, to: TxID) {
        let has_locks_in_state = self
            .owner_shard(from)
            .get(&from)
            .is_some_and(|locks| locks.has_locks_in_state);
        let mut state = has_locks_in_state.then(|| self.state.lock());
        self.move_owner_locks(from, to);
        if let Some(state) = state.as_mut() {
            state.move_locks(from, to);
        }
    }

    pub(crate) fn deadlock_weight(&self, owner: TxID) -> u64 {
        if self.waits_for_a_definition_lock(owner) {
            DEFINITION_CHANGE_DEADLOCK_WEIGHT
        } else {
            TABLE_USE_DEADLOCK_WEIGHT
        }
    }

    pub(crate) fn note_definitions_changed(&self, tables: &[String], schema_version: u64) {
        let mut definition_changed_in = self.definition_changed_in.write();
        for table in tables {
            let changed_in = definition_changed_in.entry(table.clone()).or_default();
            *changed_in = (*changed_in).max(schema_version);
        }
    }

    pub(crate) fn definition_changed_after(&self, table: &str, schema_version: u64) -> bool {
        self.definition_changed_in
            .read()
            .get(table)
            .is_some_and(|changed_in| *changed_in > schema_version)
    }

    pub(crate) fn a_used_definition_changed_after(&self, owner: TxID, schema_version: u64) -> bool {
        let mut tables: Vec<String> = Vec::new();
        let has_locks_in_state = match self.owner_shard(owner).get(&owner) {
            None => false,
            Some(locks) => {
                tables.extend(locks.uses.iter().map(|(table, _)| table.clone()));
                locks.has_locks_in_state
            }
        };
        if has_locks_in_state {
            if let Some(owned) = self.state.lock().tables_of_owner.get(&owner) {
                tables.extend(owned.iter().cloned());
            }
        }
        let definition_changed_in = self.definition_changed_in.read();
        tables.iter().any(|table| {
            definition_changed_in
                .get(table)
                .is_some_and(|changed_in| *changed_in > schema_version)
        })
    }

    /// Grants a shared read or write lock in the owner's shard alone, which
    /// is right only while no lock that locks a definition is granted,
    /// waited for or being asked for: those are the only locks such a lock
    /// meets. One being asked for is counted before it reads the shards, and
    /// this reads the count under the owner's shard, so either it sees the
    /// count or the asker sees this lock.
    fn use_without_the_state(&self, owner: TxID, table: &str, mode: MetadataLockMode) -> bool {
        let mut shard = self.owner_shard(owner);
        let locks = shard.entry(owner).or_default();
        if locks
            .uses
            .iter()
            .any(|(used, held)| used == table && held.is_at_least(mode))
        {
            return true;
        }
        if self.definition_locks_present.load(Ordering::SeqCst) != 0 {
            return false;
        }
        locks.uses.push((table.to_string(), mode));
        true
    }

    fn lock_the_definition(
        &self,
        state: &mut LockState,
        owner: TxID,
        table: &str,
        mode: MetadataLockMode,
    ) -> Vec<TxID> {
        if self.holds_as_strong(state, owner, table, mode) {
            return Vec::new();
        }
        state.definition_locks_being_asked_for += 1;
        self.count_definition_locks(state);
        let uses = owners_of_conflicting_uses(&self.all_owner_shards(), owner, table, mode);
        let locks = state.tables.entry(table.to_string()).or_default();
        let in_the_way = locks.owners_in_the_way(owner, mode, &uses);
        if in_the_way.is_empty() {
            locks.waiting.retain(|(waiter, _)| *waiter != owner);
            locks.granted.push((owner, mode));
            state.note_owner_uses(owner, table);
            self.owner_shard(owner)
                .entry(owner)
                .or_default()
                .has_locks_in_state = true;
        }
        state.definition_locks_being_asked_for -= 1;
        self.count_definition_locks(state);
        in_the_way
    }

    fn holds_as_strong(
        &self,
        state: &LockState,
        owner: TxID,
        table: &str,
        mode: MetadataLockMode,
    ) -> bool {
        let holds_a_use = self.owner_shard(owner).get(&owner).is_some_and(|locks| {
            locks
                .uses
                .iter()
                .any(|(used, held)| used == table && held.is_at_least(mode))
        });
        holds_a_use
            || state
                .tables
                .get(table)
                .is_some_and(|locks| locks.holds_as_strong(owner, mode))
    }

    fn stop_waiting_in_state(&self, state: &mut LockState, owner: TxID, table: &str) {
        let now_unused = state.tables.get_mut(table).is_some_and(|locks| {
            locks.waiting.retain(|(waiter, _)| *waiter != owner);
            locks.is_empty()
        });
        if now_unused {
            state.tables.remove(table);
        }
        self.count_definition_locks(state);
    }

    fn count_definition_locks(&self, state: &LockState) {
        let held_or_waited_for = state
            .tables
            .values()
            .flat_map(|locks| locks.granted.iter().chain(locks.waiting.iter()))
            .filter(|(_, mode)| mode.locks_the_definition())
            .count();
        self.definition_locks_present.store(
            held_or_waited_for + state.definition_locks_being_asked_for,
            Ordering::SeqCst,
        );
    }

    fn waits_for_a_definition_lock(&self, owner: TxID) -> bool {
        let state = self.state.lock();
        state
            .tables_of_owner
            .get(&owner)
            .into_iter()
            .flatten()
            .any(|table| {
                state.tables.get(table).is_some_and(|locks| {
                    locks
                        .waiting
                        .iter()
                        .any(|(waiter, mode)| *waiter == owner && mode.locks_the_definition())
                })
            })
    }

    /// Holds both owners' shards at once, so that a request reading every
    /// shard finds the locks under one owner or the other.
    fn move_owner_locks(&self, from: TxID, to: TxID) {
        let (from_index, to_index) = (shard_index(from), shard_index(to));
        if from_index == to_index {
            let mut shard = self.owners[from_index].lock();
            if let Some(moved) = shard.remove(&from) {
                shard.entry(to).or_default().take_up(moved);
            }
            return;
        }
        let mut first = self.owners[from_index.min(to_index)].lock();
        let mut second = self.owners[from_index.max(to_index)].lock();
        let (from_shard, to_shard) = if from_index < to_index {
            (&mut first, &mut second)
        } else {
            (&mut second, &mut first)
        };
        if let Some(moved) = from_shard.remove(&from) {
            to_shard.entry(to).or_default().take_up(moved);
        }
    }

    fn owner_shard(&self, owner: TxID) -> OwnerShard<'_> {
        self.owners[shard_index(owner)].lock()
    }

    fn all_owner_shards(&self) -> Vec<OwnerShard<'_>> {
        self.owners.iter().map(|shard| shard.lock()).collect()
    }
}

fn shard_index(owner: TxID) -> usize {
    (owner % OWNER_SHARDS as u64) as usize
}

fn owners_of_conflicting_uses(
    shards: &[OwnerShard<'_>],
    owner: TxID,
    table: &str,
    mode: MetadataLockMode,
) -> Vec<(TxID, MetadataLockMode)> {
    shards
        .iter()
        .flat_map(|shard| shard.iter())
        .filter(|(holder, _)| **holder != owner)
        .flat_map(|(holder, locks)| {
            locks
                .uses
                .iter()
                .filter(|(used, held)| used == table && mode.conflicts_with_granted(*held))
                .map(|(_, held)| (*holder, *held))
        })
        .collect()
}

impl LockState {
    fn note_owner_uses(&mut self, owner: TxID, table: &str) {
        let tables = self.tables_of_owner.entry(owner).or_default();
        if !tables.iter().any(|used| used == table) {
            tables.push(table.to_string());
        }
    }

    fn holds_any_granted(&self, owner: TxID) -> bool {
        self.tables_of_owner
            .get(&owner)
            .into_iter()
            .flatten()
            .any(|table| {
                self.tables
                    .get(table)
                    .is_some_and(|locks| locks.granted.iter().any(|(holder, _)| *holder == owner))
            })
    }

    fn release(&mut self, owner: TxID) {
        let Some(tables) = self.tables_of_owner.remove(&owner) else {
            return;
        };
        for table in tables {
            let now_unused = self.tables.get_mut(&table).is_some_and(|locks| {
                locks.granted.retain(|(holder, _)| *holder != owner);
                locks.waiting.retain(|(waiter, _)| *waiter != owner);
                locks.is_empty()
            });
            if now_unused {
                self.tables.remove(&table);
            }
        }
    }

    fn move_locks(&mut self, from: TxID, to: TxID) {
        let Some(tables) = self.tables_of_owner.remove(&from) else {
            return;
        };
        for table in &tables {
            if let Some(locks) = self.tables.get_mut(table) {
                for (holder, _) in locks.granted.iter_mut().chain(locks.waiting.iter_mut()) {
                    if *holder == from {
                        *holder = to;
                    }
                }
            }
        }
        for table in tables {
            self.note_owner_uses(to, &table);
        }
    }
}

impl OwnerLocks {
    fn take_up(&mut self, moved: OwnerLocks) {
        self.uses.extend(moved.uses);
        self.has_locks_in_state |= moved.has_locks_in_state;
    }
}

impl TableLocks {
    fn holds_as_strong(&self, owner: TxID, mode: MetadataLockMode) -> bool {
        self.granted
            .iter()
            .any(|(holder, held)| *holder == owner && held.is_at_least(mode))
    }

    fn owners_in_the_way(
        &self,
        owner: TxID,
        mode: MetadataLockMode,
        uses: &[(TxID, MetadataLockMode)],
    ) -> Vec<TxID> {
        let granted = self
            .granted
            .iter()
            .chain(uses.iter())
            .filter(|(holder, held)| *holder != owner && mode.conflicts_with_granted(*held));
        let waiting = self
            .waiting
            .iter()
            .filter(|(waiter, waits)| *waiter != owner && mode.conflicts_with_waiting(*waits));
        let mut owners: Vec<TxID> = granted.chain(waiting).map(|(other, _)| *other).collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }

    fn is_empty(&self) -> bool {
        self.granted.is_empty() && self.waiting.is_empty()
    }
}

impl MetadataLockMode {
    fn conflicts_with_granted(self, granted: MetadataLockMode) -> bool {
        use MetadataLockMode::*;
        match self {
            SharedRead => matches!(granted, SharedNoReadWrite | Exclusive),
            SharedWrite => matches!(granted, SharedReadOnly | SharedNoReadWrite | Exclusive),
            SharedReadOnly => matches!(granted, SharedWrite | SharedNoReadWrite | Exclusive),
            SharedNoReadWrite | Exclusive => true,
        }
    }

    fn conflicts_with_waiting(self, waiting: MetadataLockMode) -> bool {
        use MetadataLockMode::*;
        match self {
            SharedRead | SharedWrite => matches!(waiting, SharedNoReadWrite | Exclusive),
            SharedReadOnly => matches!(waiting, SharedWrite | SharedNoReadWrite | Exclusive),
            SharedNoReadWrite => waiting == Exclusive,
            Exclusive => false,
        }
    }

    fn is_at_least(self, other: MetadataLockMode) -> bool {
        use MetadataLockMode::*;
        [
            SharedRead,
            SharedWrite,
            SharedReadOnly,
            SharedNoReadWrite,
            Exclusive,
        ]
        .into_iter()
        .all(|granted| {
            !other.conflicts_with_granted(granted) || self.conflicts_with_granted(granted)
        })
    }

    fn locks_the_definition(self) -> bool {
        matches!(
            self,
            MetadataLockMode::SharedReadOnly
                | MetadataLockMode::SharedNoReadWrite
                | MetadataLockMode::Exclusive
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{MetadataLockMode::*, MetadataLocks, TABLE_USE_DEADLOCK_WEIGHT};

    #[test]
    fn readers_and_writers_share_a_table_and_a_definition_change_waits_for_both() {
        let locks = MetadataLocks::default();
        assert!(locks.lock(1, "t", SharedRead).is_empty());
        assert!(locks.lock(2, "t", SharedWrite).is_empty());
        assert_eq!(locks.lock(3, "t", Exclusive), [1, 2]);
        assert!(locks.lock(3, "u", Exclusive).is_empty());
    }

    #[test]
    fn a_waiting_definition_change_holds_back_new_readers_but_not_old_holders() {
        let locks = MetadataLocks::default();
        assert!(locks.lock(1, "t", SharedRead).is_empty());
        assert_eq!(locks.lock(2, "t", Exclusive), [1]);
        locks.add_waiting(2, "t", Exclusive);
        assert_eq!(locks.lock(3, "t", SharedRead), [2]);
        assert!(locks.lock(1, "t", SharedRead).is_empty());
        assert_eq!(locks.lock(1, "t", SharedWrite), [2]);
        assert!(locks.release(1));
        assert!(locks.lock(2, "t", Exclusive).is_empty());
        assert_eq!(locks.deadlock_weight(2), TABLE_USE_DEADLOCK_WEIGHT);
        assert_eq!(locks.lock(3, "t", SharedRead), [2]);
    }

    #[test]
    fn a_table_locked_for_reading_takes_readers_and_refuses_writers() {
        let locks = MetadataLocks::default();
        assert!(locks.lock(1, "t", SharedReadOnly).is_empty());
        assert!(locks.lock(2, "t", SharedRead).is_empty());
        assert!(locks.lock(3, "t", SharedReadOnly).is_empty());
        assert_eq!(locks.lock(4, "t", SharedWrite), [1, 3]);
        locks.add_waiting(4, "t", SharedWrite);
        assert_eq!(locks.lock(5, "t", SharedReadOnly), [4]);
        assert_eq!(locks.lock(6, "t", SharedWrite), [1, 3]);
    }

    #[test]
    fn a_table_locked_for_writing_refuses_everyone_else() {
        let locks = MetadataLocks::default();
        assert!(locks.lock(1, "t", SharedNoReadWrite).is_empty());
        for mode in [
            SharedRead,
            SharedWrite,
            SharedReadOnly,
            SharedNoReadWrite,
            Exclusive,
        ] {
            assert_eq!(locks.lock(2, "t", mode), [1]);
        }
        assert!(locks.lock(1, "t", SharedWrite).is_empty());
        assert!(locks.lock(2, "u", SharedWrite).is_empty());
    }

    #[test]
    fn locks_moved_to_another_owner_belong_to_it() {
        let locks = MetadataLocks::default();
        assert!(locks.lock(1, "t", Exclusive).is_empty());
        locks.move_locks(1, 7);
        assert!(!locks.holds_any(1));
        assert!(locks.holds_any(7));
        assert_eq!(locks.lock(2, "t", SharedRead), [7]);
        assert!(locks.release(7));
        assert!(locks.lock(2, "t", SharedRead).is_empty());
    }

    #[test]
    fn a_definition_change_is_seen_only_by_owners_of_that_table() {
        let locks = MetadataLocks::default();
        assert!(locks.lock(1, "t", SharedWrite).is_empty());
        assert!(locks.lock(2, "u", SharedWrite).is_empty());
        locks.note_definitions_changed(&["t".to_string()], 10);
        assert!(locks.a_used_definition_changed_after(1, 5));
        assert!(!locks.a_used_definition_changed_after(1, 10));
        assert!(!locks.a_used_definition_changed_after(2, 5));
        assert!(locks.definition_changed_after("t", 9));
        assert!(!locks.definition_changed_after("u", 0));
    }

    #[test]
    fn a_shared_use_moved_to_another_owner_still_holds_back_a_definition_change() {
        let locks = MetadataLocks::default();
        assert!(locks.lock(1, "t", SharedRead).is_empty());
        locks.move_locks(1, 70);
        assert!(!locks.holds_any(1));
        assert!(locks.holds_any(70));
        assert_eq!(locks.lock(2, "t", Exclusive), [70]);
        assert!(locks.release(70));
        assert!(locks.lock(2, "t", Exclusive).is_empty());
        assert_eq!(locks.lock(3, "t", SharedRead), [2]);
    }

    #[test]
    fn shared_uses_and_a_definition_change_taken_side_by_side_never_overlap() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let locks = Arc::new(MetadataLocks::default());
        let readers_inside = Arc::new(AtomicUsize::new(0));
        let changer_inside = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (1..=4u64)
            .map(|reader| {
                let locks = Arc::clone(&locks);
                let readers_inside = Arc::clone(&readers_inside);
                let changer_inside = Arc::clone(&changer_inside);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut owner = reader * 1_000_000;
                    while !stop.load(Ordering::SeqCst) {
                        owner += 1;
                        if locks.lock(owner, "t", SharedRead).is_empty() {
                            readers_inside.fetch_add(1, Ordering::SeqCst);
                            assert!(!changer_inside.load(Ordering::SeqCst));
                            readers_inside.fetch_sub(1, Ordering::SeqCst);
                        }
                        locks.release(owner);
                    }
                })
            })
            .collect();
        for owner in 1..=500u64 {
            while !locks.lock(owner, "t", Exclusive).is_empty() {
                std::thread::yield_now();
            }
            changer_inside.store(true, Ordering::SeqCst);
            assert_eq!(readers_inside.load(Ordering::SeqCst), 0);
            changer_inside.store(false, Ordering::SeqCst);
            assert!(locks.release(owner));
        }
        stop.store(true, Ordering::SeqCst);
        for reader in readers {
            reader.join().unwrap();
        }
    }
}
