use super::{CursorPosition, CursorRowLocks, MvccCursorType, MvccLazyCursor, RangeEndCheck};
use crate::alloc::ConcurrentAllocator;
use crate::mvcc::clock::LogicalClock;
use crate::mvcc::database::{
    GapBetween, GapKey, RowID, RowKey, RowLockAttempt, RowLockLevel, RowLockMode,
    RowLockWaitPolicy, SortableIndexKey,
};
use crate::numeric::Numeric;
use crate::storage::btree::CursorTrait;
use crate::sync::Arc;
use crate::translate::plan::IterationDirection;
use crate::types::{IOResult, IOResultOr, IndexInfo, SeekKey, SeekOp, SeekResult, Value};
use crate::{return_if_io, turso_assert, LimboError, Result};

#[derive(Default)]
pub(super) struct ScanLocks {
    previous: Option<RowKey>,
    pending: Option<PendingRow>,
    arrival: Option<(Arrival, Option<RowKey>)>,
    search: Option<NeighborSearch>,
    unique_match: bool,
    lookups: u32,
    current: CurrentRow,
    pub(super) positions_to_write: bool,
    pub(super) reads_the_range_end: bool,
}

#[derive(Default)]
struct CurrentRow {
    matched: bool,
    found_by_a_unique_key: bool,
    newly_locked: Vec<RowID>,
    held_by: Option<Vec<u64>>,
}

#[derive(Clone)]
struct PendingRow {
    row: RowKey,
    direction: IterationDirection,
    below: Below,
    reads_past_a_held_row: bool,
}

#[derive(Clone)]
enum Below {
    Nothing,
    Known(Option<RowKey>, ScanStart),
    Unknown(ScanStart),
}

#[derive(Clone)]
enum ScanStart {
    Everywhere,
    Sought { key: RowKey, op: SeekOp },
}

#[derive(Clone)]
enum Arrival {
    Rewound,
    AtTheLastRow,
    Stepped(IterationDirection),
    MovingPastAHeldRow(IterationDirection),
    Sought {
        start: RowKey,
        op: SeekOp,
        result: SeekResult,
        neighbors: Neighbors,
    },
    Probed {
        key: i64,
        found: bool,
        neighbors: Neighbors,
    },
}

#[derive(Clone, Default)]
struct Neighbors {
    below: Option<Option<RowKey>>,
    above: Option<Option<RowKey>>,
}

impl Neighbors {
    fn on(&mut self, side: Side) -> &mut Option<Option<RowKey>> {
        match side {
            Side::Below => &mut self.below,
            Side::Above => &mut self.above,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Side {
    Below,
    Above,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Arrived {
    OnARow,
    OffTheRows,
    PastAHeldRow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UndoneInsertNeighbor {
    Below,
    Above,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordLock {
    Taken,
    HeldByAnother,
}

enum NeighborSearch {
    Seeking,
    Returning(Option<RowKey>),
}

#[derive(Clone)]
enum AfterTheSearch {
    ReturnTo(RowKey, SeekOp),
    LeaveTheRows(IterationDirection),
}

pub(crate) struct RangeEnd<'a> {
    pub(crate) equality: bool,
    pub(crate) is_the_last_key_inside: &'a dyn Fn(&RowKey) -> bool,
    pub(crate) holds: &'a dyn Fn(&RowKey) -> bool,
}

impl<Clock: LogicalClock + 'static, A: ConcurrentAllocator> MvccLazyCursor<Clock, A> {
    pub(super) fn locks_this_move(&self) -> bool {
        self.row_locks.is_some() && !self.scan.positions_to_write
    }

    pub(super) fn has_a_pending_row_lock(&self) -> bool {
        self.scan.pending.is_some() && !self.scan.reads_the_range_end
    }

    pub(super) fn step_and_lock(&mut self, direction: IterationDirection) -> IOResultOr<()> {
        if self.scan.arrival.is_none() {
            return_if_io!(self.leave_the_row(true));
            self.scan.previous = self.current_key();
            match direction {
                IterationDirection::Forwards => return_if_io!(self.next_row()),
                IterationDirection::Backwards => return_if_io!(self.prev_row()),
            }
            self.arrive(Arrival::Stepped(direction));
        }
        return_if_io!(self.finish_the_arrival());
        Ok(IOResult::Done(()))
    }

    pub(super) fn start_over_and_lock(&mut self, direction: IterationDirection) -> IOResultOr<()> {
        if self.scan.arrival.is_none() {
            return_if_io!(self.leave_the_row(false));
            self.scan.lookups += 1;
            self.scan.previous = None;
            self.scan.unique_match = false;
            match direction {
                IterationDirection::Forwards => return_if_io!(self.rewind_rows()),
                IterationDirection::Backwards => return_if_io!(self.last_row()),
            }
            self.arrive(match direction {
                IterationDirection::Forwards => Arrival::Rewound,
                IterationDirection::Backwards => Arrival::AtTheLastRow,
            });
        }
        return_if_io!(self.finish_the_arrival());
        Ok(IOResult::Done(()))
    }

    pub(super) fn seek_and_lock(
        &mut self,
        seek_key: SeekKey<'_>,
        op: SeekOp,
    ) -> IOResultOr<SeekResult> {
        if self.scan.arrival.is_none() {
            return_if_io!(self.leave_the_row(false));
            self.scan.lookups += 1;
            self.scan.previous = None;
            self.scan.unique_match = false;
            let start = self.row_key_of(&seek_key)?;
            let result = return_if_io!(self.seek_row(seek_key, op));
            self.arrive(Arrival::Sought {
                start,
                op,
                result,
                neighbors: Neighbors::default(),
            });
        }
        let arrived = return_if_io!(self.finish_the_arrival());
        let positioned = matches!(self.current_pos, CursorPosition::Loaded { .. });
        Ok(IOResult::Done(match arrived {
            Arrived::OnARow if positioned => SeekResult::Found,
            _ => SeekResult::NotFound,
        }))
    }

    pub(super) fn probe_and_lock(&mut self, key: &Value) -> IOResultOr<bool> {
        if self.scan.arrival.is_none() {
            return_if_io!(self.leave_the_row(false));
            self.scan.lookups += 1;
            self.scan.previous = None;
            self.scan.unique_match = false;
            let found = return_if_io!(self.row_exists(key));
            let Value::Numeric(Numeric::Integer(key)) = key else {
                unreachable!("a table row is found by its integer rowid");
            };
            self.arrive(Arrival::Probed {
                key: *key,
                found,
                neighbors: Neighbors::default(),
            });
        }
        let arrived = return_if_io!(self.finish_the_arrival());
        Ok(IOResult::Done(arrived == Arrived::OnARow))
    }

    pub(super) fn lock_the_pending_row_before_reading_it(&mut self) -> IOResultOr<()> {
        let taken = return_if_io!(self.lock_the_pending_row());
        turso_assert!(
            taken == RecordLock::Taken,
            "a row another transaction holds is skipped at its range end check, before it is read"
        );
        Ok(IOResult::Done(()))
    }

    pub(crate) fn passed_the_range_end_check(
        &mut self,
        unique_equality: bool,
    ) -> IOResultOr<RecordLock> {
        let Some(pending) = self.scan.pending.as_mut() else {
            return Ok(IOResult::Done(RecordLock::Taken));
        };
        if unique_equality {
            pending.below = Below::Nothing;
            self.scan.unique_match = true;
            self.scan.current.found_by_a_unique_key = true;
        }
        self.lock_the_pending_row()
    }

    pub(crate) fn reached_the_range_end(&mut self, end: &RangeEnd<'_>) -> IOResultOr<()> {
        let Some(pending) = self.scan.pending.clone() else {
            return Ok(IOResult::Done(()));
        };
        let row_locks = self.row_locks_in_force();
        let row = pending.row.clone();
        let repeatable = row_locks.level == RowLockLevel::RepeatableRead;
        match pending.direction {
            IterationDirection::Backwards => {
                turso_assert!(
                    end.equality,
                    "a descending scan checks its range end before locking only for one value"
                );
            }
            IterationDirection::Forwards if repeatable => {
                let ended_on_the_previous_row = self
                    .scan
                    .previous
                    .as_ref()
                    .is_some_and(|previous| (end.is_the_last_key_inside)(previous));
                let nothing_more =
                    self.scan.unique_match || (row_locks.primary && ended_on_the_previous_row);
                if !nothing_more {
                    let (low, start) = return_if_io!(self.gap_below_the_pending_row());
                    self.lock_the_gap(low, Some(row.clone()), |key| match start.place(key) {
                        GapKey::Met if !(end.holds)(key) => GapKey::AboveTheScan,
                        place => place,
                    })?;
                }
            }
            IterationDirection::Forwards => {
                let (low, start) = return_if_io!(self.gap_below_the_pending_row());
                self.lock_the_gap(low, Some(row.clone()), |key| match start.place(key) {
                    GapKey::Met if end.equality && !(end.holds)(key) => GapKey::AboveTheScan,
                    place => place,
                })?;
                if !end.equality && !pending.reads_past_a_held_row {
                    self.try_the_record_without_keeping_it(&row)?;
                }
            }
        }
        self.scan.pending = None;
        Ok(IOResult::Done(()))
    }

    pub(crate) fn row_matched(&mut self) -> Result<()> {
        turso_assert!(
            self.scan.pending.is_none(),
            "a row reached the end of its conditions before its range end check"
        );
        self.scan.current.matched = true;
        match self.scan.current.held_by.take() {
            Some(holders) => Err(LimboError::RowLocked(holders)),
            None => Ok(()),
        }
    }

    pub(crate) fn let_go_of_a_row_that_did_not_match(&mut self) {
        let Some(row_locks) = self.row_locks else {
            return;
        };
        if !row_locks.releases_unmatched_rows
            || self.scan.current.matched
            || self.keeps_its_one_unique_lookup()
        {
            return;
        }
        let rows = std::mem::take(&mut self.scan.current.newly_locked);
        self.unlock_rows(&rows);
    }

    pub(crate) fn lock_the_duplicate(&mut self) -> IOResultOr<()> {
        let Some(row_locks) = self.row_locks else {
            return Ok(IOResult::Done(()));
        };
        let Some(row) = self.current_key() else {
            return Ok(IOResult::Done(()));
        };
        let id = RowID::new(self.table_id, row.clone());
        match self
            .db
            .lock_row_for_read(self.tx_id, &id, row_locks.duplicate_mode)?
        {
            RowLockAttempt::Locked { .. } => {}
            RowLockAttempt::HeldBy(holders) => return Err(LimboError::RowLocked(holders).into()),
            RowLockAttempt::ChangedSinceRead => {
                return Err(LimboError::RowLocked(Vec::new()).into())
            }
        }
        if row_locks.primary {
            return Ok(IOResult::Done(()));
        }
        let low = return_if_io!(self.neighbor_of(
            &row,
            SeekOp::LT,
            AfterTheSearch::ReturnTo(row.clone(), SeekOp::GE { eq_only: false }),
        ));
        self.db.lock_gap(
            self.tx_id,
            GapBetween {
                table_id: self.table_id,
                low,
                high: Some(row),
            },
            |_| GapKey::BelowTheScan,
            true,
        )?;
        Ok(IOResult::Done(()))
    }

    pub(crate) fn neighbor_of_an_undone_insert(
        &mut self,
        key: &RowKey,
        side: UndoneInsertNeighbor,
    ) -> IOResultOr<Option<RowKey>> {
        let op = match side {
            UndoneInsertNeighbor::Below => SeekOp::LT,
            UndoneInsertNeighbor::Above => SeekOp::GT,
        };
        self.neighbor_of(
            key,
            op,
            AfterTheSearch::LeaveTheRows(IterationDirection::Forwards),
        )
    }

    pub(crate) fn keeps_gaps(&self) -> bool {
        self.row_locks
            .is_some_and(|row_locks| row_locks.level == RowLockLevel::RepeatableRead)
    }

    pub(crate) fn keep_the_gap_an_undone_insert_left(
        &self,
        key: &RowKey,
        low: Option<RowKey>,
        high: Option<RowKey>,
    ) {
        self.db.keep_the_gap_an_undone_insert_left(
            self.tx_id,
            GapBetween {
                table_id: self.table_id,
                low,
                high,
            },
            |inserted| {
                if inserted < key {
                    GapKey::BelowTheScan
                } else {
                    GapKey::AboveTheScan
                }
            },
        );
    }

    pub(crate) fn table_id(&self) -> crate::mvcc::database::MVTableId {
        self.table_id
    }

    fn row_locks_in_force(&self) -> CursorRowLocks {
        self.row_locks
            .expect("only a cursor that locks the rows it reads keeps scan locks")
    }

    fn current_key(&self) -> Option<RowKey> {
        match &self.current_pos {
            CursorPosition::Loaded { row_id, .. } => Some(row_id.row_id.clone()),
            CursorPosition::BeforeFirst | CursorPosition::End => None,
        }
    }

    fn row_key_of(&self, seek_key: &SeekKey<'_>) -> Result<RowKey> {
        Ok(match seek_key {
            SeekKey::TableRowId(rowid) => RowKey::Int(*rowid),
            SeekKey::IndexKey(record) => {
                let MvccCursorType::Index(index_info) = &self.mv_cursor_type else {
                    panic!("an index key is sought in an index");
                };
                let info = Arc::new(IndexInfo::new_in(
                    index_info.key_info.iter().cloned(),
                    index_info.has_rowid,
                    record.column_count(),
                    index_info.is_unique,
                    self.db.allocator(),
                )?);
                RowKey::Record(Arc::new(SortableIndexKey::new_from_payload_in(
                    record.get_payload(),
                    info,
                    self.db.allocator(),
                )?))
            }
        })
    }

    fn leave_the_row(&mut self, keeps_one_unique_lookup: bool) -> IOResultOr<()> {
        if self.scan.pending.is_some() {
            return_if_io!(self.lock_the_pending_row());
        }
        let keeps = keeps_one_unique_lookup && self.keeps_its_one_unique_lookup();
        let current = std::mem::take(&mut self.scan.current);
        if self.row_locks_in_force().releases_unmatched_rows && !current.matched && !keeps {
            self.unlock_rows(&current.newly_locked);
        }
        Ok(IOResult::Done(()))
    }

    fn keeps_its_one_unique_lookup(&self) -> bool {
        self.row_locks_in_force().locking_select
            && self.scan.lookups == 1
            && self.scan.current.found_by_a_unique_key
    }

    fn unlock_rows(&self, rows: &[RowID]) {
        for row in rows {
            self.db.unlock_row(self.tx_id, row);
        }
    }

    fn arrive(&mut self, arrival: Arrival) {
        self.scan.arrival = Some((arrival, self.current_key()));
    }

    fn finish_the_arrival(&mut self) -> IOResultOr<Arrived> {
        loop {
            let (arrival, row) = self.scan.arrival.clone().expect("a move is being finished");
            if let Arrival::MovingPastAHeldRow(direction) = arrival {
                match direction {
                    IterationDirection::Forwards => return_if_io!(self.next_row()),
                    IterationDirection::Backwards => return_if_io!(self.prev_row()),
                }
                self.arrive(Arrival::Stepped(direction));
                continue;
            }
            let arrived = return_if_io!(self.lock_on_arrival(arrival.clone(), row.clone()));
            if arrived == Arrived::PastAHeldRow {
                if let Some(direction) = self.direction_to_move_past_a_held_row(&arrival) {
                    self.scan.previous = row;
                    self.scan.current = CurrentRow::default();
                    self.scan.arrival = Some((Arrival::MovingPastAHeldRow(direction), None));
                    continue;
                }
                self.move_off_the_rows(IterationDirection::Forwards);
            }
            self.scan.arrival = None;
            return Ok(IOResult::Done(arrived));
        }
    }

    fn direction_to_move_past_a_held_row(&self, arrival: &Arrival) -> Option<IterationDirection> {
        match arrival {
            Arrival::Rewound => Some(IterationDirection::Forwards),
            Arrival::AtTheLastRow => Some(IterationDirection::Backwards),
            Arrival::Stepped(direction) | Arrival::MovingPastAHeldRow(direction) => {
                Some(*direction)
            }
            Arrival::Sought { op, .. } if !op.eq_only() => Some(op.iteration_direction()),
            Arrival::Sought { .. } | Arrival::Probed { .. } => None,
        }
    }

    fn lock_on_arrival(&mut self, arrival: Arrival, row: Option<RowKey>) -> IOResultOr<Arrived> {
        self.scan.current = CurrentRow::default();
        let previous = self.scan.previous.clone();
        match arrival {
            Arrival::Rewound | Arrival::Stepped(IterationDirection::Forwards) => {
                let low = match arrival {
                    Arrival::Rewound => None,
                    _ => previous,
                };
                match row {
                    Some(row) => self.expect_the_row(
                        row,
                        IterationDirection::Forwards,
                        Below::Known(low, ScanStart::Everywhere),
                    ),
                    None => {
                        if !self.scan.unique_match {
                            self.lock_the_gap(low, None, |_| GapKey::Met)?;
                        }
                        Ok(IOResult::Done(Arrived::OffTheRows))
                    }
                }
            }
            Arrival::AtTheLastRow | Arrival::Stepped(IterationDirection::Backwards) => {
                let high = match arrival {
                    Arrival::AtTheLastRow => None,
                    _ => previous,
                };
                match row {
                    Some(row) => {
                        self.lock_the_gap(Some(row.clone()), high, |_| GapKey::Met)?;
                        self.expect_the_row(row, IterationDirection::Backwards, Below::Nothing)
                    }
                    None => {
                        self.lock_the_gap(None, high, |_| GapKey::Met)?;
                        Ok(IOResult::Done(Arrived::OffTheRows))
                    }
                }
            }
            Arrival::Sought {
                start, op, result, ..
            } => self.lock_after_a_seek(row, start, op, result),
            Arrival::Probed { key, found, .. } => self.lock_after_a_probe(row, key, found),
            Arrival::MovingPastAHeldRow(_) => {
                unreachable!("moving past a held row is finished before locking")
            }
        }
    }

    fn lock_after_a_seek(
        &mut self,
        row: Option<RowKey>,
        start: RowKey,
        op: SeekOp,
        result: SeekResult,
    ) -> IOResultOr<Arrived> {
        let row_locks = self.row_locks_in_force();
        let sought = ScanStart::Sought {
            key: start.clone(),
            op,
        };
        let direction = op.iteration_direction();
        let found_row = row.clone().filter(|_| result == SeekResult::Found);
        match (direction, found_row) {
            (IterationDirection::Forwards, Some(row)) => {
                let exact_start = row_locks.primary
                    && matches!(op, SeekOp::GE { .. })
                    && row_key_is(&row, &start);
                self.scan.current.found_by_a_unique_key = exact_start && op.eq_only();
                let below = if exact_start {
                    Below::Nothing
                } else {
                    Below::Unknown(sought)
                };
                self.expect_the_row(row, direction, below)
            }
            (IterationDirection::Backwards, Some(row)) => {
                let high = return_if_io!(self.neighbor_once(
                    Side::Above,
                    &row,
                    SeekOp::GT,
                    AfterTheSearch::ReturnTo(row.clone(), SeekOp::LE { eq_only: false }),
                ));
                self.lock_the_gap(Some(row.clone()), high, |key| sought.place(key))?;
                self.expect_the_row(row, direction, Below::Nothing)
            }
            (_, None) => {
                let (low, high) = match row {
                    Some(near) => match direction {
                        IterationDirection::Forwards => {
                            let low = return_if_io!(self.neighbor_once(
                                Side::Below,
                                &near,
                                SeekOp::LT,
                                AfterTheSearch::ReturnTo(
                                    near.clone(),
                                    SeekOp::GE { eq_only: false }
                                ),
                            ));
                            (low, Some(near))
                        }
                        IterationDirection::Backwards => {
                            let high = return_if_io!(self.neighbor_once(
                                Side::Above,
                                &near,
                                SeekOp::GT,
                                AfterTheSearch::ReturnTo(
                                    near.clone(),
                                    SeekOp::LE { eq_only: false }
                                ),
                            ));
                            (Some(near), high)
                        }
                    },
                    None => return_if_io!(self.neighbors_around_a_missed_key(&start, op)),
                };
                self.lock_the_gap(low, high, |key| sought.place(key))?;
                Ok(IOResult::Done(Arrived::OffTheRows))
            }
        }
    }

    fn lock_after_a_probe(
        &mut self,
        row: Option<RowKey>,
        key: i64,
        found: bool,
    ) -> IOResultOr<Arrived> {
        if found {
            let row = row.expect("a found row is under the cursor");
            return match self.lock_the_record(&row, true, false)? {
                RecordLock::Taken => {
                    self.scan.current.found_by_a_unique_key = true;
                    Ok(IOResult::Done(Arrived::OnARow))
                }
                RecordLock::HeldByAnother => Ok(IOResult::Done(Arrived::PastAHeldRow)),
            };
        }
        let probed = RowKey::Int(key);
        let op = SeekOp::GE { eq_only: true };
        let (low, high) = return_if_io!(self.neighbors_around_a_missed_key(&probed, op));
        let sought = ScanStart::Sought { key: probed, op };
        self.lock_the_gap(low, high, |key| sought.place(key))?;
        Ok(IOResult::Done(Arrived::OffTheRows))
    }

    fn neighbors_around_a_missed_key(
        &mut self,
        start: &RowKey,
        op: SeekOp,
    ) -> IOResultOr<(Option<RowKey>, Option<RowKey>)> {
        let direction = op.iteration_direction();
        let leave = AfterTheSearch::LeaveTheRows(direction);
        let low = if direction == IterationDirection::Forwards || op.eq_only() {
            let below = match op {
                SeekOp::GT => SeekOp::LE { eq_only: false },
                _ => SeekOp::LT,
            };
            return_if_io!(self.neighbor_once(Side::Below, start, below, leave.clone()))
        } else {
            None
        };
        let high = if direction == IterationDirection::Backwards || op.eq_only() {
            let above = match op {
                SeekOp::LT => SeekOp::GE { eq_only: false },
                _ => SeekOp::GT,
            };
            return_if_io!(self.neighbor_once(Side::Above, start, above, leave))
        } else {
            None
        };
        Ok(IOResult::Done((low, high)))
    }

    fn neighbor_once(
        &mut self,
        side: Side,
        key: &RowKey,
        op: SeekOp,
        after: AfterTheSearch,
    ) -> IOResultOr<Option<RowKey>> {
        let neighbors = match self.scan.arrival.as_mut() {
            Some((Arrival::Sought { neighbors, .. } | Arrival::Probed { neighbors, .. }, _)) => {
                neighbors
            }
            _ => unreachable!("only a seek or a probe looks for the neighbors of its key"),
        };
        if let Some(found) = neighbors.on(side).clone() {
            return Ok(IOResult::Done(found));
        }
        let found = return_if_io!(self.neighbor_of(key, op, after));
        if let Some((Arrival::Sought { neighbors, .. } | Arrival::Probed { neighbors, .. }, _)) =
            self.scan.arrival.as_mut()
        {
            *neighbors.on(side) = Some(found.clone());
        }
        Ok(IOResult::Done(found))
    }

    fn expect_the_row(
        &mut self,
        row: RowKey,
        direction: IterationDirection,
        below: Below,
    ) -> IOResultOr<Arrived> {
        let row_locks = self.row_locks_in_force();
        let reads_past_a_held_row = row_locks.reads_past_held_rows
            && !matches!(self.scan.arrival, Some((Arrival::Probed { .. }, _)))
            && !matches!(self.scan.arrival, Some((Arrival::Sought { op, .. }, _)) if op.eq_only());
        self.scan.pending = Some(PendingRow {
            row,
            direction,
            below,
            reads_past_a_held_row,
        });
        let range_end_checked_before_locking = match row_locks.range_end_check {
            RangeEndCheck::None => false,
            RangeEndCheck::Equality => true,
            RangeEndCheck::Range => row_locks.primary && direction == IterationDirection::Forwards,
        };
        if range_end_checked_before_locking {
            return Ok(IOResult::Done(Arrived::OnARow));
        }
        match return_if_io!(self.lock_the_pending_row()) {
            RecordLock::Taken => Ok(IOResult::Done(Arrived::OnARow)),
            RecordLock::HeldByAnother => Ok(IOResult::Done(Arrived::PastAHeldRow)),
        }
    }

    fn lock_the_pending_row(&mut self) -> IOResultOr<RecordLock> {
        let pending = self.scan.pending.clone().expect("a row waits for its lock");
        let taken = self.lock_the_record(&pending.row, true, pending.reads_past_a_held_row)?;
        if taken == RecordLock::Taken {
            let row_locks = self.row_locks_in_force();
            match pending.direction {
                IterationDirection::Forwards if !matches!(pending.below, Below::Nothing) => {
                    let (low, start) = return_if_io!(self.gap_below_the_pending_row());
                    let skips = row_locks.policy == RowLockWaitPolicy::SkipLocked;
                    self.lock_the_gap(low, Some(pending.row.clone()), |key| {
                        match start.place(key) {
                            GapKey::Met if skips => GapKey::Ignored,
                            place => place,
                        }
                    })?;
                }
                IterationDirection::Backwards
                    if row_locks.level == RowLockLevel::RepeatableRead =>
                {
                    let low = return_if_io!(self.neighbor_of(
                        &pending.row,
                        SeekOp::LT,
                        AfterTheSearch::ReturnTo(
                            pending.row.clone(),
                            SeekOp::LE { eq_only: false }
                        ),
                    ));
                    self.lock_the_gap(low, Some(pending.row.clone()), |_| GapKey::BelowTheScan)?;
                }
                IterationDirection::Forwards | IterationDirection::Backwards => {}
            }
        }
        self.scan.pending = None;
        Ok(IOResult::Done(taken))
    }

    fn gap_below_the_pending_row(&mut self) -> IOResultOr<(Option<RowKey>, ScanStart)> {
        let pending = self.scan.pending.clone().expect("a row waits for its lock");
        match pending.below {
            Below::Known(low, start) => Ok(IOResult::Done((low, start))),
            Below::Nothing => Ok(IOResult::Done((
                Some(pending.row.clone()),
                ScanStart::Everywhere,
            ))),
            Below::Unknown(start) => {
                let low = return_if_io!(self.neighbor_of(
                    &pending.row,
                    SeekOp::LT,
                    AfterTheSearch::ReturnTo(pending.row.clone(), SeekOp::GE { eq_only: false }),
                ));
                if let Some(pending) = self.scan.pending.as_mut() {
                    pending.below = Below::Known(low.clone(), start.clone());
                }
                Ok(IOResult::Done((low, start)))
            }
        }
    }

    fn lock_the_gap(
        &self,
        low: Option<RowKey>,
        high: Option<RowKey>,
        place: impl Fn(&RowKey) -> GapKey,
    ) -> Result<()> {
        let row_locks = self.row_locks_in_force();
        self.db.lock_gap(
            self.tx_id,
            GapBetween {
                table_id: self.table_id,
                low,
                high,
            },
            place,
            row_locks.level == RowLockLevel::RepeatableRead,
        )
    }

    fn try_the_record_without_keeping_it(&mut self, row: &RowKey) -> Result<()> {
        let id = RowID::new(self.table_id, row.clone());
        let mode = self.row_locks_in_force().mode;
        match self.db.lock_row_for_read(self.tx_id, &id, mode)? {
            RowLockAttempt::Locked { newly_held } => {
                if newly_held {
                    self.db.unlock_row(self.tx_id, &id);
                }
                Ok(())
            }
            RowLockAttempt::HeldBy(holders) => Err(LimboError::RowLocked(holders)),
            RowLockAttempt::ChangedSinceRead => Err(LimboError::RowLocked(Vec::new())),
        }
    }

    fn lock_the_record(
        &mut self,
        row: &RowKey,
        with_the_table_row: bool,
        reads_past_a_held_row: bool,
    ) -> Result<RecordLock> {
        let row_locks = self.row_locks_in_force();
        let id = RowID::new(self.table_id, row.clone());
        if self.lock_one_record(id.clone(), row_locks, reads_past_a_held_row)?
            == RecordLock::HeldByAnother
        {
            return Ok(RecordLock::HeldByAnother);
        }
        if with_the_table_row && row_locks.mode == RowLockMode::Exclusive {
            if let Some(table_row) = self.table_row_of_index_entry(&id, row_locks) {
                return self.lock_one_record(table_row, row_locks, reads_past_a_held_row);
            }
        }
        Ok(RecordLock::Taken)
    }

    fn lock_one_record(
        &mut self,
        id: RowID,
        row_locks: CursorRowLocks,
        reads_past_a_held_row: bool,
    ) -> Result<RecordLock> {
        match self.db.lock_row_for_read(self.tx_id, &id, row_locks.mode)? {
            RowLockAttempt::Locked { newly_held } => {
                if newly_held {
                    self.scan.current.newly_locked.push(id);
                }
                Ok(RecordLock::Taken)
            }
            RowLockAttempt::HeldBy(holders) => match row_locks.policy {
                RowLockWaitPolicy::SkipLocked => Ok(RecordLock::HeldByAnother),
                RowLockWaitPolicy::Wait | RowLockWaitPolicy::NoWait
                    if reads_past_a_held_row && !holders.is_empty() =>
                {
                    self.scan.current.held_by = Some(holders);
                    Ok(RecordLock::Taken)
                }
                RowLockWaitPolicy::Wait | RowLockWaitPolicy::NoWait => {
                    Err(LimboError::RowLocked(holders))
                }
            },
            RowLockAttempt::ChangedSinceRead => Err(LimboError::RowLocked(Vec::new())),
        }
    }

    fn neighbor_of(
        &mut self,
        key: &RowKey,
        op: SeekOp,
        after: AfterTheSearch,
    ) -> IOResultOr<Option<RowKey>> {
        loop {
            match self.scan.search.take() {
                None => self.scan.search = Some(NeighborSearch::Seeking),
                Some(NeighborSearch::Seeking) => {
                    let found = match self.seek_row(seek_key_of(key), op) {
                        Ok(IOResult::Done(found)) => found,
                        Ok(IOResult::IO(io)) => {
                            self.scan.search = Some(NeighborSearch::Seeking);
                            return Ok(IOResult::IO(io));
                        }
                        Err(err) => return Err(err),
                    };
                    let neighbor = match found {
                        SeekResult::Found => self.current_key(),
                        _ => None,
                    };
                    match &after {
                        AfterTheSearch::ReturnTo(..) => {
                            self.scan.search = Some(NeighborSearch::Returning(neighbor));
                        }
                        AfterTheSearch::LeaveTheRows(direction) => {
                            self.move_off_the_rows(*direction);
                            return Ok(IOResult::Done(neighbor));
                        }
                    }
                }
                Some(NeighborSearch::Returning(neighbor)) => {
                    let AfterTheSearch::ReturnTo(row, back) = &after else {
                        unreachable!("only a search that returns goes back to its row");
                    };
                    match self.seek_row(seek_key_of(row), *back) {
                        Ok(IOResult::Done(found)) => {
                            turso_assert!(
                                found == SeekResult::Found,
                                "the cursor found its row again after looking at its neighbor"
                            );
                        }
                        Ok(IOResult::IO(io)) => {
                            self.scan.search = Some(NeighborSearch::Returning(neighbor));
                            return Ok(IOResult::IO(io));
                        }
                        Err(err) => return Err(err),
                    }
                    return Ok(IOResult::Done(neighbor));
                }
            }
        }
    }

    fn move_off_the_rows(&mut self, direction: IterationDirection) {
        let _ = self.table_iterator.take();
        let _ = self.index_iterator.take();
        self.reset_dual_peek();
        self.invalidate_record();
        self.current_pos = match direction {
            IterationDirection::Forwards => CursorPosition::End,
            IterationDirection::Backwards => CursorPosition::BeforeFirst,
        };
    }
}

impl ScanStart {
    fn place(&self, key: &RowKey) -> GapKey {
        let ScanStart::Sought { key: start, op } = self else {
            return GapKey::Met;
        };
        let order = key.cmp(start);
        match op {
            SeekOp::GE { eq_only: true } | SeekOp::LE { eq_only: true } => match order {
                std::cmp::Ordering::Less => GapKey::BelowTheScan,
                std::cmp::Ordering::Equal => GapKey::Met,
                std::cmp::Ordering::Greater => GapKey::AboveTheScan,
            },
            SeekOp::GE { .. } if order.is_lt() => GapKey::BelowTheScan,
            SeekOp::GT if order.is_le() => GapKey::BelowTheScan,
            SeekOp::LE { .. } if order.is_gt() => GapKey::AboveTheScan,
            SeekOp::LT if order.is_ge() => GapKey::AboveTheScan,
            SeekOp::GE { .. } | SeekOp::GT | SeekOp::LE { .. } | SeekOp::LT => GapKey::Met,
        }
    }
}

fn row_key_is(row: &RowKey, start: &RowKey) -> bool {
    match (row, start) {
        (RowKey::Int(row), RowKey::Int(start)) => row == start,
        (RowKey::Record(row), RowKey::Record(start)) => {
            let unique_columns = row
                .metadata
                .num_cols
                .saturating_sub(usize::from(row.metadata.has_rowid));
            start.metadata.num_cols >= unique_columns && row.as_ref() == start.as_ref()
        }
        _ => false,
    }
}

fn seek_key_of(key: &RowKey) -> SeekKey<'_> {
    match key {
        RowKey::Int(rowid) => SeekKey::TableRowId(*rowid),
        RowKey::Record(record) => SeekKey::IndexKey(record.key.clone()),
    }
}
