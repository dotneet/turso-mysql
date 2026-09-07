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
    /// The same aggregate over a window, which answers almost the same shape.
    WindowAggregate {
        column_name: String,
        kind: ColumnAggregateKind,
    },
    /// A `COUNT` over a window, which answers the same shape a plain `COUNT`
    /// does apart from the binary flag.
    WindowCount,
    /// A scalar subquery in a projection, which answers the shape the aggregate
    /// inside it answers — nullable, whatever the aggregate is.
    ScalarSubquery(Box<StaticSelectMetadata>),
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
    /// `CURDATE` and `CURRENT_DATE`, which read no column either and answer
    /// the day alone.
    Today,
    /// `CURTIME` and `CURRENT_TIME`, which answer the time of day alone.
    TimeOfDay,
    /// `YEAR`, which reads the year out of a date and answers a `YEAR`.
    ReadsTheYear,
    /// `MONTH` and `DAY`, which read a smaller part of the same date.
    ReadsAMonthOrDay,
    /// `HOUR`, which reads the hour out of a moment.
    ReadsTheHour,
    /// `MINUTE` and `SECOND`, which read a smaller part of the same
    /// moment.
    ReadsAMinuteOrSecond,
    /// `DATEDIFF`, which answers the days between two dates.
    CountsDaysBetween,
    /// `DATE_ADD` and `DATE_SUB` over an interval of whole days, months or
    /// years, which answer the column's own kind.
    ShiftsByWholeDays,
    /// `DATE_ADD` and `DATE_SUB` over an interval carrying a time, which
    /// answer a moment whatever the column was.
    ShiftsByTime,
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
    /// `JSON_EXTRACT` over one path, which answers the JSON value it found —
    /// a string comes back with its quotes.
    ReadsAJsonValue,
    /// `JSON_UNQUOTE` over that, which answers the text inside the value.
    ReadsJsonText,
    /// `JSON_VALID`, which answers one or zero.
    ChecksJson,
    /// `JSON_TYPE`, which names the kind of the document it was given.
    NamesAJsonKind,
    /// `JSON_LENGTH`, which counts what a document holds at the top.
    CountsJsonMembers,
    /// `JSON_KEYS`, which answers an object's keys as a document of their own.
    ListsJsonKeys,
    /// `JSON_QUOTE`, which writes text as a JSON string.
    QuotesAsJson,
    /// `JSON_ARRAY` and `JSON_OBJECT`, which build a document out of what they
    /// are given.
    BuildsJson,
    /// `JSON_SET`, `JSON_INSERT`, `JSON_REPLACE` and `JSON_REMOVE`, which
    /// answer a document with one member changed.
    ChangesJson,
    /// `DATE_FORMAT` over a literal format, whose answer is as wide as the
    /// format could make it.
    WritesAMoment,
    /// `STR_TO_DATE` over a literal format naming only day parts.
    ReadsADay,
    /// `STR_TO_DATE` over a literal format naming only clock parts.
    ReadsAClock,
    /// `STR_TO_DATE` over a literal format naming both.
    ReadsAMoment,
    /// `RAND` with no argument, which answers a double between zero and one.
    Randomises,
    /// `UUID`, which answers a new identifier and reads no column.
    Identifies,
    /// `MD5`, whose answer is thirty-two hexadecimal characters.
    Digests,
    /// `ROW_NUMBER`, `RANK`, `DENSE_RANK` and `NTILE` over a window, which
    /// answer an unsigned 64-bit row count.
    RanksRows,
    /// `PERCENT_RANK` and `CUME_DIST` over a window, which answer a double
    /// between zero and one.
    RanksFraction,
    /// `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE` and `NTH_VALUE` over a
    /// window, which answer another row's value for the column they name, and
    /// NULL where there is no such row.
    ShiftsRow,
    /// `SQRT` and `POW`, which answer a floating-point DOUBLE of length 23 and not-fixed decimals.
    Approximates,
    /// `MOD`, which answers its argument's own numeric shape but can be null.
    Modulo,
    /// `GREATEST` and `LEAST`, which answer the widest shape among their arguments.
    Widest,
    /// `NULLIF`, which answers its first argument's shape but can always be null.
    NullsOnMatch,
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
    /// `GROUP_CONCAT`, which answers a BLOB of length 65536 and 31 decimals.
    Concatenated,
    /// `STDDEV_SAMP`, which answers a DOUBLE whatever the column is.
    DeviatesBySample,
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
        } => classify_substring(expr, substring_from.as_deref(), substring_for.as_deref()),
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
        Expr::BinaryOp { .. } => classify_json_arrow(expr)
            .or_else(|| classify_arithmetic(expr).map(StaticSelectMetadata::Arithmetic)),
        Expr::Subquery(query) => classify_scalar_subquery(query),
        Expr::Function(function) if function.over.is_some() => classify_window_call(function),
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

/// Classifies `col -> '$.path'` and `col ->> '$.path'`, which are MySQL's own
/// spellings of `JSON_EXTRACT` and the `JSON_UNQUOTE` around it.
fn classify_json_arrow(expr: &Expr) -> Option<StaticSelectMetadata> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let function = match op {
        sqlparser::ast::BinaryOperator::Arrow => ScalarFunction::ReadsAJsonValue,
        sqlparser::ast::BinaryOperator::LongArrow => ScalarFunction::ReadsJsonText,
        _ => return None,
    };
    let Expr::Identifier(column) = left.as_ref() else {
        return None;
    };
    let Expr::Value(path) = right.as_ref() else {
        return None;
    };
    let (Value::SingleQuotedString(path) | Value::DoubleQuotedString(path)) = &path.value else {
        return None;
    };
    if !names_a_plain_json_path(path) {
        return None;
    }
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
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

/// Classifies a scalar subquery in a projection.
///
/// Measured on MySQL 8.4.11: it answers the shape its aggregate answers on its
/// own — a `MAX` over an `INT` a `LONG` of 11, a `SUM` over a `DECIMAL(10,2)` a
/// `NEWDECIMAL` of 34 with its scale, a `COUNT` a `LONGLONG` of 21 — and is
/// nullable whatever that aggregate is, where a plain `COUNT` is NOT NULL.
///
/// Only an aggregate is taken. A subquery answering a column would answer the
/// column's own shape and a row that is not there as NULL, which is a rule of
/// its own and unmeasured here.
pub(super) fn classify_scalar_subquery(
    query: &sqlparser::ast::Query,
) -> Option<StaticSelectMetadata> {
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let [item] = select.projection.as_slice() else {
        return None;
    };
    let (sqlparser::ast::SelectItem::UnnamedExpr(expr)
    | sqlparser::ast::SelectItem::ExprWithAlias { expr, .. }) = item
    else {
        return None;
    };
    let inner = match expr {
        Expr::Function(function) if is_count_call(function) => StaticSelectMetadata::Count,
        Expr::Function(function) => {
            let (kind, column) = column_aggregate_argument(function)?;
            StaticSelectMetadata::ColumnAggregate {
                column_name: column.value.clone(),
                kind,
            }
        }
        _ => return None,
    };
    Some(StaticSelectMetadata::ScalarSubquery(Box::new(inner)))
}

/// Classifies the window calls whose MySQL result shape has been measured.
///
/// Measured on MySQL 8.4.11, whatever the window is over:
///
/// * `ROW_NUMBER()`, `RANK()`, `DENSE_RANK()` and `NTILE(n)` answer a
///   `LONGLONG` of length 21 and no decimals, carrying the NOT NULL, unsigned
///   and numeric flags.
/// * `PERCENT_RANK()` and `CUME_DIST()` answer a `DOUBLE` of length 23 with the
///   not-fixed decimals value, carrying the NOT NULL and numeric flags.
/// * `LAG(col)`, `LEAD(col)`, `FIRST_VALUE(col)`, `LAST_VALUE(col)` and
///   `NTH_VALUE(col, n)` answer the column's own shape, widened to
///   `LONGLONG` where it is an integer, and are always nullable because the row
///   they reach for may not be there. They carry the numeric flag and, unlike
///   `ABS`, not the binary one.
///
/// The window has to be written out — a named one is a spelling of its own —
/// and every `PARTITION BY` and `ORDER BY` term has to be a plain column, since
/// that is what the checked ordering path can answer for. A frame clause is
/// refused: it changes nothing for any of these, and taking one silently would
/// mean taking it for the functions where it does change something.
pub(super) fn classify_window_call(
    function: &sqlparser::ast::Function,
) -> Option<StaticSelectMetadata> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.uses_odbc_syntax
        || function.parameters != sqlparser::ast::FunctionArguments::None
    {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    checked_window_spec(function.over.as_ref()?)?;
    let named = |candidates: &[&str]| {
        candidates
            .iter()
            .any(|candidate| name.value.eq_ignore_ascii_case(candidate))
    };
    let fixed = |function| StaticSelectMetadata::ScalarCall {
        function,
        columns: Vec::new(),
        literal_characters: 0,
        not_null: true,
    };
    if named(&["ROW_NUMBER", "RANK", "DENSE_RANK"]) {
        return arguments
            .args
            .is_empty()
            .then(|| fixed(ScalarFunction::RanksRows));
    }
    if named(&["PERCENT_RANK", "CUME_DIST"]) {
        return arguments
            .args
            .is_empty()
            .then(|| fixed(ScalarFunction::RanksFraction));
    }
    // Measured: `NTILE(0)` answers 1210, so the count has to be one or more.
    if named(&["NTILE"]) {
        let [argument] = arguments.args.as_slice() else {
            return None;
        };
        let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(value),
        )) = argument
        else {
            return None;
        };
        let Value::Number(digits, false) = &value.value else {
            return None;
        };
        return (digits.parse::<u64>().ok()? >= 1).then(|| fixed(ScalarFunction::RanksRows));
    }
    // Measured: a windowed aggregate answers the shape its plain form does,
    // apart from the binary flag, which it does not carry, and MIN and MAX,
    // which widen an INT to LONGLONG where the plain form leaves it LONG.
    if named(&["COUNT"]) {
        return matches!(
            arguments.args.as_slice(),
            [sqlparser::ast::FunctionArg::Unnamed(
                sqlparser::ast::FunctionArgExpr::Wildcard
                    | sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(_)),
            )]
        )
        .then_some(StaticSelectMetadata::WindowCount);
    }
    if named(&["SUM", "AVG", "MIN", "MAX"]) {
        let kind = if named(&["SUM"]) {
            ColumnAggregateKind::Sum
        } else if named(&["AVG"]) {
            ColumnAggregateKind::Avg
        } else {
            ColumnAggregateKind::MinMax
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        return Some(StaticSelectMetadata::WindowAggregate {
            column_name: column.value.clone(),
            kind,
        });
    }
    // `LAG` and `LEAD` read another row of the window and `FIRST_VALUE`,
    // `LAST_VALUE` and `NTH_VALUE` read one of its ends; all five answer the
    // column's own shape and may find no row at all. An offset or a default
    // argument on a `LAG` or `LEAD` brings rules of its own, unmeasured, and
    // `NTH_VALUE(col, 0)` answers 1210, so its count is one or more.
    let takes_a_count = named(&["NTH_VALUE"]);
    if named(&["LAG", "LEAD", "FIRST_VALUE", "LAST_VALUE"]) || takes_a_count {
        let (column, count) = match arguments.args.as_slice() {
            [column] if !takes_a_count => (column, None),
            [column, count] if takes_a_count => (column, Some(count)),
            _ => return None,
        };
        let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )) = column
        else {
            return None;
        };
        if let Some(count) = count {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Value(value),
            )) = count
            else {
                return None;
            };
            let Value::Number(digits, false) = &value.value else {
                return None;
            };
            if digits.parse::<u64>().ok()? < 1 {
                return None;
            }
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::ShiftsRow,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    None
}

/// Reads the window a ranking call is over, if it is one this takes.
pub(crate) fn checked_window_spec(
    over: &sqlparser::ast::WindowType,
) -> Option<&sqlparser::ast::WindowSpec> {
    let sqlparser::ast::WindowType::WindowSpec(spec) = over else {
        return None;
    };
    if spec.window_name.is_some() {
        return None;
    }
    if let Some(frame) = spec.window_frame.as_ref() {
        checked_window_frame(frame)?;
    }
    if !spec
        .partition_by
        .iter()
        .all(|expr| matches!(expr, Expr::Identifier(_)))
    {
        return None;
    }
    for term in &spec.order_by {
        if !matches!(term.expr, Expr::Identifier(_))
            || term.options.nulls_first.is_some()
            || term.with_fill.is_some()
        {
            return None;
        }
    }
    if spec.partition_by.is_empty() && spec.order_by.is_empty() {
        return None;
    }
    Some(spec)
}

/// Reads the frame a window is over, if it is one this takes.
///
/// `ROWS` and `RANGE` are taken and `GROUPS` is not: measured, MySQL 8.4.11
/// answers 1235 for it. A bound has to be `CURRENT ROW`, an unbounded end, or a
/// non-negative whole number of rows or of the ordering column's own units.
pub(crate) fn checked_window_frame(
    frame: &sqlparser::ast::WindowFrame,
) -> Option<&sqlparser::ast::WindowFrame> {
    use sqlparser::ast::WindowFrameUnits;
    if matches!(frame.units, WindowFrameUnits::Groups) {
        return None;
    }
    checked_window_frame_bound(&frame.start_bound)?;
    if let Some(end_bound) = frame.end_bound.as_ref() {
        checked_window_frame_bound(end_bound)?;
    }
    Some(frame)
}

fn checked_window_frame_bound(bound: &sqlparser::ast::WindowFrameBound) -> Option<()> {
    use sqlparser::ast::WindowFrameBound;
    let offset = match bound {
        WindowFrameBound::CurrentRow => return Some(()),
        WindowFrameBound::Preceding(offset) | WindowFrameBound::Following(offset) => offset,
    };
    let Some(offset) = offset else {
        return Some(());
    };
    let Expr::Value(value) = offset.as_ref() else {
        return None;
    };
    let Value::Number(digits, false) = &value.value else {
        return None;
    };
    digits.parse::<u64>().ok().map(|_| ())
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
        } => {
            matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false)))
        }
        _ => false,
    }
}

fn is_scalar_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            &value.value,
            Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::Number(_, _)
                | Value::Null
                | Value::Boolean(_)
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => {
            matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, _)))
        }
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
    let named = |candidates: &[&str]| {
        candidates
            .iter()
            .any(|candidate| name.value.eq_ignore_ascii_case(candidate))
    };
    // MySQL spells the two clock readings with and without their parentheses,
    // and sqlparser gives the bare form no argument list at all rather than an
    // empty one, so both shapes count as no arguments here.
    let takes_nothing = match &function.args {
        sqlparser::ast::FunctionArguments::None => true,
        sqlparser::ast::FunctionArguments::List(arguments) => arguments.args.is_empty(),
        sqlparser::ast::FunctionArguments::Subquery(_) => false,
    };
    if named(&["NOW", "CURRENT_TIMESTAMP"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Now,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    if named(&["CURDATE", "CURRENT_DATE"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Today,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    if named(&["CURTIME", "CURRENT_TIME"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::TimeOfDay,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    // Measured on MySQL 8.4.11: `RAND()` reports NOT NULL, and `UUID()` does
    // not. A seeded `RAND(n)` is refused: the engine has no seeded random, so
    // answering one would answer a different sequence.
    if named(&["RAND"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Randomises,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: true,
        });
    }
    if named(&["UUID"]) {
        return takes_nothing.then(|| StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Identifies,
            columns: Vec::new(),
            literal_characters: 0,
            not_null: false,
        });
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
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
    // `LOCATE(needle, haystack, start)` looks from a place in the haystack
    // rather than from its front. The place has to be a literal, because what
    // it renders to depends on whether it names one.
    if named(&["LOCATE"]) && arguments.args.len() == 3 {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(needle)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(start),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        if !matches!(
            needle,
            Expr::Value(value)
                if matches!(
                    &value.value,
                    Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
                )
        ) {
            return None;
        }
        let Value::Number(digits, false) = &start.value else {
            return None;
        };
        if digits.parse::<u32>().is_err() {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Locates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    if named(&["LOCATE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(substr)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] = arguments.args.as_slice()
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
    // `POW(col, exp)` and `POWER(col, exp)` answer DOUBLE.
    if named(&["POW", "POWER"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(exp))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = exp else {
            return None;
        };
        if !matches!(&value.value, Value::Number(_, _)) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Approximates,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `MOD(col, divisor)` answers the column's own numeric shape.
    if named(&["MOD"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(divisor))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let Expr::Value(value) = divisor else {
            return None;
        };
        if !matches!(&value.value, Value::Number(_, _)) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Modulo,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `NULLIF(col, literal)` answers the column's shape and is always nullable.
    if named(&["NULLIF"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(literal))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        if !is_scalar_literal(literal) {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::NullsOnMatch,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `GREATEST` and `LEAST` with 2 or more arguments.
    if named(&["GREATEST", "LEAST"]) {
        if arguments.args.len() < 2 {
            return None;
        }
        let mut columns = Vec::new();
        let mut max_literal_chars: u32 = 0;
        let mut has_text = false;
        let mut has_numeric = false;

        for arg in &arguments.args {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                arg
            else {
                return None;
            };
            match expr {
                Expr::Identifier(column) => {
                    columns.push(column.value.clone());
                }
                Expr::Value(value) => match &value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        has_text = true;
                        max_literal_chars = max_literal_chars.max(text.chars().count() as u32);
                    }
                    Value::Number(_, _) => {
                        has_numeric = true;
                    }
                    _ => return None,
                },
                Expr::UnaryOp {
                    op: UnaryOperator::Minus | UnaryOperator::Plus,
                    expr: inner,
                } => match inner.as_ref() {
                    Expr::Value(value) if matches!(&value.value, Value::Number(_, _)) => {
                        has_numeric = true;
                    }
                    _ => return None,
                },
                _ => return None,
            }
        }
        if has_text && has_numeric {
            return None;
        }
        if columns.is_empty() {
            return None;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::Widest,
            columns,
            literal_characters: max_literal_chars,
            not_null: false,
        });
    }
    // `JSON_ARRAY(...)` and `JSON_OBJECT(k, v, ...)` build a document out of
    // what they are given. Each argument is a column or a plain literal; a
    // nested call is not read here, and `JSON_OBJECT` takes its arguments in
    // pairs. A boolean literal is refused: MySQL writes `true` where the
    // engine has only the number one to write.
    if named(&["JSON_ARRAY", "JSON_OBJECT"]) {
        if named(&["JSON_OBJECT"]) && arguments.args.len() % 2 != 0 {
            return None;
        }
        let mut columns = Vec::new();
        for argument in &arguments.args {
            json_argument_column(argument, &mut columns)?;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::BuildsJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_SET(doc, '$.a', v)` and its three relatives answer the document
    // with one member changed. Only a path naming one member of the top-level
    // object is taken — `$.a`, not `$.a.b` or `$.a[0]`. Measured on MySQL
    // 8.4.11, the two disagree about a path that names something not there to
    // name: MySQL leaves `JSON_SET('{}', '$.x.y', 1)` alone where the engine
    // builds the missing parent, and MySQL appends `JSON_SET('[1,2]',
    // '$[5]', 9)` where the engine leaves it. A one-step path cannot reach
    // either.
    if named(&["JSON_SET", "JSON_INSERT", "JSON_REPLACE", "JSON_REMOVE"]) {
        let removes = named(&["JSON_REMOVE"]);
        let [document, rest @ ..] = arguments.args.as_slice() else {
            return None;
        };
        if rest.is_empty() || (!removes && rest.len() % 2 != 0) {
            return None;
        }
        let mut columns = Vec::new();
        json_argument_column(document, &mut columns)?;
        for (position, argument) in rest.iter().enumerate() {
            // A remove takes paths alone; the others take a path and a value
            // in turn.
            if removes || position % 2 == 0 {
                let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    Expr::Value(value),
                )) = argument
                else {
                    return None;
                };
                let (Value::SingleQuotedString(path) | Value::DoubleQuotedString(path)) =
                    &value.value
                else {
                    return None;
                };
                if !names_one_member(path) {
                    return None;
                }
                continue;
            }
            json_argument_column(argument, &mut columns)?;
        }
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::ChangesJson,
            columns,
            literal_characters: 0,
            not_null: false,
        });
    }
    // `DATE_ADD(a, INTERVAL 1 DAY)` is an ordinary call whose second argument
    // is sqlparser's own interval node. Measured on MySQL 8.4.11: over a DATE
    // an interval of whole days, months or years answers a DATE, and any
    // other interval — or any DATETIME column — answers a DATETIME.
    if named(&["DATE_ADD", "DATE_SUB"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Interval(interval),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let whole_days = checked_interval_unit(interval)?.1;
        return Some(StaticSelectMetadata::ScalarCall {
            function: if whole_days {
                ScalarFunction::ShiftsByWholeDays
            } else {
                ScalarFunction::ShiftsByTime
            },
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // Measured on MySQL 8.4.11: `DATEDIFF(b, a)` answers the days between the
    // two, counting the date alone, as a LONGLONG of length 9.
    // `JSON_TYPE`, `JSON_LENGTH` and `JSON_KEYS` read a whole column or one
    // path out of it, so each takes a reading rather than a plain column.
    for (names, function) in [
        (["JSON_TYPE"], ScalarFunction::NamesAJsonKind),
        (["JSON_LENGTH"], ScalarFunction::CountsJsonMembers),
        (["JSON_KEYS"], ScalarFunction::ListsJsonKeys),
    ] {
        if !named(&names) {
            continue;
        }
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(read))] =
            arguments.args.as_slice()
        else {
            return None;
        };
        let column = json_reading_column(read)?;
        return Some(StaticSelectMetadata::ScalarCall {
            function,
            columns: vec![column],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `JSON_EXTRACT(col, '$.path')` and the `JSON_UNQUOTE` around it. Only one
    // path is read: MySQL takes several and answers an array of what they
    // found, which is a different shape.
    if named(&["JSON_EXTRACT"]) {
        return json_path_call(arguments, ScalarFunction::ReadsAJsonValue);
    }
    if named(&["JSON_UNQUOTE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Function(inner),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let [sqlparser::ast::ObjectNamePart::Identifier(inner_name)] = inner.name.0.as_slice()
        else {
            return None;
        };
        if inner_name.quote_style.is_some()
            || !inner_name.value.eq_ignore_ascii_case("JSON_EXTRACT")
        {
            return None;
        }
        let sqlparser::ast::FunctionArguments::List(inner_arguments) = &inner.args else {
            return None;
        };
        return json_path_call(inner_arguments, ScalarFunction::ReadsJsonText);
    }
    // `STR_TO_DATE(col, 'fmt')`. The format has to be a literal, because what
    // it names is what says whether the answer is a day, a clock reading or a
    // moment.
    if named(&["STR_TO_DATE"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(format),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let (Value::SingleQuotedString(format) | Value::DoubleQuotedString(format)) = &format.value
        else {
            return None;
        };
        let function = match crate::format_reads(format)? {
            crate::FormatShape::Day => ScalarFunction::ReadsADay,
            crate::FormatShape::Clock => ScalarFunction::ReadsAClock,
            crate::FormatShape::Moment => ScalarFunction::ReadsAMoment,
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function,
            columns: vec![column.value.clone()],
            literal_characters: 0,
            not_null: false,
        });
    }
    // `DATE_FORMAT(col, 'fmt')`. The format has to be a literal, because the
    // answer's width is worked out from it.
    if named(&["DATE_FORMAT"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(format),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        let (Value::SingleQuotedString(format) | Value::DoubleQuotedString(format)) = &format.value
        else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::WritesAMoment,
            columns: vec![column.value.clone()],
            literal_characters: crate::format_width(format),
            not_null: false,
        });
    }
    if named(&["DATEDIFF"]) {
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(left),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(right),
        ))] = arguments.args.as_slice()
        else {
            return None;
        };
        return Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::CountsDaysBetween,
            columns: vec![left.value.clone(), right.value.clone()],
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
    } else if named(&["MD5"]) {
        ScalarFunction::Digests
    } else if named(&["JSON_VALID"]) {
        ScalarFunction::ChecksJson
    } else if named(&["JSON_QUOTE"]) {
        ScalarFunction::QuotesAsJson
    } else if named(&["LENGTH", "CHAR_LENGTH", "CHARACTER_LENGTH"]) {
        ScalarFunction::CountsText
    } else if named(&["ABS"]) {
        ScalarFunction::KeepsNumericShape
    } else if named(&["SQRT"]) {
        ScalarFunction::Approximates
    // FLOOR and CEIL are their own AST shapes, classified above.
    } else if named(&["ROUND", "CEILING", "SIGN"]) {
        ScalarFunction::Truncates
    // Measured on MySQL 8.4.11: each of the three reports no NOT NULL flag
    // even over a NOT NULL column, which the `not_null: false` below says.
    } else if named(&["YEAR"]) {
        ScalarFunction::ReadsTheYear
    } else if named(&["MONTH", "DAY"]) {
        ScalarFunction::ReadsAMonthOrDay
    } else if named(&["HOUR"]) {
        ScalarFunction::ReadsTheHour
    } else if named(&["MINUTE", "SECOND"]) {
        ScalarFunction::ReadsAMinuteOrSecond
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

/// Reads `(column, '$path')`, the one shape of `JSON_EXTRACT` this takes.
///
/// The path has to be a literal, because it names what the answer is, and it
/// is held to the plain member-and-element spelling both MySQL and the engine
/// read the same way. MySQL's wildcards — `$.*`, `$[*]` and `$**` — are not
/// among them.
fn json_path_call(
    arguments: &sqlparser::ast::FunctionArgumentList,
    function: ScalarFunction,
) -> Option<StaticSelectMetadata> {
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Identifier(column),
    )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Value(
        path,
    )))] = arguments.args.as_slice()
    else {
        return None;
    };
    let (Value::SingleQuotedString(path) | Value::DoubleQuotedString(path)) = &path.value else {
        return None;
    };
    if !names_a_plain_json_path(path) {
        return None;
    }
    Some(StaticSelectMetadata::ScalarCall {
        function,
        columns: vec![column.value.clone()],
        literal_characters: 0,
        not_null: false,
    })
}

/// Reads one argument a JSON builder or changer takes, recording its column.
///
/// A column or a plain literal is taken; a nested call is not read here, and a
/// boolean literal is refused because MySQL writes `true` where the engine has
/// only the number one to write.
fn json_argument_column(
    argument: &sqlparser::ast::FunctionArg,
    columns: &mut Vec<String>,
) -> Option<()> {
    let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
        argument
    else {
        return None;
    };
    match expr {
        Expr::Identifier(column) => columns.push(column.value.clone()),
        Expr::Value(value)
            if matches!(
                &value.value,
                Value::SingleQuotedString(_)
                    | Value::DoubleQuotedString(_)
                    | Value::Number(_, _)
                    | Value::Null
            ) => {}
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } if matches!(
            inner.as_ref(),
            Expr::Value(value) if matches!(&value.value, Value::Number(_, _))
        ) => {}
        _ => return None,
    }
    Some(())
}

/// Answers whether a JSON path names one member of the top-level object.
fn names_one_member(path: &str) -> bool {
    let Some(name) = path.strip_prefix("$.") else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Returns the column a JSON reading names: the column itself, or the one a
/// `JSON_EXTRACT` or an arrow reads a path out of.
fn json_reading_column(expr: &Expr) -> Option<String> {
    if let Expr::Identifier(column) = expr {
        return Some(column.value.clone());
    }
    let Some(StaticSelectMetadata::ScalarCall {
        function, columns, ..
    }) = (match expr {
        Expr::Function(function) => scalar_call(function),
        Expr::BinaryOp { .. } => classify_json_arrow(expr),
        _ => None,
    })
    else {
        return None;
    };
    // Only the reading that answers a document is taken. The unquoted one
    // answers the text inside a string, which is not a document to read again.
    (function == ScalarFunction::ReadsAJsonValue)
        .then(|| columns.first().cloned())
        .flatten()
}

/// Reports whether a path is `$` followed by plain `.member` and `[index]`
/// steps, which is the part of MySQL's path language the engine reads too.
pub fn names_a_plain_json_path(path: &str) -> bool {
    let Some(mut rest) = path.strip_prefix('$') else {
        return false;
    };
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let member = after
                .split(['.', '['])
                .next()
                .expect("splitting a string answers at least one part");
            if member.is_empty()
                || !member
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return false;
            }
            rest = &after[member.len()..];
            continue;
        }
        let Some(after) = rest.strip_prefix('[') else {
            return false;
        };
        let Some(end) = after.find(']') else {
            return false;
        };
        let index = &after[..end];
        if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        rest = &after[end + 1..];
    }
    true
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
    } else if name.value.eq_ignore_ascii_case("GROUP_CONCAT") {
        ColumnAggregateKind::Concatenated
    // Only the sample form is here. MySQL spells the population one
    // `STDDEV`, `STD` and `STDDEV_POP`, and the engine has no aggregate for
    // it, so answering one of those would mean answering a different number.
    } else if name.value.eq_ignore_ascii_case("STDDEV_SAMP") {
        ColumnAggregateKind::DeviatesBySample
    } else {
        return None;
    };
    if has_aggregate_modifiers(function) {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    // `GROUP_CONCAT` is the one aggregate here that carries either of these:
    // a `DISTINCT` before its column or a `SEPARATOR` after it. The engine
    // spells the separator as a second argument, and it takes `DISTINCT`
    // only over a single argument, so the two together are refused.
    if kind == ColumnAggregateKind::Concatenated {
        if arguments.duplicate_treatment.is_some() && !arguments.clauses.is_empty() {
            return None;
        }
        if !checked_group_concat_clauses(&arguments.clauses) {
            return None;
        }
    } else if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => Some((kind, column)),
        _ => None,
    }
}

/// Reports whether a call is a plain `COUNT` or `COUNT(DISTINCT ...)`, which is
/// the one aggregate whose result metadata does not depend on what it counts.
///
/// Measured on MySQL 8.4.11: `COUNT(*)`, `COUNT(col)`, and `COUNT(DISTINCT col)` all answer
/// a non-null `LONGLONG` of length 21, and 0 rather than NULL on an empty table. `MIN` and
/// `MAX` answer their argument's own type, and `SUM` and `AVG` answer DECIMAL,
/// so none of those belong here.
pub(super) fn is_count_call(function: &sqlparser::ast::Function) -> bool {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    if !name.value.eq_ignore_ascii_case("COUNT") || name.quote_style.is_some() {
        return false;
    }
    if has_aggregate_modifiers(function) {
        return false;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return false;
    };
    if !arguments.clauses.is_empty() {
        return false;
    }
    match arguments.duplicate_treatment {
        None => matches!(
            arguments.args.as_slice(),
            [sqlparser::ast::FunctionArg::Unnamed(
                sqlparser::ast::FunctionArgExpr::Wildcard
                    | sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(_)),
            )]
        ),
        Some(sqlparser::ast::DuplicateTreatment::Distinct) => matches!(
            arguments.args.as_slice(),
            [sqlparser::ast::FunctionArg::Unnamed(
                sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(_)),
            )]
        ),
        _ => false,
    }
}

/// Reports whether a call is the bare aggregate form and nothing more.
///
/// Anything past it — DISTINCT, an OVER clause, a filter — has its own meaning
/// that this module does not model.
/// Reads the unit an interval names, and whether it counts whole days.
///
/// The engine spells each unit as a modifier of its own — `'+1 days'` — and
/// takes only these six. Anything else, a fractional-second precision
/// included, is left out.
pub(super) fn checked_interval_unit(
    interval: &sqlparser::ast::Interval,
) -> Option<(&'static str, bool)> {
    use sqlparser::ast::DateTimeField;
    if interval.leading_precision.is_some()
        || interval.last_field.is_some()
        || interval.fractional_seconds_precision.is_some()
    {
        return None;
    }
    match interval.leading_field.as_ref()? {
        DateTimeField::Year => Some(("years", true)),
        DateTimeField::Month => Some(("months", true)),
        DateTimeField::Day => Some(("days", true)),
        DateTimeField::Hour => Some(("hours", false)),
        DateTimeField::Minute => Some(("minutes", false)),
        DateTimeField::Second => Some(("seconds", false)),
        _ => None,
    }
}

/// Reports whether a `GROUP_CONCAT` carries nothing but a separator.
///
/// Its `ORDER BY` is left out: MySQL orders the parts it joins, and the
/// engine's `group_concat` has no way to say in what order it joins them.
pub(super) fn checked_group_concat_clauses(
    clauses: &[sqlparser::ast::FunctionArgumentClause],
) -> bool {
    matches!(
        clauses,
        [] | [sqlparser::ast::FunctionArgumentClause::Separator(
            sqlparser::ast::ValueWithSpan {
                value: sqlparser::ast::Value::SingleQuotedString(_),
                ..
            },
        )]
    )
}

/// Returns the separator a checked `GROUP_CONCAT` was given, if any.
pub(super) fn group_concat_separator(function: &sqlparser::ast::Function) -> Option<&str> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    match arguments.clauses.as_slice() {
        [sqlparser::ast::FunctionArgumentClause::Separator(sqlparser::ast::ValueWithSpan {
            value: sqlparser::ast::Value::SingleQuotedString(separator),
            ..
        })] => Some(separator),
        _ => None,
    }
}

fn is_plain_aggregate(function: &sqlparser::ast::Function) -> bool {
    if has_aggregate_modifiers(function) {
        return false;
    }
    match &function.args {
        // `CURRENT_DATE` and `CURRENT_TIMESTAMP` are spelled without
        // parentheses, and sqlparser gives those no argument list at all.
        sqlparser::ast::FunctionArguments::None => true,
        sqlparser::ast::FunctionArguments::List(arguments) => {
            arguments.duplicate_treatment.is_none() && arguments.clauses.is_empty()
        }
        sqlparser::ast::FunctionArguments::Subquery(_) => false,
    }
}

fn has_aggregate_modifiers(function: &sqlparser::ast::Function) -> bool {
    function.filter.is_some()
        || function.over.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.uses_odbc_syntax
        || function.parameters != sqlparser::ast::FunctionArguments::None
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
