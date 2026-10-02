use rustc_hash::FxHashMap as HashMap;

use super::TxID;
use crate::sync::Mutex;

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

#[derive(Default)]
pub(crate) struct MetadataLocks {
    state: Mutex<LockState>,
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
    definition_changed_in: HashMap<String, u64>,
}

#[derive(Default)]
struct TableLocks {
    granted: Vec<(TxID, MetadataLockMode)>,
    waiting: Vec<(TxID, MetadataLockMode)>,
}

impl MetadataLocks {
    pub(crate) fn lock(&self, owner: TxID, table: &str, mode: MetadataLockMode) -> Vec<TxID> {
        let mut state = self.state.lock();
        let locks = state.tables.entry(table.to_string()).or_default();
        if locks.holds_as_strong(owner, mode) {
            return Vec::new();
        }
        let in_the_way = locks.owners_in_the_way(owner, mode);
        if !in_the_way.is_empty() {
            return in_the_way;
        }
        locks.waiting.retain(|(waiter, _)| *waiter != owner);
        locks.granted.push((owner, mode));
        state.note_owner_uses(owner, table);
        Vec::new()
    }

    pub(crate) fn owners_in_the_way(
        &self,
        owner: TxID,
        table: &str,
        mode: MetadataLockMode,
    ) -> Vec<TxID> {
        let state = self.state.lock();
        let Some(locks) = state.tables.get(table) else {
            return Vec::new();
        };
        if locks.holds_as_strong(owner, mode) {
            return Vec::new();
        }
        locks.owners_in_the_way(owner, mode)
    }

    pub(crate) fn add_waiting(&self, owner: TxID, table: &str, mode: MetadataLockMode) {
        let mut state = self.state.lock();
        let locks = state.tables.entry(table.to_string()).or_default();
        if locks.waiting.contains(&(owner, mode)) {
            return;
        }
        locks.waiting.push((owner, mode));
        state.note_owner_uses(owner, table);
    }

    pub(crate) fn remove_waiting(&self, owner: TxID, table: &str) {
        let mut state = self.state.lock();
        let now_unused = state.tables.get_mut(table).is_some_and(|locks| {
            locks.waiting.retain(|(waiter, _)| *waiter != owner);
            locks.is_empty()
        });
        if now_unused {
            state.tables.remove(table);
        }
    }

    pub(crate) fn holds_any(&self, owner: TxID) -> bool {
        let state = self.state.lock();
        state
            .tables_of_owner
            .get(&owner)
            .into_iter()
            .flatten()
            .any(|table| {
                state
                    .tables
                    .get(table)
                    .is_some_and(|locks| locks.granted.iter().any(|(holder, _)| *holder == owner))
            })
    }

    pub(crate) fn release(&self, owner: TxID) -> bool {
        let mut state = self.state.lock();
        let Some(tables) = state.tables_of_owner.remove(&owner) else {
            return false;
        };
        for table in tables {
            let now_unused = state.tables.get_mut(&table).is_some_and(|locks| {
                locks.granted.retain(|(holder, _)| *holder != owner);
                locks.waiting.retain(|(waiter, _)| *waiter != owner);
                locks.is_empty()
            });
            if now_unused {
                state.tables.remove(&table);
            }
        }
        true
    }

    pub(crate) fn move_locks(&self, from: TxID, to: TxID) {
        let mut state = self.state.lock();
        let Some(tables) = state.tables_of_owner.remove(&from) else {
            return;
        };
        for table in &tables {
            if let Some(locks) = state.tables.get_mut(table) {
                for (holder, _) in locks.granted.iter_mut().chain(locks.waiting.iter_mut()) {
                    if *holder == from {
                        *holder = to;
                    }
                }
            }
        }
        for table in tables {
            state.note_owner_uses(to, &table);
        }
    }

    pub(crate) fn deadlock_weight(&self, owner: TxID) -> u64 {
        if self.waits_for_a_definition_lock(owner) {
            DEFINITION_CHANGE_DEADLOCK_WEIGHT
        } else {
            TABLE_USE_DEADLOCK_WEIGHT
        }
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

    pub(crate) fn note_definitions_changed(&self, tables: &[String], schema_version: u64) {
        let mut state = self.state.lock();
        for table in tables {
            let changed_in = state
                .definition_changed_in
                .entry(table.clone())
                .or_default();
            *changed_in = (*changed_in).max(schema_version);
        }
    }

    pub(crate) fn definition_changed_after(&self, table: &str, schema_version: u64) -> bool {
        self.state
            .lock()
            .definition_changed_in
            .get(table)
            .is_some_and(|changed_in| *changed_in > schema_version)
    }

    pub(crate) fn a_used_definition_changed_after(&self, owner: TxID, schema_version: u64) -> bool {
        let state = self.state.lock();
        state
            .tables_of_owner
            .get(&owner)
            .into_iter()
            .flatten()
            .any(|table| {
                state
                    .definition_changed_in
                    .get(table)
                    .is_some_and(|changed_in| *changed_in > schema_version)
            })
    }
}

impl LockState {
    fn note_owner_uses(&mut self, owner: TxID, table: &str) {
        let tables = self.tables_of_owner.entry(owner).or_default();
        if !tables.iter().any(|used| used == table) {
            tables.push(table.to_string());
        }
    }
}

impl TableLocks {
    fn holds_as_strong(&self, owner: TxID, mode: MetadataLockMode) -> bool {
        self.granted
            .iter()
            .any(|(holder, held)| *holder == owner && held.is_at_least(mode))
    }

    fn owners_in_the_way(&self, owner: TxID, mode: MetadataLockMode) -> Vec<TxID> {
        let granted = self
            .granted
            .iter()
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
}
