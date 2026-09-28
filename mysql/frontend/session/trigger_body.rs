//! Holding a trigger's body to the types of the columns it reads and writes.
//!
//! A trigger is translated for the engine without its columns' types, as it
//! is each time the database is opened, so each value its body writes or
//! compares is taken only where that translation means what MySQL means with
//! the types the columns have: a word or a whole number, copied, joined with
//! `CONCAT` or moved by a whole number, and a reading of the clock into a
//! column holding a moment of its kind. The engine's own write checks then
//! hold what is written to the column it lands in, as they hold any write —
//! measured on MySQL 8.4.11, `CONCAT('new ', NEW.name)` longer than its
//! `VARCHAR` answers 1406 there, and here.

use super::*;
use turso_mysql_parser::{
    CheckedComparisonNow, MySqlTriggerEvent, MySqlTriggerTiming, MySqlTriggerValue,
    MySqlTriggerWriteKind,
};

impl MySqlConnection {
    /// Refuses a trigger whose body reads or writes a column in a way whose
    /// meaning turns on a type this does not follow.
    pub(super) fn check_trigger_body(&self, written: &str) -> Result<()> {
        let body = turso_mysql_parser::trigger_body_readings(written, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let row = self.base_table_columns(body.table())?;
        let before = body.timing() == MySqlTriggerTiming::Before;
        // The frontend writes the moment such a column takes into an `UPDATE`
        // itself, and whether MySQL does that before or after a `BEFORE
        // UPDATE` trigger has not been measured.
        if before && body.event() == MySqlTriggerEvent::Update && carries_an_on_update_column(&row)
        {
            return Err(trigger_body_error(
                "a BEFORE UPDATE trigger on a table with an ON UPDATE column",
            ));
        }
        let set_new = body
            .writes()
            .iter()
            .filter(|write| write.kind() == MySqlTriggerWriteKind::SetNew)
            .flat_map(|write| write.assigned().iter().map(|(column, _)| column.as_str()))
            .collect::<Vec<_>>();
        for write in body.writes() {
            let target = self.base_table_columns(write.table())?;
            // An `UPDATE` rewrites such a column to the moment it runs at,
            // which the frontend writes into its own statements and a
            // trigger's would leave out.
            if write.kind() == MySqlTriggerWriteKind::Update && carries_an_on_update_column(&target)
            {
                return Err(trigger_body_error(
                    "a trigger updating a table with an ON UPDATE column",
                ));
            }
            let reading = TriggerColumns {
                row: &row,
                target: &target,
                event: body.event(),
                before,
                set_new: &set_new,
            };
            for (column, value) in write.assigned() {
                let written = named_column(&target, column)?;
                // A counted column's numbers are handed out by the frontend,
                // which a trigger's own write goes around, and the key is what
                // the row is found by.
                if written.extra().eq_ignore_ascii_case("AUTO_INCREMENT")
                    || (write.kind() == MySqlTriggerWriteKind::SetNew
                        && written.key() == MySqlColumnKey::Primary)
                {
                    return Err(trigger_body_error(
                        "a trigger writing a counted or key column",
                    ));
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

    /// The columns a table's `BEFORE INSERT` trigger sets in `NEW`, which an
    /// `INSERT` may leave out whatever their defaults: measured on MySQL
    /// 8.4.11, a `NOT NULL` column with no default of its own is taken from
    /// the trigger then, where it answers 1364 without one.
    pub(super) fn columns_set_before_insert(&self, table: &str) -> Vec<String> {
        self.columns_set_before(table, &turso_parser::ast::TriggerEvent::Insert)
    }

    /// Reports whether a statement writes a value other than NULL into a
    /// column its table's `BEFORE` trigger sets, or writes such a table
    /// through `ON DUPLICATE KEY UPDATE`, which this refuses.
    ///
    /// Measured on MySQL 8.4.11, the statement's value is held to its column
    /// before the trigger runs — one too long is 1406 even where the trigger
    /// writes another — and here a value is held to its column only as the
    /// row is written, after the trigger. NULL is what a trigger may replace
    /// in a `NOT NULL` column, and is taken.
    pub(super) fn writes_a_value_a_trigger_replaces(&self, statement: &Stmt) -> bool {
        use turso_parser::ast::{Expr, InsertBody, Literal, OneSelect, TriggerEvent};

        let is_null = |value: &Expr| matches!(value, Expr::Literal(Literal::Null));
        match statement {
            Stmt::Insert {
                tbl_name,
                columns,
                body,
                ..
            } => {
                let table = tbl_name.name.as_str();
                let set = self.columns_set_before(table, &TriggerEvent::Insert);
                let set_here = |at: usize| {
                    set.iter()
                        .any(|column| column.eq_ignore_ascii_case(columns[at].as_str()))
                };
                match body {
                    InsertBody::DefaultValues => false,
                    InsertBody::Select(_, Some(_)) => {
                        !set.is_empty()
                            || !self
                                .columns_set_before(table, &TriggerEvent::Update)
                                .is_empty()
                    }
                    InsertBody::Select(select, None) => match &select.body.select {
                        OneSelect::Values(rows) => rows.iter().any(|row| {
                            row.iter()
                                .enumerate()
                                .any(|(at, value)| set_here(at) && !is_null(value))
                        }),
                        _ => (0..columns.len()).any(set_here),
                    },
                }
            }
            Stmt::Update(update) => {
                let set =
                    self.columns_set_before(update.tbl_name.name.as_str(), &TriggerEvent::Update);
                update.sets.iter().any(|assignment| {
                    !is_null(&assignment.expr)
                        && assignment.col_names.iter().any(|column| {
                            set.iter()
                                .any(|set| set.eq_ignore_ascii_case(column.as_str()))
                        })
                })
            }
            _ => false,
        }
    }

    fn columns_set_before(
        &self,
        table: &str,
        event: &turso_parser::ast::TriggerEvent,
    ) -> Vec<String> {
        self.inner
            .current_schema()
            .get_triggers_for_table(table)
            .filter(|trigger| {
                trigger.time == turso_parser::ast::TriggerTime::Before && trigger.event == *event
            })
            .flat_map(|trigger| trigger.commands.iter())
            .filter_map(|command| match command {
                turso_parser::ast::TriggerCmd::SetNew { sets } => Some(sets),
                _ => None,
            })
            .flatten()
            .flat_map(|set| set.col_names.iter())
            .map(|column| column.as_str().to_owned())
            .collect()
    }

    /// Reports whether any trigger of this database reads the clock. A
    /// trigger runs under the time zone of the session whose statement fires
    /// it, as MySQL's does, and the engine's clock reads UTC.
    pub(super) fn a_trigger_reads_the_clock(&self) -> bool {
        self.inner
            .current_schema()
            .triggers
            .values()
            .flatten()
            .any(|trigger| uses_session_local_clock(&trigger.sql))
    }
}

fn carries_an_on_update_column(columns: &[MySqlColumnMetadata]) -> bool {
    columns
        .iter()
        .any(|column| column.extra().to_ascii_lowercase().contains("on update"))
}

fn trigger_body_error(what: &str) -> LimboError {
    LimboError::ParseError(format!("unsupported MySQL trigger body: {what}"))
}

/// The kinds of value a trigger's body is taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerValueKind {
    Word,
    WholeNumber,
    /// A `DATETIME` or a `TIMESTAMP` with no fraction of a second.
    Moment,
    Day,
    /// A `TIME` with no fraction of a second.
    TimeOfDay,
}

/// Reads a column's kind: a word for the text types, a whole number for the
/// integer types but `BIGINT UNSIGNED`, which the engine keeps in a form of
/// its own, and `BOOLEAN`, and a moment, a day or a time of day for the
/// temporal types a reading of the clock fills whole.
fn value_kind(column: &MySqlColumnMetadata) -> Option<TriggerValueKind> {
    let type_name = column.type_name();
    let whole_seconds = column.temporal_precision().unwrap_or(0) == 0;
    match type_name {
        _ if is_text_type(type_name) => Some(TriggerValueKind::Word),
        "BIGINT UNSIGNED" | "BOOLEAN" => None,
        _ if is_integer_type(type_name) => Some(TriggerValueKind::WholeNumber),
        "DATETIME" | "TIMESTAMP" if whole_seconds => Some(TriggerValueKind::Moment),
        "DATE" => Some(TriggerValueKind::Day),
        "TIME" if whole_seconds => Some(TriggerValueKind::TimeOfDay),
        _ => None,
    }
}

/// The columns one statement of a trigger's body may read.
struct TriggerColumns<'a> {
    /// The columns of the table the trigger runs for, which `NEW` and `OLD`
    /// read.
    row: &'a [MySqlColumnMetadata],
    /// The columns of the table the statement writes.
    target: &'a [MySqlColumnMetadata],
    event: MySqlTriggerEvent,
    before: bool,
    /// The columns the trigger sets in `NEW`.
    set_new: &'a [&'a str],
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
            // A moment copied from another column would be written again under
            // the session's time zone where one is a `TIMESTAMP`, which is not
            // followed here.
            MySqlTriggerValue::Row { .. } | MySqlTriggerValue::Column(_) => {
                let read = self.read_kind(value)?;
                matches!(read, TriggerValueKind::Word | TriggerValueKind::WholeNumber)
                    && read == kind
            }
            // `CONCAT` writes a number out as its digits, which is what the
            // engine's `||` writes too.
            MySqlTriggerValue::Joined(parts) => {
                for part in parts {
                    if matches!(
                        part,
                        MySqlTriggerValue::Row { .. } | MySqlTriggerValue::Column(_)
                    ) && !matches!(
                        self.read_kind(part)?,
                        TriggerValueKind::Word | TriggerValueKind::WholeNumber
                    ) {
                        return Err(trigger_body_error(
                            "a trigger joining a column that is neither a word nor a whole number",
                        ));
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
            MySqlTriggerValue::Clock(clock) => {
                kind == match clock {
                    CheckedComparisonNow::Moment => TriggerValueKind::Moment,
                    CheckedComparisonNow::Day => TriggerValueKind::Day,
                    CheckedComparisonNow::TimeOfDay => TriggerValueKind::TimeOfDay,
                }
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
                if *new && self.before {
                    self.read_before_the_row_is_written(column)?;
                }
                kind_of(self.row, column)
            }
            MySqlTriggerValue::Column(column) => kind_of(self.target, column),
            _ => unreachable!("only a column is read"),
        }
    }

    /// Refuses a `BEFORE INSERT` trigger reading a `NEW` column whose value
    /// is not yet the one MySQL reads there: a column the trigger sets
    /// itself, which before it does holds the implicit default MySQL gives a
    /// column the statement left out, and a counted column, which MySQL reads
    /// as 0 where the frontend has already handed out its number.
    fn read_before_the_row_is_written(&self, column: &str) -> Result<()> {
        if self.event != MySqlTriggerEvent::Insert {
            return Ok(());
        }
        let set_here = self
            .set_new
            .iter()
            .any(|set| set.eq_ignore_ascii_case(column));
        let counted = named_column(self.row, column)?
            .extra()
            .eq_ignore_ascii_case("AUTO_INCREMENT");
        if set_here || counted {
            return Err(trigger_body_error(
                "a BEFORE trigger reading a NEW column it sets, or a counted one",
            ));
        }
        Ok(())
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
        trigger_body_error("a trigger reading or writing a column of a type it does not follow")
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
