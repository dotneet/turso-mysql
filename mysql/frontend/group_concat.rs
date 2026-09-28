//! MySQL's `GROUP_CONCAT`, joined from the rows the engine gathered and cut
//! where the session's `group_concat_max_len` says.
//!
//! The engine gathers each group's rows with its own `group_concat`, every
//! row written as `N` for a NULL or `V<bytes>:<value>` for a value, so that
//! nothing a value holds can be mistaken for the separator. This joins them
//! the way MySQL does and cuts the result where MySQL cuts it. Measured on
//! MySQL 8.4.11:
//!
//! - the cut is in bytes and never splits a character: at 4, `aéé` becomes
//!   `aé`, three bytes;
//! - a separator counts like a value, and the cut can leave one in place:
//!   at 4, `aé,bb` becomes `aé,`;
//! - the warning (1260, "Row N was cut by GROUP_CONCAT()") numbers the value
//!   the cut fell in by counting what that call has joined across every
//!   group of the statement, NULLs and the values after a cut left out;
//! - a statement writing the result, `INSERT ... SELECT`, fails with the
//!   same 1260 instead, under the strict mode this server runs;
//! - `ORDER BY` orders the parts before they are joined, so the cut and the
//!   row a warning names count in that order: at 4, three rows ordered by
//!   `n DESC` into `b,a,A` are cut in the third, `b,a,`. A NULL it orders by
//!   comes first, and last from the last.
//!
//! An ordered call gathers each row's column to order by before its value,
//! `Z` for a NULL or `K<bytes>:<key>`.

use std::collections::HashMap;

use turso_core::{Connection, LimboError, Result, Value};

use crate::call_state::with_call_state;

pub(crate) const MYSQL_GROUP_CONCAT: &str = "mysql_group_concat";
pub(crate) const MYSQL_GROUP_CONCAT_COUNT: &str = "mysql_group_concat_count";

/// The first words of the error a cut raises in a statement that writes the
/// result, which the caller answers as MySQL's 1260.
pub const GROUP_CONCAT_CUT_ERROR: &str = "GROUP_CONCAT cut row ";

/// The limit a session starts with. Measured on MySQL 8.4.11.
pub const DEFAULT_GROUP_CONCAT_MAX_LEN: u64 = 1024;

/// What a session's `GROUP_CONCAT` calls keep between groups, left on the
/// engine connection because each call is worked out once per group.
pub(crate) struct GroupConcatProgress {
    max_len: u64,
    calls: HashMap<i64, CallProgress>,
    cuts: Vec<Cut>,
}

impl Default for GroupConcatProgress {
    fn default() -> Self {
        Self {
            max_len: DEFAULT_GROUP_CONCAT_MAX_LEN,
            calls: HashMap::new(),
            cuts: Vec::new(),
        }
    }
}

#[derive(Default)]
struct CallProgress {
    joined: u64,
    groups: u64,
}

/// One value cut short, and where MySQL raises its warning among the others.
struct Cut {
    /// Which of the call's groups it was, counting from zero.
    group: u64,
    /// Which row of the group the cut fell at, counting from one. MySQL
    /// warns as it reads the rows, so two calls cut in one group warn in the
    /// order their rows were read.
    row_in_group: u64,
    call: i64,
    /// The row MySQL's warning names.
    row: u64,
}

/// Sets the limit a `GROUP_CONCAT` is cut at from now on.
pub(crate) fn set_max_len(connection: &Connection, max_len: u64) {
    with_progress(connection, |progress| progress.max_len = max_len);
}

pub(crate) fn max_len(connection: &Connection) -> u64 {
    with_progress(connection, |progress| progress.max_len)
}

/// Starts counting afresh for the statement about to run.
pub(crate) fn forget_progress(connection: &Connection) {
    with_progress(connection, |progress| {
        progress.calls.clear();
        progress.cuts.clear();
    });
}

/// The rows the warnings of the statement that ran name, in the order MySQL
/// raises them.
pub(crate) fn take_cut_rows(connection: &Connection) -> Vec<u64> {
    let mut cuts = with_progress(connection, |progress| {
        progress.calls.clear();
        std::mem::take(&mut progress.cuts)
    });
    cuts.sort_by_key(|cut| (cut.group, cut.row_in_group, cut.call));
    cuts.into_iter().map(|cut| cut.row).collect()
}

fn with_progress<T>(
    connection: &Connection,
    read: impl FnOnce(&mut GroupConcatProgress) -> T,
) -> T {
    with_call_state(connection, |state| read(&mut state.group_concat))
}

/// `mysql_group_concat(gathered, separator, order, call, on_cut)`, and
/// `mysql_group_concat_count` with the same arguments.
///
/// `order` is 0 for parts joined as the engine read them, 1 and 2 for parts
/// ordered by words under `utf8mb4_0900_ai_ci` from the first and from the
/// last, and 3 and 4 for parts ordered by whole numbers. `call` tells the
/// calls of one statement apart. `on_cut` is 1 for a call
/// that warns about a cut, 2 for one that fails its statement instead, and 0
/// for the same call named again in a `HAVING` or an `ORDER BY`, which
/// MySQL does not count or warn about twice.
///
/// `mysql_group_concat_count` counts the group and answers true. A `HAVING`
/// tests it first, because the engine works out the projection only for
/// the groups the `HAVING` keeps and MySQL counts and warns about every
/// group.
pub(crate) fn call(connection: &Connection, name: &str, args: &[Value]) -> Result<Value> {
    let joined = join_the_group(connection, args)?;
    if name.eq_ignore_ascii_case(MYSQL_GROUP_CONCAT_COUNT) {
        return Ok(Value::from_i64(1));
    }
    Ok(joined)
}

fn join_the_group(connection: &Connection, args: &[Value]) -> Result<Value> {
    let [gathered, Value::Text(separator), Value::Numeric(turso_core::Numeric::Integer(order)), Value::Numeric(turso_core::Numeric::Integer(call)), Value::Numeric(turso_core::Numeric::Integer(on_cut))] =
        args
    else {
        return Err(LimboError::InternalError(format!(
            "{MYSQL_GROUP_CONCAT} takes the gathered rows, a separator, an order, a call and what a cut does"
        )));
    };
    let (call, counts, fails_on_cut) = (*call, *on_cut != 0, *on_cut == 2);
    // The engine gathers nothing only over no rows at all, which MySQL
    // answers NULL for.
    let gathered = match gathered {
        Value::Null => return Ok(Value::Null),
        Value::Text(gathered) => gathered.as_str(),
        _ => {
            return Err(LimboError::InternalError(format!(
                "{MYSQL_GROUP_CONCAT} was given gathered rows that are not text"
            )))
        }
    };
    let rows = match Order::named(*order)? {
        None => gathered_rows(gathered)?,
        Some(order) => ordered_rows(gathered, order)?,
    };
    let (joined, cut_row) = with_progress(connection, |progress| {
        let joined = join(&rows, separator.as_str(), progress.max_len);
        if !counts {
            return (joined, None);
        }
        let call_progress = progress.calls.entry(call).or_default();
        let group = call_progress.groups;
        call_progress.groups += 1;
        call_progress.joined += joined.values;
        let row = call_progress.joined;
        if let Some(row_in_group) = joined.cut_at {
            progress.cuts.push(Cut {
                group,
                row_in_group,
                call,
                row,
            });
        }
        let cut_row = joined.cut_at.map(|_| row);
        (joined, cut_row)
    });
    if let (true, Some(row)) = (fails_on_cut, cut_row) {
        return Err(LimboError::InvalidArgument(format!(
            "{GROUP_CONCAT_CUT_ERROR}{row}"
        )));
    }
    Ok(match joined.text {
        Some(text) => Value::build_text(text),
        None => Value::Null,
    })
}

/// Reads back what the engine gathered for one group: each row's value, or
/// `None` for a NULL.
fn gathered_rows(gathered: &str) -> Result<Vec<Option<&str>>> {
    let malformed = || {
        LimboError::InternalError(format!(
            "{MYSQL_GROUP_CONCAT} was given rows it did not gather"
        ))
    };
    let mut rows = Vec::new();
    let mut rest = gathered;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('N') {
            rows.push(None);
            rest = after;
            continue;
        }
        let after = rest.strip_prefix('V').ok_or_else(malformed)?;
        let (length, after) = after.split_once(':').ok_or_else(malformed)?;
        let length: usize = length.parse().map_err(|_| malformed())?;
        if !after.is_char_boundary(length) {
            return Err(malformed());
        }
        let (value, after) = after.split_at(length);
        rows.push(Some(value));
        rest = after;
    }
    Ok(rows)
}

/// What an ordered call orders its parts by.
#[derive(Clone, Copy)]
struct Order {
    by_number: bool,
    from_the_last: bool,
}

impl Order {
    fn named(order: i64) -> Result<Option<Self>> {
        let (by_number, from_the_last) = match order {
            0 => return Ok(None),
            1 => (false, false),
            2 => (false, true),
            3 => (true, false),
            4 => (true, true),
            _ => {
                return Err(LimboError::InternalError(format!(
                    "{MYSQL_GROUP_CONCAT} was given an order it does not know"
                )))
            }
        };
        Ok(Some(Self {
            by_number,
            from_the_last,
        }))
    }

    fn compare(self, first: Option<&str>, second: Option<&str>) -> Result<std::cmp::Ordering> {
        let ordering = match (first, second) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(first), Some(second)) if self.by_number => {
                whole_number(first)?.cmp(&whole_number(second)?)
            }
            (Some(first), Some(second)) => turso_core::mysql_uca9_compare(first, second),
        };
        Ok(if self.from_the_last {
            ordering.reverse()
        } else {
            ordering
        })
    }
}

fn whole_number(written: &str) -> Result<i128> {
    written.parse().map_err(|_| {
        LimboError::InternalError(format!(
            "{MYSQL_GROUP_CONCAT} was ordered by a number it cannot read"
        ))
    })
}

/// Reads back what an ordered call gathered for one group and puts the
/// values in its order.
///
/// Measured on MySQL 8.4.11, parts that tie in the order come out in an order
/// of MySQL's own — `a` before `A` under `utf8mb4_0900_ai_ci` whichever way
/// the order runs — which is not the order they were read in. So a group
/// whose tied parts are not all the same value is refused rather than joined
/// in an order MySQL might not join it in.
fn ordered_rows(gathered: &str, order: Order) -> Result<Vec<Option<&str>>> {
    let malformed = || {
        LimboError::InternalError(format!(
            "{MYSQL_GROUP_CONCAT} was given ordered rows it did not gather"
        ))
    };
    let mut rows = Vec::new();
    let mut rest = gathered;
    while !rest.is_empty() {
        let key = if let Some(after) = rest.strip_prefix('Z') {
            rest = after;
            None
        } else {
            let after = rest.strip_prefix('K').ok_or_else(malformed)?;
            let (key, after) = one_length_prefixed(after).ok_or_else(malformed)?;
            rest = after;
            Some(key)
        };
        let value = if let Some(after) = rest.strip_prefix('N') {
            rest = after;
            None
        } else {
            let after = rest.strip_prefix('V').ok_or_else(malformed)?;
            let (value, after) = one_length_prefixed(after).ok_or_else(malformed)?;
            rest = after;
            Some(value)
        };
        rows.push((key, value));
    }
    let mut failed = None;
    rows.sort_by(|first, second| match order.compare(first.0, second.0) {
        Ok(ordering) => ordering,
        Err(error) => {
            failed.get_or_insert(error);
            std::cmp::Ordering::Equal
        }
    });
    if let Some(error) = failed {
        return Err(error);
    }
    for tied in rows.chunk_by(|first, second| {
        order
            .compare(first.0, second.0)
            .is_ok_and(std::cmp::Ordering::is_eq)
    }) {
        let mut values = tied.iter().filter_map(|(_, value)| *value);
        if let Some(first) = values.next() {
            if values.any(|value| value != first) {
                return Err(LimboError::InvalidArgument(format!(
                    "{MYSQL_GROUP_CONCAT} would join values tied in its ORDER BY in an order MySQL may not join them in"
                )));
            }
        }
    }
    Ok(rows.into_iter().map(|(_, value)| value).collect())
}

/// Reads `<bytes>:<text>` off the front of `written`.
fn one_length_prefixed(written: &str) -> Option<(&str, &str)> {
    let (length, after) = written.split_once(':')?;
    let length: usize = length.parse().ok()?;
    if !after.is_char_boundary(length) {
        return None;
    }
    Some(after.split_at(length))
}

struct Joined {
    /// The result, or `None` when every row was NULL.
    text: Option<String>,
    /// How many values went into it, the one the cut fell in among them.
    values: u64,
    /// The row of the group the cut fell at, counting NULLs, if it was cut.
    cut_at: Option<u64>,
}

fn join(rows: &[Option<&str>], separator: &str, max_len: u64) -> Joined {
    let mut text = String::new();
    let mut values = 0;
    for (index, row) in rows.iter().enumerate() {
        let Some(value) = row else {
            continue;
        };
        if values > 0 {
            text.push_str(separator);
        }
        text.push_str(value);
        values += 1;
        if text.len() as u64 > max_len {
            let mut kept = usize::try_from(max_len).expect("a cut falls inside the text");
            while !text.is_char_boundary(kept) {
                kept -= 1;
            }
            text.truncate(kept);
            return Joined {
                text: Some(text),
                values,
                cut_at: Some(index as u64 + 1),
            };
        }
    }
    Joined {
        text: (values > 0).then_some(text),
        values,
        cut_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_never_splits_a_character() {
        let joined = join(&[Some("aéé")], ",", 4);
        assert_eq!(joined.text.as_deref(), Some("aé"));
        assert_eq!(joined.cut_at, Some(1));
    }

    #[test]
    fn a_separator_counts_like_a_value_and_can_be_left_at_the_end() {
        let joined = join(&[Some("aé"), Some("bb"), Some("cc")], ",", 4);
        assert_eq!(joined.text.as_deref(), Some("aé,"));
        assert_eq!(joined.values, 2);
        assert_eq!(joined.cut_at, Some(2));
    }

    #[test]
    fn a_result_exactly_as_long_as_the_limit_is_not_cut() {
        let joined = join(&[Some("cc"), None, Some("dd")], ",", 5);
        assert_eq!(joined.text.as_deref(), Some("cc,dd"));
        assert_eq!(joined.cut_at, None);
    }

    #[test]
    fn nulls_are_skipped_and_a_group_of_them_is_null() {
        assert_eq!(join(&[None, None], ",", 4).text, None);
        let joined = join(&[None, Some("x"), None, Some("yyyy")], ",", 4);
        assert_eq!(joined.text.as_deref(), Some("x,yy"));
        assert_eq!(joined.values, 2);
        assert_eq!(joined.cut_at, Some(4));
    }

    #[test]
    fn gathered_rows_keep_values_holding_the_markers() {
        assert_eq!(
            gathered_rows("V3:N:VNV0:V2:é").unwrap(),
            [Some("N:V"), None, Some(""), Some("é")]
        );
        assert!(gathered_rows("V9:ab").is_err());
        assert!(gathered_rows("V1:é").is_err());
    }

    #[test]
    fn ordered_rows_put_a_null_key_first_and_refuse_ties_of_different_values() {
        let by_number = Order::named(3).unwrap().unwrap();
        let from_the_last = Order::named(4).unwrap().unwrap();
        let gathered = "K2:10V1:bZV1:cK1:9V1:aK2:10V1:b";
        assert_eq!(
            ordered_rows(gathered, by_number).unwrap(),
            [Some("c"), Some("a"), Some("b"), Some("b")]
        );
        assert_eq!(
            ordered_rows(gathered, from_the_last).unwrap(),
            [Some("b"), Some("b"), Some("a"), Some("c")]
        );
        assert!(ordered_rows("K1:1V1:aK1:1V1:b", by_number).is_err());
        // A NULL value tying with another is left out of the join anyway.
        assert_eq!(
            ordered_rows("K1:1V1:aK1:1N", by_number).unwrap(),
            [Some("a"), None]
        );
        let by_word = Order::named(1).unwrap().unwrap();
        assert!(ordered_rows("K1:aV1:aK1:AV1:A", by_word).is_err());
        assert!(ordered_rows("K1:xV1:aK1:1V1:b", by_number).is_err());
    }
}
