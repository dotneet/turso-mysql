//! A trigger written the way MySQL keeps it.
//!
//! MySQL keeps a trigger's body as the statement wrote it: measured on 8.4.11,
//! `SHOW CREATE TRIGGER` prints `` CREATE DEFINER=... TRIGGER `t` AFTER INSERT
//! ON `posts` FOR EACH ROW `` and then the body exactly as it was written,
//! case, spacing and line breaks included, without the whitespace around it or
//! the `;` ending the statement. `SHOW TRIGGERS` and
//! `information_schema.TRIGGERS` print that same body. So what is kept here is
//! that header and the body as written, and the engine's trigger is
//! translated from it each time the database is opened.
//!
//! A body is one statement or a `BEGIN ... END` list of them. Each is an
//! `INSERT ... VALUES` of one row, an `UPDATE` or a `DELETE` of another table,
//! and each value it writes or compares is one this translates the same way
//! whatever the columns' types are — the types are held to what the value
//! means in MySQL by the frontend, through [`MySqlTriggerBody`], when the
//! trigger is made.

use super::*;
use sqlparser::ast::{ConditionalStatements, FromTable, TableWithJoins};

/// Writes a `CREATE TRIGGER` the way MySQL keeps it, or answers nothing for
/// any other statement.
///
/// The header is written the way `SHOW CREATE TRIGGER` prints it, and the
/// body is kept as it was written.
pub fn trigger_written_as_mysql_keeps_it(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let dump_ddl = parse_optional_mysqldump_ddl(sql)?;
    let sql = dump_ddl.as_ref().map_or(sql, MySqlDumpDdl::normalized_sql);
    let Some(trigger) = parse_trigger(sql, mode)? else {
        return Ok(None);
    };
    let header = TriggerHeader::read(&trigger)?;
    let body = written_body(sql, mode)?;
    // The body has to be one this takes before the trigger is kept at all.
    trigger_body(&trigger, &header)?;
    Ok(Some(header.written(&body)))
}

/// Translates a trigger kept the way MySQL keeps it into the statement the
/// engine makes it with.
pub(crate) fn translate_create_trigger(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Some(trigger) = parse_trigger(sql, mode)? else {
        return Err(ParseError::ExpectedCreateTrigger);
    };
    let header = TriggerHeader::read(&trigger)?;
    let body = trigger_body(&trigger, &header)?;
    let commands = body
        .writes
        .iter()
        .map(|write| format!("{};", write.engine_sql))
        .collect::<Vec<_>>();
    Ok(format!(
        "CREATE TRIGGER {} {} {} ON {} FOR EACH ROW BEGIN {} END",
        render_ident_str(header.name.as_str()),
        header.timing.written(),
        header.event.written(),
        render_ident_str(header.table.as_str()),
        commands.join(" ")
    ))
}

/// The MySQL DDL a trigger is kept under: the text it was made from, once it
/// is proved to translate to the engine's statement.
pub fn mysql_create_trigger_ddl(
    statement: &Stmt,
    written: &str,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    if parse_create_trigger_ast(written, mode)? != *statement {
        return unsupported("a trigger written other than as it was made");
    }
    Ok(written.to_owned())
}

/// What `SHOW TRIGGERS` and `SHOW CREATE TRIGGER` read out of a trigger kept
/// the way MySQL keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlWrittenTrigger {
    name: MySqlTableName,
    table: MySqlTableName,
    timing: MySqlTriggerTiming,
    event: MySqlTriggerEvent,
    body: String,
}

impl MySqlWrittenTrigger {
    pub fn name(&self) -> &MySqlTableName {
        &self.name
    }

    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    pub const fn timing(&self) -> MySqlTriggerTiming {
        self.timing
    }

    pub const fn event(&self) -> MySqlTriggerEvent {
        self.event
    }

    /// The body as it was written.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// What `SHOW CREATE TRIGGER` prints, naming the account that made it.
    pub fn show_create(&self, username: &str) -> String {
        format!(
            "CREATE DEFINER=`{}`@`%` TRIGGER {} {} {} ON {} FOR EACH ROW {}",
            username.replace('`', "``"),
            quoted_name(self.name.as_str()),
            self.timing.written(),
            self.event.written(),
            quoted_name(self.table.as_str()),
            self.body
        )
    }
}

/// When a trigger runs, beside the row it is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MySqlTriggerTiming {
    Before,
    After,
}

impl MySqlTriggerTiming {
    pub const fn written(self) -> &'static str {
        match self {
            Self::Before => "BEFORE",
            Self::After => "AFTER",
        }
    }
}

/// Which statement a trigger runs for. MySQL lists a table's triggers in this
/// order: measured on 8.4.11, `SHOW TRIGGERS` puts a table's `INSERT` triggers
/// before its `UPDATE` ones and those before its `DELETE` ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MySqlTriggerEvent {
    Insert,
    Update,
    Delete,
}

impl MySqlTriggerEvent {
    pub const fn written(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

/// Reads a trigger kept the way MySQL keeps it.
pub fn written_trigger(sql: &str, mode: SessionSqlMode) -> Result<MySqlWrittenTrigger, ParseError> {
    let Some(trigger) = parse_trigger(sql, mode)? else {
        return Err(ParseError::ExpectedCreateTrigger);
    };
    let header = TriggerHeader::read(&trigger)?;
    Ok(MySqlWrittenTrigger {
        name: header.name,
        table: header.table,
        timing: header.timing,
        event: header.event,
        body: written_body(sql, mode)?,
    })
}

/// What a trigger's body writes, and what each value it writes or compares
/// reads, so the frontend can hold each to the types of the columns it meets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlTriggerBody {
    table: MySqlTableName,
    event: MySqlTriggerEvent,
    writes: Vec<MySqlTriggerWrite>,
}

impl MySqlTriggerBody {
    /// The table the trigger runs for.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    pub const fn event(&self) -> MySqlTriggerEvent {
        self.event
    }

    /// Each statement of the body, in order.
    pub fn writes(&self) -> &[MySqlTriggerWrite] {
        &self.writes
    }
}

/// One statement of a trigger's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlTriggerWrite {
    table: MySqlTableName,
    kind: MySqlTriggerWriteKind,
    /// Each column the statement writes and the value it writes there.
    assigned: Vec<(String, MySqlTriggerValue)>,
    /// Each column the statement's `WHERE` compares, and what with.
    compared: Vec<(String, MySqlTriggerValue)>,
    engine_sql: String,
}

impl MySqlTriggerWrite {
    /// The table the statement writes.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    pub const fn kind(&self) -> MySqlTriggerWriteKind {
        self.kind
    }

    pub fn assigned(&self) -> &[(String, MySqlTriggerValue)] {
        &self.assigned
    }

    pub fn compared(&self) -> &[(String, MySqlTriggerValue)] {
        &self.compared
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlTriggerWriteKind {
    Insert,
    Update,
    Delete,
}

/// One value a trigger's body writes or compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlTriggerValue {
    /// `NULL`.
    Null,
    /// A written word.
    Word(String),
    /// A written whole number.
    WholeNumber(i64),
    /// A column of the row the trigger runs for, as it is after the statement
    /// (`NEW`) or was before it (`OLD`).
    Row { new: bool, column: String },
    /// A column of the row the statement itself changes, read as it stood.
    Column(String),
    /// `CONCAT` of its parts, each a word, a whole number or a column.
    Joined(Vec<MySqlTriggerValue>),
    /// A column with a whole number added or taken away.
    Shifted {
        column: Box<MySqlTriggerValue>,
        by: i64,
    },
}

/// Reads what a trigger kept the way MySQL keeps it writes and reads.
pub fn trigger_body_readings(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlTriggerBody, ParseError> {
    let Some(trigger) = parse_trigger(sql, mode)? else {
        return Err(ParseError::ExpectedCreateTrigger);
    };
    let header = TriggerHeader::read(&trigger)?;
    trigger_body(&trigger, &header)
}

/// Reads a `CREATE TRIGGER`, or answers nothing for any other statement.
///
/// A body of one statement ends where the text does, and sqlparser wants a
/// `;` after every statement of a body, so it is read again with one when it
/// was written without.
pub(crate) fn parse_trigger(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<CreateTrigger>, ParseError> {
    let statement = match parse_one_statement(sql, mode) {
        Ok(statement) => statement,
        Err(error) => match parse_one_statement(&format!("{sql};"), mode) {
            Ok(statement @ Statement::CreateTrigger(_)) => statement,
            _ => return Err(error),
        },
    };
    match statement {
        Statement::CreateTrigger(trigger) => Ok(Some(trigger)),
        _ => Ok(None),
    }
}

/// The part of a `CREATE TRIGGER` before its body.
struct TriggerHeader {
    name: MySqlTableName,
    table: MySqlTableName,
    timing: MySqlTriggerTiming,
    event: MySqlTriggerEvent,
}

impl TriggerHeader {
    fn read(trigger: &CreateTrigger) -> Result<Self, ParseError> {
        if trigger.or_alter
            || trigger.temporary
            || trigger.or_replace
            || trigger.is_constraint
            || !trigger.period_before_table
            || trigger.referenced_table_name.is_some()
            || !trigger.referencing.is_empty()
            || trigger.condition.is_some()
            || trigger.exec_body.is_some()
            || trigger.statements_as
            || trigger.characteristics.is_some()
            || !matches!(
                trigger.trigger_object,
                Some(TriggerObjectKind::ForEach(TriggerObject::Row))
            )
        {
            return unsupported("CREATE TRIGGER option");
        }
        // A `BEFORE` trigger can change the row before it is written, which the
        // engine has no statement for.
        let timing = match trigger.period {
            Some(TriggerPeriod::After) => MySqlTriggerTiming::After,
            _ => return unsupported("CREATE TRIGGER timing"),
        };
        let event = match trigger.events.as_slice() {
            [SqlTriggerEvent::Insert] => MySqlTriggerEvent::Insert,
            [SqlTriggerEvent::Update(columns)] if columns.is_empty() => MySqlTriggerEvent::Update,
            [SqlTriggerEvent::Delete] => MySqlTriggerEvent::Delete,
            _ => return unsupported("CREATE TRIGGER event"),
        };
        Ok(Self {
            name: one_name(&trigger.name)?,
            table: one_name(&trigger.table_name)?,
            timing,
            event,
        })
    }

    fn written(&self, body: &str) -> String {
        format!(
            "CREATE TRIGGER {} {} {} ON {} FOR EACH ROW {body}",
            quoted_name(self.name.as_str()),
            self.timing.written(),
            self.event.written(),
            quoted_name(self.table.as_str()),
        )
    }
}

fn one_name(name: &ObjectName) -> Result<MySqlTableName, ParseError> {
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return unsupported("CREATE TRIGGER naming a database");
    };
    MySqlTableName::parse(&name.value)
}

/// Reads the body out of the text: measured on MySQL 8.4.11, everything after
/// `FOR EACH ROW`, without the whitespace around it or the `;` that ends the
/// statement — `VALUES (OLD.id, 2) ;` keeps `VALUES (OLD.id, 2)`.
fn written_body(sql: &str, mode: SessionSqlMode) -> Result<String, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize_with_location()
        .map_err(|error| ParseError::Sqlparser(ParserError::from(error).to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let body_start = words
        .windows(4)
        .find(|window| {
            is_unquoted_word(&window[0].token, "FOR")
                && is_unquoted_word(&window[1].token, "EACH")
                && is_unquoted_word(&window[2].token, "ROW")
        })
        .map(|window| window[3].span.start)
        .ok_or(ParseError::Unsupported {
            feature: "CREATE TRIGGER body",
        })?;
    let offset = byte_offset(sql, body_start).ok_or(ParseError::Unsupported {
        feature: "CREATE TRIGGER body",
    })?;
    let body = sql[offset..]
        .trim_end()
        .trim_end_matches(';')
        .trim_end()
        .to_owned();
    if body.is_empty() {
        return unsupported("CREATE TRIGGER body");
    }
    Ok(body)
}

fn byte_offset(source: &str, location: sqlparser::tokenizer::Location) -> Option<usize> {
    let mut line = 1;
    let mut column = 1;
    for (offset, character) in source.char_indices() {
        if line == location.line && column == location.column {
            return Some(offset);
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line == location.line && column == location.column).then_some(source.len())
}

/// Reads each statement of a trigger's body and the engine statement it is.
fn trigger_body(
    trigger: &CreateTrigger,
    header: &TriggerHeader,
) -> Result<MySqlTriggerBody, ParseError> {
    let statements = match &trigger.statements {
        Some(ConditionalStatements::BeginEnd(body)) => body.statements.as_slice(),
        Some(ConditionalStatements::Sequence { statements }) if statements.len() == 1 => {
            statements.as_slice()
        }
        _ => return unsupported("CREATE TRIGGER body"),
    };
    if statements.is_empty() {
        return unsupported("CREATE TRIGGER body without a statement");
    }
    let rows = TriggerRows {
        new: header.event != MySqlTriggerEvent::Delete,
        old: header.event != MySqlTriggerEvent::Insert,
    };
    let writes = statements
        .iter()
        .map(|statement| {
            let write = match statement {
                Statement::Insert(insert) => trigger_insert(insert, rows)?,
                Statement::Update(update) => trigger_update(update, rows)?,
                Statement::Delete(delete) => trigger_delete(delete, rows)?,
                _ => return unsupported("CREATE TRIGGER body statement"),
            };
            // MySQL answers 1442 when a trigger writes the table whose
            // statement fired it.
            if write.table == header.table {
                return unsupported("CREATE TRIGGER writing its own table");
            }
            Ok(write)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MySqlTriggerBody {
        table: header.table.clone(),
        event: header.event,
        writes,
    })
}

/// Which of `NEW` and `OLD` a trigger's event has: measured on MySQL 8.4.11,
/// naming the other is 1363 when the trigger is made.
#[derive(Debug, Clone, Copy)]
struct TriggerRows {
    new: bool,
    old: bool,
}

fn trigger_insert(insert: &Insert, rows: TriggerRows) -> Result<MySqlTriggerWrite, ParseError> {
    let TableObject::TableName(target) = &insert.table else {
        return unsupported("CREATE TRIGGER INSERT target");
    };
    if !insert.optimizer_hints.is_empty()
        || insert.or.is_some()
        || insert.ignore
        || insert.replace_into
        || !insert.into
        || insert.table_alias.is_some()
        || insert.overwrite
        || !insert.assignments.is_empty()
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.has_table_keyword
        || insert.on.is_some()
        || insert.returning.is_some()
        || insert.output.is_some()
        || insert.priority.is_some()
        || insert.insert_alias.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
        || insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return unsupported("CREATE TRIGGER INSERT option");
    }
    let table = one_name(target)?;
    let columns = insert
        .columns
        .iter()
        .map(|column| match column.0.as_slice() {
            [ObjectNamePart::Identifier(column)] => Ok(column.value.clone()),
            _ => unsupported("CREATE TRIGGER INSERT column"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE TRIGGER INSERT without columns");
    }
    let Some(source) = &insert.source else {
        return unsupported("CREATE TRIGGER INSERT source");
    };
    if source.with.is_some()
        || source.order_by.is_some()
        || source.limit_clause.is_some()
        || source.fetch.is_some()
        || !source.locks.is_empty()
        || source.for_clause.is_some()
        || source.settings.is_some()
        || source.format_clause.is_some()
        || !source.pipe_operators.is_empty()
    {
        return unsupported("CREATE TRIGGER INSERT source");
    }
    let SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("CREATE TRIGGER INSERT SELECT");
    };
    if values.explicit_row || values.value_keyword {
        return unsupported("CREATE TRIGGER INSERT VALUES option");
    }
    let [values] = values.rows.as_slice() else {
        return unsupported("CREATE TRIGGER INSERT VALUES rows");
    };
    if values.len() != columns.len() {
        return unsupported("CREATE TRIGGER INSERT value count");
    }
    let reading = TriggerReading {
        rows,
        own_columns: false,
    };
    let assigned = columns
        .into_iter()
        .zip(values.iter())
        .map(|(column, value)| Ok((column, reading.value(value)?)))
        .collect::<Result<Vec<_>, ParseError>>()?;
    let engine_sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        render_ident_str(table.as_str()),
        assigned
            .iter()
            .map(|(column, _)| render_ident_str(column))
            .collect::<Vec<_>>()
            .join(", "),
        assigned
            .iter()
            .map(|(_, value)| value.engine_sql())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(MySqlTriggerWrite {
        table,
        kind: MySqlTriggerWriteKind::Insert,
        assigned,
        compared: Vec::new(),
        engine_sql,
    })
}

fn trigger_update(update: &Update, rows: TriggerRows) -> Result<MySqlTriggerWrite, ParseError> {
    if !update.optimizer_hints.is_empty()
        || update.from.is_some()
        || update.returning.is_some()
        || update.output.is_some()
        || update.or.is_some()
        || !update.order_by.is_empty()
        || update.limit.is_some()
    {
        return unsupported("CREATE TRIGGER UPDATE option");
    }
    let table = one_table(&update.table)?;
    if update.assignments.is_empty() {
        return unsupported("CREATE TRIGGER UPDATE without assignments");
    }
    let reading = TriggerReading {
        rows,
        own_columns: true,
    };
    let mut assigned: Vec<(String, MySqlTriggerValue)> = Vec::new();
    for assignment in &update.assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
            return unsupported("CREATE TRIGGER UPDATE assignment");
        };
        let [ObjectNamePart::Identifier(column)] = column.0.as_slice() else {
            return unsupported("CREATE TRIGGER UPDATE assignment");
        };
        let value = reading.value(&assignment.value)?;
        // MySQL reads a column an earlier assignment wrote as written, and
        // the engine as it stood.
        if assigned
            .iter()
            .any(|(earlier, _)| value.reads_own_column(earlier))
            || assigned
                .iter()
                .any(|(earlier, _)| earlier.eq_ignore_ascii_case(&column.value))
        {
            return unsupported("CREATE TRIGGER UPDATE reading a column it has already assigned");
        }
        assigned.push((column.value.clone(), value));
    }
    let compared = match &update.selection {
        Some(condition) => reading.condition(condition)?,
        None => Vec::new(),
    };
    let mut engine_sql = format!(
        "UPDATE {} SET {}",
        render_ident_str(table.as_str()),
        assigned
            .iter()
            .map(|(column, value)| format!("{} = {}", render_ident_str(column), value.engine_sql()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    push_engine_condition(&mut engine_sql, &compared);
    Ok(MySqlTriggerWrite {
        table,
        kind: MySqlTriggerWriteKind::Update,
        assigned,
        compared,
        engine_sql,
    })
}

fn trigger_delete(delete: &Delete, rows: TriggerRows) -> Result<MySqlTriggerWrite, ParseError> {
    if !delete.optimizer_hints.is_empty()
        || !delete.tables.is_empty()
        || delete.using.is_some()
        || delete.returning.is_some()
        || delete.output.is_some()
        || !delete.order_by.is_empty()
        || delete.limit.is_some()
    {
        return unsupported("CREATE TRIGGER DELETE option");
    }
    let FromTable::WithFromKeyword(from) = &delete.from else {
        return unsupported("CREATE TRIGGER DELETE form");
    };
    let [from] = from.as_slice() else {
        return unsupported("CREATE TRIGGER DELETE form");
    };
    let table = one_table(from)?;
    let reading = TriggerReading {
        rows,
        own_columns: true,
    };
    // A `DELETE` with no condition empties the table, which a trigger has no
    // measured use for.
    let Some(condition) = &delete.selection else {
        return unsupported("CREATE TRIGGER DELETE without a condition");
    };
    let compared = reading.condition(condition)?;
    let mut engine_sql = format!("DELETE FROM {}", render_ident_str(table.as_str()));
    push_engine_condition(&mut engine_sql, &compared);
    Ok(MySqlTriggerWrite {
        table,
        kind: MySqlTriggerWriteKind::Delete,
        assigned: Vec::new(),
        compared,
        engine_sql,
    })
}

fn one_table(from: &TableWithJoins) -> Result<MySqlTableName, ParseError> {
    let TableFactor::Table {
        name,
        alias: None,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_hints,
    } = &from.relation
    else {
        return unsupported("CREATE TRIGGER statement table");
    };
    if !from.joins.is_empty()
        || !with_hints.is_empty()
        || !partitions.is_empty()
        || !index_hints.is_empty()
    {
        return unsupported("CREATE TRIGGER statement table");
    }
    one_name(name)
}

fn push_engine_condition(engine_sql: &mut String, compared: &[(String, MySqlTriggerValue)]) {
    if compared.is_empty() {
        return;
    }
    engine_sql.push_str(" WHERE ");
    engine_sql.push_str(
        &compared
            .iter()
            .map(|(column, value)| {
                format!("({} = {})", render_ident_str(column), value.engine_sql())
            })
            .collect::<Vec<_>>()
            .join(" AND "),
    );
}

/// What a value in one statement of a trigger's body may read.
struct TriggerReading {
    rows: TriggerRows,
    /// Whether a bare column names a column of the table the statement
    /// changes, which an `UPDATE` and a `DELETE` read and an `INSERT` does
    /// not.
    own_columns: bool,
}

impl TriggerReading {
    fn value(&self, expr: &Expr) -> Result<MySqlTriggerValue, ParseError> {
        match expr {
            Expr::Nested(inner) => self.value(inner),
            Expr::Value(value) => match &value.value {
                Value::Null => Ok(MySqlTriggerValue::Null),
                Value::SingleQuotedString(word) | Value::DoubleQuotedString(word) => {
                    Ok(MySqlTriggerValue::Word(word.clone()))
                }
                // A number with a point or an exponent is written back by
                // rules of its own, and only a whole one is taken.
                Value::Number(number, false) => number
                    .parse::<i64>()
                    .map(MySqlTriggerValue::WholeNumber)
                    .map_err(|_| ParseError::Unsupported {
                        feature: "CREATE TRIGGER number",
                    }),
                _ => unsupported("CREATE TRIGGER value"),
            },
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr: inner,
            } => match self.value(inner)? {
                MySqlTriggerValue::WholeNumber(number) => number
                    .checked_neg()
                    .map(MySqlTriggerValue::WholeNumber)
                    .ok_or(ParseError::Unsupported {
                        feature: "CREATE TRIGGER number",
                    }),
                _ => unsupported("CREATE TRIGGER value"),
            },
            Expr::Identifier(column) if self.own_columns => {
                Ok(MySqlTriggerValue::Column(column.value.clone()))
            }
            Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [row, column] if row.value.eq_ignore_ascii_case("NEW") && self.rows.new => {
                    Ok(MySqlTriggerValue::Row {
                        new: true,
                        column: column.value.clone(),
                    })
                }
                [row, column] if row.value.eq_ignore_ascii_case("OLD") && self.rows.old => {
                    Ok(MySqlTriggerValue::Row {
                        new: false,
                        column: column.value.clone(),
                    })
                }
                _ => unsupported("CREATE TRIGGER column"),
            },
            Expr::BinaryOp {
                left,
                op: op @ (BinaryOperator::Plus | BinaryOperator::Minus),
                right,
            } => {
                let column = self.value(left)?;
                let MySqlTriggerValue::WholeNumber(by) = self.value(right)? else {
                    return unsupported("CREATE TRIGGER arithmetic");
                };
                if !matches!(
                    column,
                    MySqlTriggerValue::Row { .. } | MySqlTriggerValue::Column(_)
                ) {
                    return unsupported("CREATE TRIGGER arithmetic");
                }
                let by = if matches!(op, BinaryOperator::Minus) {
                    by.checked_neg().ok_or(ParseError::Unsupported {
                        feature: "CREATE TRIGGER number",
                    })?
                } else {
                    by
                };
                Ok(MySqlTriggerValue::Shifted {
                    column: Box::new(column),
                    by,
                })
            }
            Expr::Function(function) => self.joined(function),
            _ => unsupported("CREATE TRIGGER value"),
        }
    }

    /// Reads `CONCAT(a, b, ...)` over words, whole numbers and columns.
    fn joined(&self, function: &sqlparser::ast::Function) -> Result<MySqlTriggerValue, ParseError> {
        let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
            return unsupported("CREATE TRIGGER call");
        };
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            return unsupported("CREATE TRIGGER call");
        };
        if !name.value.eq_ignore_ascii_case("CONCAT")
            || name.quote_style.is_some()
            || function.over.is_some()
            || function.filter.is_some()
            || function.null_treatment.is_some()
            || !function.within_group.is_empty()
            || !matches!(function.parameters, sqlparser::ast::FunctionArguments::None)
            || !arguments.clauses.is_empty()
            || arguments.duplicate_treatment.is_some()
            || arguments.args.is_empty()
        {
            return unsupported("CREATE TRIGGER call");
        }
        arguments
            .args
            .iter()
            .map(|argument| {
                let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    argument,
                )) = argument
                else {
                    return unsupported("CREATE TRIGGER call argument");
                };
                match self.value(argument)? {
                    part @ (MySqlTriggerValue::Word(_)
                    | MySqlTriggerValue::WholeNumber(_)
                    | MySqlTriggerValue::Row { .. }
                    | MySqlTriggerValue::Column(_)) => Ok(part),
                    _ => unsupported("CREATE TRIGGER call argument"),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(MySqlTriggerValue::Joined)
    }

    /// Reads a condition made of `column = value` comparisons joined by `AND`.
    fn condition(&self, expr: &Expr) -> Result<Vec<(String, MySqlTriggerValue)>, ParseError> {
        match expr {
            Expr::Nested(inner) => self.condition(inner),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                let mut compared = self.condition(left)?;
                compared.extend(self.condition(right)?);
                Ok(compared)
            }
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } => {
                let (column, value) = match (left.as_ref(), right.as_ref()) {
                    (Expr::Identifier(column), value) | (value, Expr::Identifier(column)) => {
                        (column, value)
                    }
                    _ => return unsupported("CREATE TRIGGER condition"),
                };
                match self.value(value)? {
                    value @ (MySqlTriggerValue::Row { .. } | MySqlTriggerValue::WholeNumber(_)) => {
                        Ok(vec![(column.value.clone(), value)])
                    }
                    _ => unsupported("CREATE TRIGGER condition"),
                }
            }
            _ => unsupported("CREATE TRIGGER condition"),
        }
    }
}

impl MySqlTriggerValue {
    /// The engine's spelling of the value, the same whatever the columns'
    /// types: MySQL's `CONCAT` answers NULL for a NULL part, which the
    /// engine's `||` does where its own `concat` does not.
    fn engine_sql(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::Word(word) => format!("'{}'", word.replace('\'', "''")),
            Self::WholeNumber(number) => number.to_string(),
            Self::Row { new, column } => {
                format!(
                    "{}.{}",
                    if *new { "NEW" } else { "OLD" },
                    render_ident_str(column)
                )
            }
            Self::Column(column) => render_ident_str(column),
            Self::Joined(parts) => format!(
                "({})",
                parts
                    .iter()
                    .map(Self::engine_sql)
                    .collect::<Vec<_>>()
                    .join(" || ")
            ),
            Self::Shifted { column, by } if *by < 0 => {
                format!("({} - {})", column.engine_sql(), by.unsigned_abs())
            }
            Self::Shifted { column, by } => format!("({} + {by})", column.engine_sql()),
        }
    }

    fn reads_own_column(&self, name: &str) -> bool {
        match self {
            Self::Column(column) => column.eq_ignore_ascii_case(name),
            Self::Joined(parts) => parts.iter().any(|part| part.reads_own_column(name)),
            Self::Shifted { column, .. } => column.reads_own_column(name),
            Self::Null | Self::Word(_) | Self::WholeNumber(_) | Self::Row { .. } => false,
        }
    }
}

fn quoted_name(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}
