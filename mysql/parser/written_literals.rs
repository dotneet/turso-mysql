//! Which columns a statement writes a literal into whose meaning turns on the
//! column: a number with a fraction, which a column of bytes stores in
//! MySQL's spelling of it, and a string of bytes, which only a column of bytes
//! takes as it stands.
//!
//! Only the frontend knows each column's type, so this reads the statement's
//! literals out and leaves the frontend to hold them to their columns.

use crate::{parse_one_statement, ParseError, SessionSqlMode};
use sqlparser::ast::{
    AssignmentTarget, Expr, ObjectName, ObjectNamePart, OnInsert, SetExpr, Statement, TableFactor,
    TableObject, UnaryOperator, Value,
};

/// A literal whose meaning turns on the column it is written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnLiteral {
    /// A number written with a point or an exponent — `1.5`, `1e3`.
    NumberWithAFraction,
    /// A string of bytes: `X'..'`, `0x..`, `b'..'`, or a word or hexadecimal
    /// literal after `_binary`.
    Bytes,
}

/// What a statement writes these literals into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenLiterals {
    /// The tables the statement may write: the one an `INSERT` names, every
    /// table a joined `UPDATE` names.
    pub tables: Vec<String>,
    /// Each column one of these literals is written into, in the order
    /// written.
    pub columns: Vec<(String, ColumnLiteral)>,
}

/// Reads what a statement writes these literals into; `None` when it writes
/// none, or when the statement is no `INSERT` or `UPDATE` this reads.
pub fn literals_written_into_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<WrittenLiterals>, ParseError> {
    let (tables, columns) = match parse_one_statement(sql, mode)? {
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
                        for (column, value) in insert.columns.iter().zip(row.iter()) {
                            if let (Some(column), Some(literal)) =
                                (last_name(column), column_literal(value))
                            {
                                written.push((column, literal));
                            }
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
    written: &mut Vec<(String, ColumnLiteral)>,
) {
    let AssignmentTarget::ColumnName(column) = target else {
        return;
    };
    if let (Some(column), Some(literal)) = (last_name(column), column_literal(value)) {
        written.push((column, literal));
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
