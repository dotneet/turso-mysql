use std::any::Any;
use std::cmp::Ordering;

use crate::mvcc::cursor::{RangeEnd, RecordLock};
use crate::mvcc::database::RowKey;
use crate::numeric::Numeric;
use crate::storage::pager::Pager;
use crate::sync::Arc;
use crate::types::{compare_record, Cursor, IOResult, IOResultOr, IndexInfo, Value};
use crate::vdbe::execute::InsnResult;
use crate::vdbe::{
    CursorID, Insn, InsnFunctionStepResult, InsnReference, Program, ProgramState, RowLockPoint,
};
use crate::{return_if_io, LimboError, MvCursor, Result};

pub(crate) enum RowLockWork {
    PassedTheRangeEnd {
        cursor_id: CursorID,
        unique_equality: bool,
        skipped_rows_go_to: InsnReference,
    },
    ReachedTheRangeEnd {
        cursor_id: CursorID,
        equality: bool,
        bound: RangeBound,
    },
    FoundADuplicate {
        cursor_id: CursorID,
    },
}

pub(crate) fn step_with_row_locks(
    program: &Program,
    state: &mut ProgramState,
    insn: &Insn,
    pager: &Arc<Pager>,
) -> InsnResult {
    if let Some(work) = state.row_lock_work.take() {
        match do_the_work(state, &work) {
            Ok(IOResult::Done(RowAfterTheWork::Kept)) => {}
            Ok(IOResult::Done(RowAfterTheWork::Skipped(pc))) => {
                state.pc = pc;
                return Ok(InsnFunctionStepResult::Step);
            }
            Ok(IOResult::IO(io)) => {
                state.row_lock_work = Some(work);
                return Ok(state.suspend_on_io(io));
            }
            Err(err) => return Err(err),
        }
    }
    let pc = state.pc as usize;
    if matches!(insn, Insn::Halt { .. }) {
        let_go_of_rows_that_did_not_match(state);
    }
    before_the_instruction(program, state, insn, pc)?;
    let result = insn.to_function()(program, state, insn, pager);
    if let Ok(InsnFunctionStepResult::Step) = result {
        after_the_instruction(program, state, insn, pc);
    }
    result
}

enum RowAfterTheWork {
    Kept,
    Skipped(InsnReference),
}

fn do_the_work(state: &mut ProgramState, work: &RowLockWork) -> IOResultOr<RowAfterTheWork> {
    match work {
        RowLockWork::PassedTheRangeEnd {
            cursor_id,
            unique_equality,
            skipped_rows_go_to,
        } => {
            let Some(cursor) = mvcc_cursor(state, *cursor_id) else {
                return Ok(IOResult::Done(RowAfterTheWork::Kept));
            };
            match return_if_io!(cursor.passed_the_range_end_check(*unique_equality)) {
                RecordLock::Taken => Ok(IOResult::Done(RowAfterTheWork::Kept)),
                RecordLock::HeldByAnother => Ok(IOResult::Done(RowAfterTheWork::Skipped(
                    *skipped_rows_go_to,
                ))),
            }
        }
        RowLockWork::ReachedTheRangeEnd {
            cursor_id,
            equality,
            bound,
        } => {
            let Some(cursor) = mvcc_cursor(state, *cursor_id) else {
                return Ok(IOResult::Done(RowAfterTheWork::Kept));
            };
            let index_info = cursor.index_info_of_the_scan();
            let holds = |key: &RowKey| bound.holds(key, index_info.as_deref());
            let is_the_last_key_inside =
                |key: &RowKey| bound.is_the_last_key_inside(key, index_info.as_deref());
            return_if_io!(cursor.reached_the_range_end(&RangeEnd {
                equality: *equality,
                is_the_last_key_inside: &is_the_last_key_inside,
                holds: &holds,
            }));
            Ok(IOResult::Done(RowAfterTheWork::Kept))
        }
        RowLockWork::FoundADuplicate { cursor_id } => {
            if let Some(cursor) = mvcc_cursor(state, *cursor_id) {
                return_if_io!(cursor.lock_the_duplicate());
            }
            Ok(IOResult::Done(RowAfterTheWork::Kept))
        }
    }
}

fn let_go_of_rows_that_did_not_match(state: &mut ProgramState) {
    for cursor_id in 0..state.cursors.len() {
        if let Some(cursor) = mvcc_cursor(state, cursor_id) {
            cursor.let_go_of_a_row_that_did_not_match();
        }
    }
}

fn before_the_instruction(
    program: &Program,
    state: &mut ProgramState,
    insn: &Insn,
    pc: usize,
) -> Result<(), Box<LimboError>> {
    if let Some(cursor_id) = positions_to_write(insn) {
        if let Some(cursor) = mvcc_cursor(state, cursor_id) {
            cursor.position_to_write(true);
        }
    }
    for (_, point) in points_at(program, pc) {
        match *point {
            RowLockPoint::ReadsTheRangeEnd { cursor_id } => {
                if let Some(cursor) = mvcc_cursor(state, cursor_id) {
                    cursor.read_the_range_end(true);
                }
            }
            RowLockPoint::ChecksTheRangeEnd { cursor_id, .. } => {
                if reads_its_cursor(insn) {
                    if let Some(cursor) = mvcc_cursor(state, cursor_id) {
                        cursor.read_the_range_end(true);
                    }
                }
            }
            RowLockPoint::RowsMatched { cursor_id } => {
                if let Some(cursor) = mvcc_cursor(state, cursor_id) {
                    cursor.row_matched()?;
                }
            }
        }
    }
    Ok(())
}

fn after_the_instruction(program: &Program, state: &mut ProgramState, insn: &Insn, pc: usize) {
    if let Some(cursor_id) = positions_to_write(insn) {
        if let Some(cursor) = mvcc_cursor(state, cursor_id) {
            cursor.position_to_write(false);
        }
        let found_a_duplicate = matches!(insn, Insn::NotExists { .. } | Insn::NoConflict { .. })
            && state.pc as usize == pc + 1;
        if found_a_duplicate {
            state.row_lock_work = Some(RowLockWork::FoundADuplicate { cursor_id });
        }
    }
    for (_, point) in points_at(program, pc) {
        match *point {
            RowLockPoint::ReadsTheRangeEnd { cursor_id } => {
                if let Some(cursor) = mvcc_cursor(state, cursor_id) {
                    cursor.read_the_range_end(false);
                }
            }
            RowLockPoint::ChecksTheRangeEnd {
                cursor_id,
                equality,
                unique_equality,
            } => {
                if let Some(cursor) = mvcc_cursor(state, cursor_id) {
                    cursor.read_the_range_end(false);
                }
                let stopped = state.pc as usize != pc + 1;
                state.row_lock_work = Some(if stopped {
                    RowLockWork::ReachedTheRangeEnd {
                        cursor_id,
                        equality,
                        bound: RangeBound::of(state, insn),
                    }
                } else {
                    RowLockWork::PassedTheRangeEnd {
                        cursor_id,
                        unique_equality,
                        skipped_rows_go_to: where_skipped_rows_go(program, insn, pc, cursor_id),
                    }
                });
            }
            RowLockPoint::RowsMatched { .. } => {}
        }
    }
}

fn where_skipped_rows_go(
    program: &Program,
    range_end_check: &Insn,
    pc: usize,
    cursor_id: CursorID,
) -> InsnReference {
    let next_row = program.insns[pc + 1..]
        .iter()
        .position(|(insn, _)| {
            matches!(
                insn,
                Insn::Next { cursor_id: moved, .. } | Insn::Prev { cursor_id: moved, .. }
                    if *moved == cursor_id
            )
        })
        .map(|offset| (pc + 1 + offset) as InsnReference);
    next_row.unwrap_or_else(|| match range_end_check {
        Insn::Gt { target_pc, .. }
        | Insn::Ge { target_pc, .. }
        | Insn::Lt { target_pc, .. }
        | Insn::Le { target_pc, .. }
        | Insn::IdxGT { target_pc, .. }
        | Insn::IdxGE { target_pc, .. }
        | Insn::IdxLT { target_pc, .. }
        | Insn::IdxLE { target_pc, .. } => target_pc.as_offset_int(),
        _ => unreachable!("a range end is checked by a comparison: {range_end_check:?}"),
    })
}

type StopsAt = fn(Ordering) -> bool;

pub(crate) enum RangeBound {
    Rowid {
        bound: Value,
        stop: fn(Ordering) -> bool,
    },
    Index {
        bound: Vec<Value>,
        insn: Insn,
    },
}

impl RangeBound {
    fn of(state: &ProgramState, insn: &Insn) -> Self {
        match insn {
            Insn::Gt { rhs, .. } => Self::rowid(state, *rhs, Ordering::is_gt),
            Insn::Ge { rhs, .. } => Self::rowid(state, *rhs, Ordering::is_ge),
            Insn::Lt { rhs, .. } => Self::rowid(state, *rhs, Ordering::is_lt),
            Insn::Le { rhs, .. } => Self::rowid(state, *rhs, Ordering::is_le),
            Insn::IdxGT {
                start_reg,
                num_regs,
                ..
            }
            | Insn::IdxGE {
                start_reg,
                num_regs,
                ..
            }
            | Insn::IdxLT {
                start_reg,
                num_regs,
                ..
            }
            | Insn::IdxLE {
                start_reg,
                num_regs,
                ..
            } => Self::Index {
                bound: state.registers[*start_reg..*start_reg + *num_regs]
                    .iter()
                    .map(|register| register.get_value().clone())
                    .collect(),
                insn: insn.clone(),
            },
            _ => unreachable!("a range end is checked by a comparison: {insn:?}"),
        }
    }

    fn rowid(state: &ProgramState, register: usize, stop: StopsAt) -> Self {
        Self::Rowid {
            bound: state.registers[register].get_value().clone(),
            stop,
        }
    }

    fn holds(&self, key: &RowKey, index_info: Option<&IndexInfo>) -> bool {
        match self.order_against_the_bound(key, index_info, true) {
            Some((order, stop)) => !stop(order),
            None => true,
        }
    }

    fn is_the_last_key_inside(&self, key: &RowKey, index_info: Option<&IndexInfo>) -> bool {
        match self.order_against_the_bound(key, index_info, false) {
            Some((order, stop)) => order.is_eq() && !stop(order),
            None => false,
        }
    }

    fn order_against_the_bound(
        &self,
        key: &RowKey,
        index_info: Option<&IndexInfo>,
        with_the_tie_breaker: bool,
    ) -> Option<(Ordering, StopsAt)> {
        match (self, key) {
            (Self::Rowid { bound, stop }, RowKey::Int(rowid)) => {
                let Value::Numeric(Numeric::Integer(bound)) = bound else {
                    return None;
                };
                let order = rowid.cmp(bound);
                Some((order, *stop))
            }
            (Self::Index { bound, insn }, RowKey::Record(record)) => {
                let index_info = index_info?;
                let (tie_breaker, stop): (Ordering, StopsAt) = match insn {
                    Insn::IdxGT { .. } => (Ordering::Less, Ordering::is_gt),
                    Insn::IdxGE { .. } => (Ordering::Equal, Ordering::is_ge),
                    Insn::IdxLT { .. } => (Ordering::Equal, Ordering::is_lt),
                    Insn::IdxLE { .. } => (Ordering::Less, Ordering::is_le),
                    _ => unreachable!("an index range end is checked by an index comparison"),
                };
                let tie_breaker = if with_the_tie_breaker {
                    tie_breaker
                } else {
                    Ordering::Equal
                };
                let registers: Vec<_> = bound.iter().map(Value::as_ref).collect();
                let order = compare_record(
                    record.key.get_payload(),
                    registers.iter().copied(),
                    index_info,
                    tie_breaker,
                )
                .ok()?;
                Some((order, stop))
            }
            _ => None,
        }
    }
}

fn points_at(program: &Program, pc: usize) -> &[(u32, RowLockPoint)] {
    let points = &program.row_lock_points;
    let first = points.partition_point(|(point_pc, _)| (*point_pc as usize) < pc);
    let after = points.partition_point(|(point_pc, _)| (*point_pc as usize) <= pc);
    &points[first..after]
}

fn positions_to_write(insn: &Insn) -> Option<CursorID> {
    match insn {
        Insn::NotExists { cursor, .. }
        | Insn::Insert { cursor, .. }
        | Insn::NewRowid { cursor, .. } => Some(*cursor),
        Insn::IdxDelete { cursor_id, .. }
        | Insn::IdxInsert { cursor_id, .. }
        | Insn::Delete { cursor_id, .. }
        | Insn::NoConflict { cursor_id, .. }
        | Insn::Found { cursor_id, .. }
        | Insn::NotFound { cursor_id, .. } => Some(*cursor_id),
        _ => None,
    }
}

fn reads_its_cursor(insn: &Insn) -> bool {
    matches!(
        insn,
        Insn::IdxGT { .. } | Insn::IdxGE { .. } | Insn::IdxLT { .. } | Insn::IdxLE { .. }
    )
}

fn mvcc_cursor(state: &mut ProgramState, cursor_id: CursorID) -> Option<&mut MvCursor> {
    match state.cursors.get_mut(cursor_id)? {
        Some(cursor @ (Cursor::BTree(..) | Cursor::Dyn(..))) => {
            let cursor = cursor.as_btree_mut() as &mut dyn Any;
            cursor.downcast_mut::<MvCursor>()
        }
        _ => None,
    }
}
