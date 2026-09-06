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
    /// Integer arithmetic, whose result type is worked out from its operands.
    ///
    /// Like `ColumnAggregate` this is finished by the server, which is the only
    /// side that can see a column's precision.
    Arithmetic(ArithmeticShape),
    /// An aggregate whose result type is worked out from the named column.
    ///
    /// Unlike every other variant this one cannot be resolved here: the type
    /// lives in the table, so the server finishes it once it has the column.
    ColumnAggregate {
        column_name: String,
        kind: ColumnAggregateKind,
    },
}

/// One integer arithmetic expression, whose result type is a rule over its
/// operands rather than a type of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithmeticShape {
    pub operator: ArithmeticOperator,
    pub left: ArithmeticOperand,
    pub right: ArithmeticOperand,
}

impl ArithmeticShape {
    /// Reports whether any operand is a column, which is what makes the shape
    /// need the table before its type can be worked out.
    pub fn names_a_column(&self) -> bool {
        [&self.left, &self.right]
            .into_iter()
            .any(|operand| match operand {
                ArithmeticOperand::Literal { .. } => false,
                ArithmeticOperand::Column { .. } => true,
                ArithmeticOperand::Nested(shape) => shape.names_a_column(),
            })
    }
}

/// One side of an integer arithmetic expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArithmeticOperand {
    /// An integer literal. MySQL uses its digit count as the precision, so a
    /// sign and any leading zeroes do not count.
    Literal { digit_count: u32 },
    /// A column, whose precision and nullability live in the table.
    Column { column_name: String },
    /// A nested arithmetic expression.
    Nested(Box<ArithmeticShape>),
}

/// The arithmetic operators whose MySQL result shape has been measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
}

/// The aggregates whose result type is a rule over the argument column's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAggregateKind {
    /// `MIN` or `MAX`, which answer the column's own type.
    MinMax,
    /// `SUM`, which widens a decimal by 22 digits.
    Sum,
    /// `AVG`, which widens a decimal by 4 digits and 4 decimal places.
    Avg,
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
        Expr::BinaryOp { .. } => classify_arithmetic(expr).map(StaticSelectMetadata::Arithmetic),
        Expr::Function(function) if is_count_call(function) => Some(StaticSelectMetadata::Count),
        Expr::Function(function) => column_aggregate_argument(function).map(|(kind, column)| {
            StaticSelectMetadata::ColumnAggregate {
                column_name: column.value.clone(),
                kind,
            }
        }),
        _ => None,
    }
}

/// Classifies the integer arithmetic MySQL's result shape has been measured for.
///
/// Division is taken only at the top, because a nested one makes every operator
/// above it decimal arithmetic, whose precision and scale rules are their own
/// and have not been measured.
pub(super) fn classify_arithmetic(expr: &Expr) -> Option<ArithmeticShape> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let operator = match op {
        sqlparser::ast::BinaryOperator::Plus => ArithmeticOperator::Add,
        sqlparser::ast::BinaryOperator::Minus => ArithmeticOperator::Subtract,
        sqlparser::ast::BinaryOperator::Multiply => ArithmeticOperator::Multiply,
        sqlparser::ast::BinaryOperator::Divide => ArithmeticOperator::Divide,
        _ => return None,
    };
    Some(ArithmeticShape {
        operator,
        left: classify_arithmetic_operand(left)?,
        right: classify_arithmetic_operand(right)?,
    })
}

fn classify_arithmetic_operand(expr: &Expr) -> Option<ArithmeticOperand> {
    match expr {
        Expr::Identifier(column) => Some(ArithmeticOperand::Column {
            column_name: column.value.clone(),
        }),
        Expr::Nested(inner) => classify_arithmetic_operand(inner),
        Expr::BinaryOp { .. } => {
            let shape = classify_arithmetic(expr)?;
            if shape.operator == ArithmeticOperator::Divide {
                return None;
            }
            Some(ArithmeticOperand::Nested(Box::new(shape)))
        }
        _ => match classify_static_select_expr(expr)? {
            StaticSelectMetadata::Integer { digit_count, .. } => {
                Some(ArithmeticOperand::Literal { digit_count })
            }
            _ => None,
        },
    }
}

/// Returns the aggregate kind and the column a plain `MIN`, `MAX`, `SUM` or
/// `AVG` names.
///
/// Each answers a type worked out from that column — measured on MySQL 8.4.11,
/// `MIN` and `MAX` give the column's own type while `SUM` and `AVG` give a
/// decimal derived from its precision — so the call has to name a column.
/// `MIN(*)` is not even valid SQL, and an expression argument would need a type
/// this cannot work out.
pub(super) fn column_aggregate_argument(
    function: &sqlparser::ast::Function,
) -> Option<(ColumnAggregateKind, &sqlparser::ast::Ident)> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some() {
        return None;
    }
    let kind = if name.value.eq_ignore_ascii_case("MIN") || name.value.eq_ignore_ascii_case("MAX") {
        ColumnAggregateKind::MinMax
    } else if name.value.eq_ignore_ascii_case("SUM") {
        ColumnAggregateKind::Sum
    } else if name.value.eq_ignore_ascii_case("AVG") {
        ColumnAggregateKind::Avg
    } else {
        return None;
    };
    if !is_plain_aggregate(function) {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => Some((kind, column)),
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
    if !is_plain_aggregate(function) {
        return false;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return false;
    };
    matches!(
        arguments.args.as_slice(),
        [sqlparser::ast::FunctionArg::Unnamed(
            sqlparser::ast::FunctionArgExpr::Wildcard
                | sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(_)),
        )]
    )
}

/// Reports whether a call is the bare aggregate form and nothing more.
///
/// Anything past it — DISTINCT, an OVER clause, a filter — has its own meaning
/// that this module does not model.
fn is_plain_aggregate(function: &sqlparser::ast::Function) -> bool {
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
    arguments.duplicate_treatment.is_none() && arguments.clauses.is_empty()
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
