//! Which columns a statement writes a literal into whose meaning turns on the
//! column: a number with a fraction, which a column of bytes stores in
//! MySQL's spelling of it, a string of bytes, which only a column of bytes
//! takes as it stands, and a moment written as `TIMESTAMP('...')`, which only
//! a `DATETIME` or `TIMESTAMP` column stores as the word it names.
//!
//! Only the frontend knows each column's type, so this reads the statement's
//! literals out and leaves the frontend to hold them to their columns.

use crate::{read_one_statement, ParseError, SessionSqlMode};
use sqlparser::ast::{
    AssignmentTarget, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectName,
    ObjectNamePart, OnInsert, SetExpr, Statement, TableFactor, TableObject, UnaryOperator, Value,
};

/// A literal whose meaning turns on the column it is written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnLiteral {
    /// A number written with a point or an exponent — `1.5`, `1e3`.
    NumberWithAFraction,
    /// A string of bytes: `X'..'`, `0x..`, `b'..'`, or a word or hexadecimal
    /// literal after `_binary`.
    Bytes,
    /// A moment written as `TIMESTAMP('2024-01-02 03:04:05.000000')`, read by
    /// [`written_moment`].
    Moment,
}

/// What a statement writes these literals into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenLiterals {
    /// The tables the statement may write: the one an `INSERT` names, every
    /// table a joined `UPDATE` names.
    pub tables: Vec<String>,
    /// Each column one of these literals is written into, in the order
    /// written.
    pub columns: Vec<(WrittenColumn, ColumnLiteral)>,
}

/// A column a literal is written into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrittenColumn {
    /// Named by the statement.
    Named(String),
    /// The column at this place, counting from 0, in the table's own order —
    /// an `INSERT` that names no columns, as a dump writes its rows.
    AtPlace(usize),
}

/// Reads what a statement writes these literals into; `None` when it writes
/// none, or when the statement is no `INSERT` or `UPDATE` this reads.
pub fn literals_written_into_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<WrittenLiterals>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let (tables, columns) = match read_statement.as_ref().as_ref().map_err(Clone::clone)? {
        Statement::Insert(insert) => {
            let TableObject::TableName(table) = &insert.table else {
                return Ok(None);
            };
            let Some(table) = last_name(table) else {
                return Ok(None);
            };
            let mut written = Vec::new();
            for assignment in &insert.assignments {
                note_assignment(&assignment.target, &assignment.value, &mut written);
            }
            if let Some(source) = insert.source.as_deref() {
                if let SetExpr::Values(values) = source.body.as_ref() {
                    for row in &values.rows {
                        for (place, value) in row.iter().enumerate() {
                            let Some(literal) = column_literal(value) else {
                                continue;
                            };
                            let column = if insert.columns.is_empty() {
                                WrittenColumn::AtPlace(place)
                            } else {
                                match insert.columns.get(place).and_then(last_name) {
                                    Some(name) => WrittenColumn::Named(name),
                                    None => continue,
                                }
                            };
                            written.push((column, literal));
                        }
                    }
                }
            }
            if let Some(OnInsert::DuplicateKeyUpdate(assignments)) = &insert.on {
                for assignment in assignments {
                    note_assignment(&assignment.target, &assignment.value, &mut written);
                }
            }
            (vec![table], written)
        }
        Statement::Update(update) => {
            let tables = std::iter::once(&update.table.relation)
                .chain(update.table.joins.iter().map(|join| &join.relation))
                .filter_map(|relation| match relation {
                    TableFactor::Table { name, .. } => last_name(name),
                    _ => None,
                })
                .collect();
            let mut written = Vec::new();
            for assignment in &update.assignments {
                note_assignment(&assignment.target, &assignment.value, &mut written);
            }
            (tables, written)
        }
        _ => return Ok(None),
    };
    Ok((!columns.is_empty()).then_some(WrittenLiterals { tables, columns }))
}

fn note_assignment(
    target: &AssignmentTarget,
    value: &Expr,
    written: &mut Vec<(WrittenColumn, ColumnLiteral)>,
) {
    let AssignmentTarget::ColumnName(column) = target else {
        return;
    };
    if let (Some(column), Some(literal)) = (last_name(column), column_literal(value)) {
        written.push((WrittenColumn::Named(column), literal));
    }
}

/// The last part of a name, which is the column's or the table's own.
fn last_name(name: &ObjectName) -> Option<String> {
    match name.0.last()? {
        ObjectNamePart::Identifier(ident) => Some(ident.value.clone()),
        _ => None,
    }
}

fn column_literal(value: &Expr) -> Option<ColumnLiteral> {
    if written_moment(value).is_some() {
        return Some(ColumnLiteral::Moment);
    }
    match value {
        Expr::Nested(inner) => column_literal(inner),
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => column_literal(expr),
        Expr::Value(value) => match &value.value {
            Value::Number(number, _) if number.contains(['.', 'e', 'E']) => {
                Some(ColumnLiteral::NumberWithAFraction)
            }
            Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_) => {
                Some(ColumnLiteral::Bytes)
            }
            _ => None,
        },
        Expr::Prefixed { prefix, .. } if prefix.value.eq_ignore_ascii_case("_binary") => {
            Some(ColumnLiteral::Bytes)
        }
        _ => None,
    }
}

/// The word inside `TIMESTAMP('2024-01-02 03:04:05.000000')`, which is how
/// MySqlConnector writes every `DateTime` into the text of a statement.
///
/// Only a moment written the way MySQL prints one, with no more than six
/// places of a second, is read: measured on MySQL 8.4.11, such a call stores
/// into a `DATETIME` or `TIMESTAMP` column, and compares with one, as the word
/// it names does — `'.5'` into a `DATETIME` rounds to the next second both
/// ways. Written into any other column the two differ (a `BIGINT` stores
/// `20240102030406` for the call), and a moment that is no date is 1292, so
/// those are left for the caller to refuse.
pub(crate) fn written_moment(value: &Expr) -> Option<&str> {
    let Expr::Function(function) = value else {
        return None;
    };
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if !name.value.eq_ignore_ascii_case("TIMESTAMP")
        || name.quote_style.is_some()
        || function.over.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || !matches!(function.parameters, FunctionArguments::None)
    {
        return None;
    }
    let FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(written)))] =
        arguments.args.as_slice()
    else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let Value::SingleQuotedString(moment) = &written.value else {
        return None;
    };
    let places = moment
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let places = u8::try_from(places).ok().filter(|places| *places <= 6)?;
    (crate::normalize_datetime_with_precision(moment, places).as_deref() == Some(moment.as_str()))
        .then_some(moment.as_str())
}
