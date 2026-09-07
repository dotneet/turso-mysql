//! Source-level metadata for the checked MySQL `SELECT` literal subset.
//!
//! This module deliberately describes SQL syntax rather than MySQL wire
//! fields. The adapter can map the descriptor to a column definition while
//! retaining the literal spelling that MySQL uses when it chooses a display
//! width.

use sqlparser::ast::{Expr, TrimWhereField, UnaryOperator, Value};

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
    /// A scalar call, whose result shape is a rule over the columns it names
    /// and the characters its literals contribute.
    ScalarCall {
        function: ScalarFunction,
        columns: Vec<String>,
        literal_characters: u32,
        not_null: bool,
    },
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

/// The scalar functions whose MySQL result shape has been measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunction {
    /// `LOWER` and `UPPER`, which answer their argument's own shape.
    KeepsTextShape,
    /// `LENGTH`, in bytes, and `CHAR_LENGTH`, in characters.
    CountsText,
    /// `NOW` and `CURRENT_TIMESTAMP`, which read no column.
    Now,
    /// `ABS`, which answers its argument's own numeric shape.
    KeepsNumericShape,
    /// `ROUND` with one argument, which answers a whole number however wide
    /// the argument was.
    Truncates,
    /// `IFNULL` and `COALESCE`, which answer the column's shape and cannot be
    /// null when a later argument cannot.
    Defaulted,
    /// `CONCAT`, whose answer is as wide as its arguments laid end to end.
    Concatenates,
    /// `LEFT` and `RIGHT`, whose answer is as wide as the count they were
    /// asked for.
    TakesCharacters,
    /// A `CASE` or `IF`, whose answer is as wide as its widest branch.
    Branches,
    /// `REPEAT`, whose answer is as wide as its column's character length times the repeat count.
    Repeats,
    /// `LOCATE` and `INSTR`, which answer a signed 64-bit integer of length 11 and decimals 0.
    Locates,
    /// `HEX`, whose answer is as wide as its column's character length times 8.
    Hexadecimal,
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
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => classify_branches(
            operand.as_deref(),
            conditions.iter().map(|when| &when.result),
            else_result.as_deref(),
        ),
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => classify_substring(
            expr,
            substring_from.as_deref(),
            substring_for.as_deref(),
        ),
        Expr::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters,
        } => classify_trim(
            *trim_where,
            trim_what.as_deref(),
            expr,
            trim_characters.as_deref(),
        ),
        Expr::Floor { expr, field } => classify_floor_ceil(expr, field),
        Expr::Ceil { expr, field } => classify_floor_ceil(expr, field),
        Expr::BinaryOp { .. } => classify_arithmetic(expr).map(StaticSelectMetadata::Arithmetic),
        Expr::Function(function) if is_count_call(function) => Some(StaticSelectMetadata::Count),
        Expr::Function(function) => column_aggregate_argument(function)
            .map(|(kind, column)| StaticSelectMetadata::ColumnAggregate {
                column_name: column.value.clone(),
                kind,
            })
            .or_else(|| scalar_call(function)),
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

/// Classifies a `CASE` or an `IF`, whose answer is as wide as its widest
/// branch.
///
/// Measured on MySQL 8.4.11: `CASE WHEN n > 1 THEN 'y' ELSE 'n' END` answers a
/// `VAR_STRING` of length 4 — one character, four bytes each — and is NOT NULL.
/// Every branch has to be a string literal, or `NULL`, for that width to be
/// knowable.
///
/// Two things make the answer nullable, both measured: no `ELSE`, because a row
/// matching nothing answers NULL, and a `NULL` branch. Either way the width is
/// still the widest string branch — `CASE WHEN n < 3 THEN 'low' END` and
/// `... THEN 'low' ELSE NULL END` both answer length 3 with no `NOT_NULL` flag.
pub(super) fn classify_branches<'a>(
    operand: Option<&Expr>,
    results: impl Iterator<Item = &'a Expr>,
    else_result: Option<&'a Expr>,
) -> Option<StaticSelectMetadata> {
    // A `CASE col WHEN ...` compares its operand, which raises the coercion
    // question a `WHERE` comparison raises and has not been measured here.
    if operand.is_some() {
        return None;
    }
    let mut characters = 0u32;
    let mut nullable = else_result.is_none();
    for result in results.chain(else_result) {
        let Expr::Value(value) = result else {
            return None;
        };
        match &value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                characters = characters.max(text.chars().count() as u32);
            }
            Value::Null => nullable = true,
            _ => return None,
        }
    }
    // Every branch was NULL, so there is no width to answer with.
    if characters == 0 {
        return None;
    }
    Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::Branches,
        columns: Vec::new(),
        literal_characters: characters,
        not_null: !nullable,
    })
}

/// Classifies `TRIM(...)`, which answers its column's own shape.
///
/// Measured on MySQL 8.4.11: every form over a `VARCHAR(8)` answers a
/// `VAR_STRING` of length 8 with no flags, including over a `NOT NULL` column
/// — the same shape `LOWER` answers.
///
/// What to trim has to be a single character. MySQL removes whole copies of
/// what it was given, where the engine removes any of the characters in it, and
/// the two only agree when there is one character to remove: measured,
/// `TRIM(LEADING 'ax' FROM 'xaxabxa')` answers the string unchanged where the
/// engine would strip the leading `xaxa`.
///
/// `TRIM(v)` and `TRIM([side] 'x' FROM v)` are the forms taken. MySQL's bare
/// `TRIM(LEADING FROM v)` is not, because the parser library does not read it.
pub(super) fn classify_trim(
    trim_where: Option<TrimWhereField>,
    trim_what: Option<&Expr>,
    expr: &Expr,
    trim_characters: Option<&[Expr]>,
) -> Option<StaticSelectMetadata> {
    // The bracketed list is another dialect's spelling and is not MySQL's.
    if trim_characters.is_some() {
        return None;
    }
    let Expr::Identifier(column) = expr else {
        return None;
    };
    match trim_what {
        Some(trim_what) => {
            trim_single_character(trim_what)?;
        }
        // MySQL spells a bare side `TRIM(LEADING FROM v)`, which the parser
        // library does not read; what it reads instead, `TRIM(LEADING v)`, is
        // not MySQL, so a side with nothing to trim is refused.
        None if trim_where.is_some() => return None,
        None => {}
    }
    Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::KeepsTextShape,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Reads the one character a `TRIM` was asked to remove.
pub(crate) fn trim_single_character(trim_what: &Expr) -> Option<&str> {
    let Expr::Value(value) = trim_what else {
        return None;
    };
    let (Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)) = &value.value else {
        return None;
    };
    (text.chars().count() == 1).then_some(text.as_str())
}

/// Classifies `SUBSTRING(...)`.
///
/// Measured on MySQL 8.4.11: `SUBSTRING(v, 1, 2)` over a `VARCHAR(8)` answers
/// a `VAR_STRING` of length 8 — the count asked for, four bytes a character —
/// like `LEFT` and `RIGHT` already do.
fn classify_substring(
    expr: &Expr,
    substring_from: Option<&Expr>,
    substring_for: Option<&Expr>,
) -> Option<StaticSelectMetadata> {
    let Expr::Identifier(column) = expr else {
        return None;
    };
    let from_expr = substring_from?;
    if !is_numeric_literal_or_signed(from_expr) {
        return None;
    }
    let for_expr = substring_for?;
    let Expr::Value(value) = for_expr else {
        return None;
    };
    let Value::Number(digits, false) = &value.value else {
        return None;
    };
    let count: u32 = digits.parse().ok()?;
    Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::TakesCharacters,
        columns: vec![column.value.clone()],
        literal_characters: count,
        not_null: false,
    })
}

fn classify_floor_ceil(
    expr: &Expr,
    field: &sqlparser::ast::CeilFloorKind,
) -> Option<StaticSelectMetadata> {
    if !matches!(
        field,
        sqlparser::ast::CeilFloorKind::DateTimeField(sqlparser::ast::DateTimeField::NoDateTime)
    ) {
        return None;
    }
    let Expr::Identifier(column) = expr else {
        return None;
    };
    Some(StaticSelectMetadata::ScalarCall {
        function: ScalarFunction::Truncates,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

fn is_numeric_literal_or_signed(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(&value.value, Value::Number(_, false)),
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))),
        _ => false,
    }
}

/// Classifies the scalar calls whose MySQL result shape has been measured.
///
/// Each reads one plain column, or none: an expression argument would need a
/// length this cannot work out.
pub(super) fn scalar_call(function: &sqlparser::ast::Function) -> Option<StaticSelectMetadata> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some() || !is_plain_aggregate(function) {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    let named = |candidates: &[&str]| {
        candidates
            .iter()
            .any(|candidate| name.value.eq_ignore_ascii_case(candidate))
    };
    if named(&["NOW", "CURRENT_TIMESTAMP"]) {
        return arguments
            .args
            .is_empty()
            .then(|| StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::Now,
                columns: Vec::new(),
                literal_characters: 0,
                not_null: true,
            });
    }
    // `IFNULL(column, literal)` cannot be null, which is the whole reason a
    // client writes it, so the second argument has to be one that is not.
    if named(&["IFNULL", "COALESCE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(fallback))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        if !matches!(
            classify_static_select_expr(fallback),
            Some(StaticSelectMetadata::Integer { .. } | StaticSelectMetadata::Boolean(_))
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Defaulted,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: true,
        });
    }
    // MySQL's `IF` is the call spelling of a two-branch `CASE`, and answers
    // the same shape.
    if named(&["IF"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(_)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            then_result,
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            else_result,
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        return classify_branches(None, std::iter::once(then_result), Some(else_result));
    }
    // `CONCAT` is as wide as its arguments laid end to end, so every one of
    // them counts: a column contributes its own width and a string literal the
    // characters it spells.
    if named(&["CONCAT"]) {
        let mut columns = Vec::new();
        let mut literal_characters = 0u32;
        for argument in &arguments.args {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                argument
            else {
                return None;
            };
            match expr {
                Expr::Identifier(column) => columns.push(column.value.clone()),
                Expr::Value(value) => match &value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        literal_characters =
                            literal_characters.checked_add(text.chars().count() as u32)?;
                    }
                    _ => return None,
                },
                _ => return None,
            }
        }
        if columns.is_empty() {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Concatenates,
            columns,
            literal_characters,
            not_null: false,
        });
    }
    // `REPLACE(col, from, to)` answers the column's own text shape.
    if named(&["REPLACE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(from)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(to))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let (Expr::Value(from_val), Expr::Value(to_val)) = (from, to) else {
            return None;
        };
        if !matches!(
            &from_val.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) || !matches!(
            &to_val.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::KeepsTextShape,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `LEFT` and `RIGHT` are as wide as the count they were asked for, which
    // has to be a literal for that to be knowable. `SUBSTRING` is not here:
    // sqlparser gives it its own AST shape rather than a call.
    if named(&["LEFT", "RIGHT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = count else {
            return None;
        };
        let Value::Number(digits, false) = &value.value else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::TakesCharacters,
            columns: vec![column.value.clone()],
            literal_characters: digits.parse().ok()?,
            not_null: false,
        });
    }
    // `REPEAT(col, count)` answers a width proportional to the count.
    if named(&["REPEAT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = count else {
            return None;
        };
        let Value::Number(digits, false) = &value.value else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Repeats,
            columns: vec![column.value.clone()],
            literal_characters: digits.parse().ok()?,
            not_null: false,
        });
    }
    // `LPAD` and `RPAD` are as wide as the count they were asked for.
    if named(&["LPAD", "RPAD"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(count)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(pad))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(count_val) = count else {
            return None;
        };
        let Value::Number(digits, false) = &count_val.value else {
            return None;
        };
        let Expr::Value(pad_val) = pad else {
            return None;
        };
        if !matches!(
            &pad_val.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::TakesCharacters,
            columns: vec![column.value.clone()],
            literal_characters: digits.parse().ok()?,
            not_null: false,
        });
    }
    // `INSTR(str, substr)` takes the column first and the substring literal second.
    if named(&["INSTR"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(substr))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = substr else {
            return None;
        };
        if !matches!(
            &value.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Locates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `LOCATE(substr, str)` takes the substring literal first and the column second.
    if named(&["LOCATE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            substr,
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(column)))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = substr else {
            return None;
        };
        if !matches!(
            &value.value,
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
        ) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Locates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Identifier(column),
    ))] = arguments.args.as_slice()
    else {
        return None;
    };
    // TRIM is not here: MySQL's has LEADING, TRAILING and BOTH forms that
    // sqlparser gives their own shape, so it is read in `classify_trim`.
    let function = if named(&["LOWER", "UPPER", "REVERSE"]) {
        ScalarFunction::KeepsTextShape
    } else if named(&["HEX"]) {
        ScalarFunction::Hexadecimal
    } else if named(&["LENGTH", "CHAR_LENGTH", "CHARACTER_LENGTH"]) {
        ScalarFunction::CountsText
    } else if named(&["ABS"]) {
        ScalarFunction::KeepsNumericShape
    // FLOOR and CEIL are their own AST shapes, classified above.
    } else if named(&["ROUND", "CEILING"]) {
        ScalarFunction::Truncates
    } else {
        return None;
    };
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
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
