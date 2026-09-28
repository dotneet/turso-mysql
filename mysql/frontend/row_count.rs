//! `ROW_COUNT()` read inside a statement: the rows the statement before it
//! changed, which the session knows and the engine does not.
//!
//! The frontend sets the count on the connection before each statement, and
//! the statement reads it through `mysql_row_count()`. A count the session
//! does not know refuses the statement that reads it.

use turso_core::{Connection, LimboError, Result, Value};

use crate::call_state::with_call_state;

pub(crate) const MYSQL_ROW_COUNT: &str = "mysql_row_count";

/// `mysql_row_count()`: the count set for the statement running.
pub(crate) fn read(connection: &Connection) -> Result<Value> {
    with_call_state(connection, |state| state.row_count)
        .map(Value::from_i64)
        .ok_or_else(|| {
            LimboError::InternalError(format!(
                "{MYSQL_ROW_COUNT} read where the session does not know the count"
            ))
        })
}

pub(crate) fn set(connection: &Connection, count: Option<i64>) {
    with_call_state(connection, |state| state.row_count = count);
}
