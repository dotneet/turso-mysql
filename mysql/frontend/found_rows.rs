//! `SQL_CALC_FOUND_ROWS`: how many rows a `SELECT` would have answered
//! without its `LIMIT`, which `FOUND_ROWS()` reads next.
//!
//! The statement's `LIMIT` is rendered as
//! `mysql_note_found_rows((SELECT COUNT(*) FROM (<the statement without its
//! ORDER BY and LIMIT>)), <limit>)`, which the engine works out once, before
//! the first row, inside the statement's own read of the database. So the
//! count and the rows answered come from the same rows, as MySQL's do.

use turso_core::{Connection, LimboError, Numeric, Result, Value};

use crate::call_state::with_call_state;

pub(crate) const MYSQL_NOTE_FOUND_ROWS: &str = "mysql_note_found_rows";

/// `mysql_note_found_rows(count, limit)`: notes the count and answers the
/// limit it was given.
pub(crate) fn note(connection: &Connection, args: &[Value]) -> Result<Value> {
    let [Value::Numeric(Numeric::Integer(count)), limit @ Value::Numeric(Numeric::Integer(_))] =
        args
    else {
        return Err(LimboError::InternalError(format!(
            "{MYSQL_NOTE_FOUND_ROWS} takes a count of rows and a limit"
        )));
    };
    let count = u64::try_from(*count).map_err(|_| {
        LimboError::InternalError(format!(
            "{MYSQL_NOTE_FOUND_ROWS} counted fewer than no rows"
        ))
    })?;
    with_call_state(connection, |state| {
        state.found_rows_before_the_limit = Some(count);
    });
    Ok(limit.clone())
}

/// Takes what the last `SQL_CALC_FOUND_ROWS` statement noted, if one did.
pub(crate) fn take(connection: &Connection) -> Option<u64> {
    with_call_state(connection, |state| state.found_rows_before_the_limit.take())
}

pub(crate) fn noted(connection: &Connection) -> bool {
    with_call_state(connection, |state| {
        state.found_rows_before_the_limit.is_some()
    })
}
