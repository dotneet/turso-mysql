//! Source-level metadata for the checked MySQL `SELECT` literal subset.
//!
//! This module deliberately describes SQL syntax rather than MySQL wire
//! fields. The adapter can map the descriptor to a column definition while
//! retaining the literal spelling that MySQL uses when it chooses a display
//! width.

use sqlparser::ast::{Expr, UnaryOperator, Value};

/// The sign written around a checked integer literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticIntegerSign {
    /// No explicit sign was written.
    None,
    /// The literal used an explicit `+` sign.
    Positive,
    /// The literal used an explicit `-` sign.
    Negative,
}

/// Syntax provenance for a result expression whose metadata is fixed before
/// execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticSelectMetadata {
    /// A signed 64-bit integer literal. The digit count includes leading zeroes.
    Integer {
        digit_count: u32,
        sign: StaticIntegerSign,
    },
    /// A SQL boolean literal.
    Boolean(bool),
    /// A SQL NULL literal.
    Null,
    /// A `COUNT`, whose result metadata is the same whatever it counts.
    Count,
}

/// Source-level kind of one checked `SELECT` projection item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticSelectProjectionMetadata {
    /// A literal whose result metadata is fixed by its source spelling.
    Literal(StaticSelectMetadata),
    /// A wildcard whose expansion can add result columns.
    Wildcard,
    /// An expression whose result metadata is not described by this module.
    Other,
}

/// Classifies the checked literal forms whose result metadata is independent
/// of returned rows or prepared parameter values.
pub(super) fn classify_static_select_expr(expr: &Expr) -> Option<StaticSelectMetadata> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(digits, false) => classify_integer(digits, StaticIntegerSign::None),
            Value::Boolean(value) => Some(StaticSelectMetadata::Boolean(*value)),
            Value::Null => Some(StaticSelectMetadata::Null),
            _ => None,
        },
        Expr::UnaryOp { op, expr } => {
            let sign = match op {
                UnaryOperator::Plus => StaticIntegerSign::Positive,
                UnaryOperator::Minus => StaticIntegerSign::Negative,
                _ => return None,
            };
            let Expr::Value(value) = expr.as_ref() else {
                return None;
            };
            let Value::Number(digits, false) = &value.value else {
                return None;
            };
            classify_integer(digits, sign)
        }
        Expr::Nested(inner) => classify_static_select_expr(inner),
        Expr::Function(function) if is_count_call(function) => Some(StaticSelectMetadata::Count),
        _ => None,
    }
}

/// Reports whether a call is a plain `COUNT`, which is the one aggregate whose
/// result metadata does not depend on what it counts.
///
/// Measured on MySQL 8.4.11: `COUNT(*)` and `COUNT(col)` both answer a non-null
/// `LONGLONG` of length 21, and 0 rather than NULL on an empty table. `MIN` and
/// `MAX` answer their argument's own type, and `SUM` and `AVG` answer DECIMAL,
/// so none of those belong here.
pub(super) fn is_count_call(function: &sqlparser::ast::Function) -> bool {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    if !name.value.eq_ignore_ascii_case("COUNT") || name.quote_style.is_some() {
        return false;
    }
    // Anything past the plain call — DISTINCT, an OVER clause, a filter — has
    // its own meaning that this does not model.
    if function.filter.is_some()
        || function.over.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.uses_odbc_syntax
        || function.parameters != sqlparser::ast::FunctionArguments::None
    {
        return false;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return false;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return false;
    }
    matches!(
        arguments.args.as_slice(),
        [sqlparser::ast::FunctionArg::Unnamed(
            sqlparser::ast::FunctionArgExpr::Wildcard
                | sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(_)),
        )]
    )
}

fn classify_integer(digits: &str, sign: StaticIntegerSign) -> Option<StaticSelectMetadata> {
    let magnitude = digits.parse::<u64>().ok()?;
    let digit_count = u32::try_from(digits.len()).ok()?;
    let in_range = match sign {
        StaticIntegerSign::Negative => magnitude <= (i64::MAX as u64) + 1,
        StaticIntegerSign::None | StaticIntegerSign::Positive => magnitude <= i64::MAX as u64,
    };
    in_range.then_some(StaticSelectMetadata::Integer { digit_count, sign })
}
