//! Answering a `SELECT` of calls on MySQL's named locks.
//!
//! None of them reads a table or a transaction's snapshot: each asks the
//! server-wide lock table, so the answer is built here rather than by the
//! engine.

use super::*;
use turso_mysql::named_locks::{MySqlLockWait, MySqlNamedLockError, MySqlNamedLockSession};
use turso_mysql_parser::{MySqlNamedLockFunction, MySqlNamedLockQuery};

/// The longest name a named lock may have, in characters. Measured on MySQL
/// 8.4.11: 64 is taken and 65 answers 4163.
const LONGEST_LOCK_NAME: usize = 64;

/// Makes the calls one after another, as MySQL does, and answers one row.
///
/// Measured on MySQL 8.4.11: `GET_LOCK`, `RELEASE_LOCK` and `IS_FREE_LOCK`
/// answer a LONGLONG of length 1 with the binary and numeric flags, and
/// `RELEASE_ALL_LOCKS` one of length 21 that is also unsigned and NOT NULL.
pub(super) fn named_lock_result(
    query: &MySqlNamedLockQuery,
    locks: &MySqlNamedLockSession,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let mut columns = Vec::with_capacity(query.calls().len());
    let mut row = Vec::with_capacity(query.calls().len());
    for call in query.calls() {
        let mut column =
            ColumnDefinitionConfig::new(call.column_name().to_owned(), MYSQL_TYPE_LONGLONG);
        column.character_set = MYSQL_BINARY_COLLATION;
        column.decimals = 0;
        column.column_length = 1;
        column.flags = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
        let value = match call.function() {
            MySqlNamedLockFunction::Get {
                name,
                timeout_seconds,
            } => {
                let name = checked_lock_name(name.as_deref())?;
                // Measured on MySQL 8.4.11: a negative timeout waits without
                // end.
                let wait = u64::try_from(*timeout_seconds)
                    .map_or(MySqlLockWait::Forever, |seconds| {
                        MySqlLockWait::For(Duration::from_secs(seconds))
                    });
                let taken = locks.get(name, wait).map_err(|error| match error {
                    MySqlNamedLockError::Deadlock => FrontendErrorKind::UserLockDeadlock,
                })?;
                Some(u8::from(taken).to_string())
            }
            MySqlNamedLockFunction::Release { name } => locks
                .release(checked_lock_name(name.as_deref())?)
                .map(|released| u8::from(released).to_string()),
            MySqlNamedLockFunction::IsFree { name } => {
                Some(u8::from(locks.is_free(checked_lock_name(name.as_deref())?)).to_string())
            }
            // It answers the holder's connection ID, which this server does
            // not hand out.
            MySqlNamedLockFunction::IsUsed { .. } => return Err(FrontendErrorKind::Unsupported),
            MySqlNamedLockFunction::ReleaseAll => {
                column.column_length = 21;
                column.flags |= MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG;
                Some(locks.release_all().to_string())
            }
        };
        columns.push(column);
        row.push(value.map(String::into_bytes));
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows: vec![row],
        warnings: 0,
        status_flags,
    }))
}

/// Measured on MySQL 8.4.11: an empty or NULL name answers 3057, and one
/// longer than 64 characters 4163.
fn checked_lock_name(name: Option<&str>) -> Result<&str, FrontendErrorKind> {
    match name {
        None | Some("") => Err(FrontendErrorKind::IncorrectUserLockName),
        Some(name) if name.chars().count() > LONGEST_LOCK_NAME => {
            Err(FrontendErrorKind::UserLockNameTooLong)
        }
        Some(name) => Ok(name),
    }
}
