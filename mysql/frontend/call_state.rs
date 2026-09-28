//! What MySQL's own calls keep on the engine connection while a statement
//! runs: the engine gives each connection one slot for it, and more than one
//! call needs it.

use turso_core::Connection;

use crate::group_concat::GroupConcatProgress;

#[derive(Default)]
pub(crate) struct MySqlCallState {
    pub(crate) group_concat: GroupConcatProgress,
    /// The rows the last `SQL_CALC_FOUND_ROWS` statement would have answered
    /// without its `LIMIT`, until the session reads them.
    pub(crate) found_rows_before_the_limit: Option<u64>,
}

pub(crate) fn with_call_state<T>(
    connection: &Connection,
    read: impl FnOnce(&mut MySqlCallState) -> T,
) -> T {
    let mut state = connection.mysql_function_state();
    let state = state.get_or_insert_with(|| Box::new(MySqlCallState::default()));
    let state = state
        .downcast_mut::<MySqlCallState>()
        .expect("only MySQL's calls keep state on a MySQL connection");
    read(state)
}
