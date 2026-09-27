//! Holding a trigger's body to the types of the columns it reads and writes.
//!
//! A trigger is translated for the engine without its columns' types, as it
//! is each time the database is opened, so each value its body writes or
//! compares is taken only where that translation means what MySQL means with
//! the types the columns have: a word or a whole number, copied, joined with
//! `CONCAT` or moved by a whole number. The engine's own write checks then
//! hold what is written to the column it lands in, as they hold any write —
//! measured on MySQL 8.4.11, `CONCAT('new ', NEW.name)` longer than its
//! `VARCHAR` answers 1406 there, and here.

use super::*;
use turso_mysql_parser::{MySqlTriggerEvent, MySqlTriggerValue, MySqlTriggerWriteKind};

impl MySqlConnection {
    /// Refuses a trigger whose body reads or writes a column in a way whose
    /// meaning turns on a type this does not follow.
    pub(super) fn check_trigger_body(&self, written: &str) -> Result<()> {
        let body = turso_mysql_parser::trigger_body_readings(written, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let row = self.base_table_columns(body.table())?;
        for write in body.writes() {
            let target = self.base_table_columns(write.table())?;
            // An `UPDATE` rewrites such a column to the moment it runs at,
            // which the frontend writes into its own statements and a
            // trigger's would leave out.
            if write.kind() == MySqlTriggerWriteKind::Update
                && target
                    .iter()
                    .any(|column| column.extra().to_ascii_lowercase().contains("on update"))
            {
                return Err(trigger_body_error(
                    "a trigger updating a table with an ON UPDATE column",
                ));
            }
            let reading = TriggerColumns {
                row: &row,
                target: &target,
                event: body.event(),
            };
            for (column, value) in write.assigned() {
                // A counted column's numbers are handed out by the frontend,
                // which a trigger's own write goes around.
                if named_column(&target, column)?
                    .extra()
                    .eq_ignore_ascii_case("AUTO_INCREMENT")
                {
                    return Err(trigger_body_error("a trigger writing a counted column"));
                }
                reading.check_written(reading.target_kind(column)?, value)?;
            }
            for (column, value) in write.compared() {
                if reading.target_kind(column)? != TriggerValueKind::WholeNumber {
                    return Err(trigger_body_error(
                        "a trigger comparing anything but whole numbers",
                    ));
                }
                reading.check_written(TriggerValueKind::WholeNumber, value)?;
            }
        }
        Ok(())
    }

    fn base_table_columns(&self, table: &MySqlTableName) -> Result<Vec<MySqlColumnMetadata>> {
        if turso_core::schema::is_system_table(table.as_str())
            || self
                .inner
                .current_schema()
                .get_btree_table(table.as_str())
                .is_none()
        {
            return Err(trigger_body_error(
                "a trigger naming a table that is not a base table",
            ));
        }
        self.list_columns(table)
            .map_err(|_| trigger_body_error("a trigger naming a table whose columns are unread"))
    }
}

fn trigger_body_error(what: &str) -> LimboError {
    LimboError::ParseError(format!("unsupported MySQL trigger body: {what}"))
}

/// The two kinds of value a trigger's body is taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerValueKind {
    Word,
    WholeNumber,
}

/// Reads a column's kind: a word for the text types, a whole number for the
/// integer types but `BIGINT UNSIGNED`, which the engine keeps in a form of
/// its own, and `BOOLEAN`.
fn value_kind(column: &MySqlColumnMetadata) -> Option<TriggerValueKind> {
    let type_name = column.type_name();
    if is_text_type(type_name) {
        return Some(TriggerValueKind::Word);
    }
    (is_integer_type(type_name) && !matches!(type_name, "BIGINT UNSIGNED" | "BOOLEAN"))
        .then_some(TriggerValueKind::WholeNumber)
}

/// The columns one statement of a trigger's body may read.
struct TriggerColumns<'a> {
    /// The columns of the table the trigger runs for, which `NEW` and `OLD`
    /// read.
    row: &'a [MySqlColumnMetadata],
    /// The columns of the table the statement writes.
    target: &'a [MySqlColumnMetadata],
    event: MySqlTriggerEvent,
}

impl TriggerColumns<'_> {
    fn target_kind(&self, column: &str) -> Result<TriggerValueKind> {
        kind_of(self.target, column)
    }

    /// Holds a value to the kind of the column it is written to or compared
    /// with.
    fn check_written(&self, kind: TriggerValueKind, value: &MySqlTriggerValue) -> Result<()> {
        let fits = match value {
            MySqlTriggerValue::Null => true,
            MySqlTriggerValue::Word(_) => kind == TriggerValueKind::Word,
            MySqlTriggerValue::WholeNumber(_) => kind == TriggerValueKind::WholeNumber,
            MySqlTriggerValue::Row { .. } | MySqlTriggerValue::Column(_) => {
                self.read_kind(value)? == kind
            }
            // `CONCAT` writes a number out as its digits, which is what the
            // engine's `||` writes too.
            MySqlTriggerValue::Joined(parts) => {
                for part in parts {
                    if matches!(
                        part,
                        MySqlTriggerValue::Row { .. } | MySqlTriggerValue::Column(_)
                    ) {
                        self.read_kind(part)?;
                    }
                }
                kind == TriggerValueKind::Word
            }
            // MySQL answers 1690 where a `BIGINT` passes its range and the
            // engine goes on in a real number, so only a narrower column is
            // moved.
            MySqlTriggerValue::Shifted { column, .. } => {
                kind == TriggerValueKind::WholeNumber
                    && self.read_kind(column)? == TriggerValueKind::WholeNumber
                    && !matches!(self.read_type(column)?, "BIGINT")
            }
        };
        if !fits {
            return Err(trigger_body_error(
                "a trigger writing a value of another kind than its column",
            ));
        }
        Ok(())
    }

    fn read_kind(&self, value: &MySqlTriggerValue) -> Result<TriggerValueKind> {
        match value {
            MySqlTriggerValue::Row { new, column } => {
                // Measured on MySQL 8.4.11: naming `NEW` in a `DELETE` trigger
                // or `OLD` in an `INSERT` one is 1363.
                let named = if *new {
                    self.event != MySqlTriggerEvent::Delete
                } else {
                    self.event != MySqlTriggerEvent::Insert
                };
                if !named {
                    return Err(trigger_body_error(
                        "a trigger naming a row its event has not",
                    ));
                }
                kind_of(self.row, column)
            }
            MySqlTriggerValue::Column(column) => kind_of(self.target, column),
            _ => unreachable!("only a column is read"),
        }
    }

    fn read_type(&self, value: &MySqlTriggerValue) -> Result<&str> {
        let (columns, column) = match value {
            MySqlTriggerValue::Row { column, .. } => (self.row, column),
            MySqlTriggerValue::Column(column) => (self.target, column),
            _ => unreachable!("only a column is read"),
        };
        Ok(named_column(columns, column)?.type_name())
    }
}

fn kind_of(columns: &[MySqlColumnMetadata], column: &str) -> Result<TriggerValueKind> {
    value_kind(named_column(columns, column)?).ok_or_else(|| {
        trigger_body_error(
            "a trigger reading or writing a column that is neither a word nor a whole number",
        )
    })
}

fn named_column<'a>(
    columns: &'a [MySqlColumnMetadata],
    column: &str,
) -> Result<&'a MySqlColumnMetadata> {
    columns
        .iter()
        .find(|declared| declared.name().eq_ignore_ascii_case(column))
        .ok_or_else(|| trigger_body_error("a trigger naming a column its table has not"))
}
