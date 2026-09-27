//! Conditions over a `JSON` column in a `WHERE`.
//!
//! Every framework writes these for a JSON column: Laravel's `where('meta->lang',
//! 'en')` is `json_unquote(json_extract(meta, '$."lang"')) = ?`, its
//! `whereJsonContains` a bare `json_contains(meta, ?, '$."tags"')`, and Rails
//! writes `meta->>'$.lang' = 'en'` by hand. Each reading here goes through the
//! dialect rather than the engine's own `->` and `->>`, which read a path
//! differently from MySQL: measured on 8.4.11, `$[0]` over an object is the
//! object, and the JSON null unquotes to the word `null`.
//!
//! A reading names its column, and only the frontend can see whether that
//! column is a `JSON` one, so every column read is recorded for it to check.

use super::*;

/// Renders a comparison with a JSON reading on one side, or nothing when
/// neither side is one.
pub(super) fn render_comparison_over_a_json_reading(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    let (answer, op, other) = match (answered_by_json(left), answered_by_json(right)) {
        (Some(answer), _) => (answer, op.clone(), right),
        (None, Some(answer)) => {
            let Some(reversed) = reverse_checked_comparison_operator(op) else {
                return unsupported("JSON comparison operator");
            };
            (answer, reversed, left)
        }
        (None, None) => return Ok(None),
    };
    let operator = checked_select_comparison_operator(&op)
        .expect("the caller checked the comparison operator");
    let rendered = match answer {
        JsonAnswer::Text(reading) => {
            let rendered = reading.render(render_context);
            render_text_comparison(&rendered, &op, operator, other, render_context)?
        }
        JsonAnswer::Kind(reading) => {
            let rendered = format!("mysql_json_type({})", reading.render(render_context));
            render_text_comparison(&rendered, &op, operator, other, render_context)?
        }
        JsonAnswer::Count(reading) => {
            let rendered = format!("mysql_json_length({})", reading.render(render_context));
            render_count_comparison(&rendered, &op, operator, other, render_context)?
        }
        JsonAnswer::Document(reading) => {
            let rendered = reading.render(render_context);
            render_document_comparison(&rendered, &op, operator, other, render_context)?
        }
    };
    Ok(Some(rendered))
}

/// Renders a test of a JSON document standing on its own as a condition, or
/// nothing when the expression is not one.
///
/// `json_contains(meta, ?, '$."tags"')`, `json_contains_path(meta, 'one',
/// '$.a')` and the `ifnull(..., 0)` Laravel writes around the second are the
/// shapes this is for. Each answers 1, 0 or no value, which is what the engine
/// reads a condition as.
pub(super) fn render_json_test(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if let Some([tested, zero]) = plain_call(expr, "IFNULL").as_deref() {
        let is_zero = matches!(zero, Expr::Value(value)
            if matches!(&value.value, Value::Number(digits, false) if digits == "0"));
        if !is_zero {
            return unsupported("IFNULL over a JSON test with something other than 0");
        }
        return match render_contains_path(tested, render_context)? {
            Some(rendered) => Ok(format!("ifnull({rendered}, 0)")),
            None => unsupported("IFNULL over a JSON call other than JSON_CONTAINS_PATH"),
        };
    }
    if let Some(rendered) = render_contains_path(expr, render_context)? {
        return Ok(rendered);
    }
    match render_contains(expr, render_context)? {
        Some(rendered) => Ok(rendered),
        None => unsupported("JSON call standing on its own as a condition"),
    }
}

/// Renders `reading IS NULL` or `reading IS NOT NULL` over a JSON reading, or
/// nothing when the expression is not one.
///
/// Measured on MySQL 8.4.11: a member holding the JSON null is found, so
/// `json_extract(doc, '$.a') IS NULL` is false for it and true only where the
/// member is not there — and unquoted, it is the word `null`, which is no more
/// NULL.
pub(super) fn render_json_null_test(
    expr: &Expr,
    negated: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let Some(reading) = read_json_reading(expr) else {
        return unsupported("IS NULL over a JSON call other than a reading");
    };
    let rendered = reading.render(render_context);
    Ok(format!(
        "({rendered} IS {}NULL)",
        if negated { "NOT " } else { "" }
    ))
}

/// Reports whether an expression reads a JSON column, so a caller that would
/// read it some other way knows to leave it to this file.
pub(super) fn reads_a_json_column(expr: &Expr) -> bool {
    answered_by_json(expr).is_some()
        || plain_call(expr, "JSON_CONTAINS").is_some()
        || plain_call(expr, "JSON_CONTAINS_PATH").is_some()
        || matches!(plain_call(expr, "IFNULL").as_deref(), Some([tested, _]) if reads_a_json_column(tested))
}

/// What one side of a comparison over a JSON column answers.
enum JsonAnswer<'e> {
    /// `->>` or `JSON_UNQUOTE(JSON_EXTRACT(...))`: text under `utf8mb4_bin`.
    Text(JsonReading<'e>),
    /// `JSON_TYPE(...)`: the word naming the kind, under `utf8mb4_bin` too.
    Kind(JsonReading<'e>),
    /// `JSON_LENGTH(...)`: a count.
    Count(JsonReading<'e>),
    /// `->` or `JSON_EXTRACT(...)`: a JSON value, compared by JSON's rules.
    Document(JsonReading<'e>),
}

fn answered_by_json(expr: &Expr) -> Option<JsonAnswer<'_>> {
    if let Some(reading) = read_json_reading(expr) {
        return Some(if reading.unquoted {
            JsonAnswer::Text(reading)
        } else {
            JsonAnswer::Document(reading)
        });
    }
    if let Some([read]) = plain_call(expr, "JSON_TYPE").as_deref() {
        return read_document_reading(read).map(JsonAnswer::Kind);
    }
    match plain_call(expr, "JSON_LENGTH").as_deref() {
        Some([read]) => read_document_reading(read).map(JsonAnswer::Count),
        Some([Expr::Identifier(column), path]) => {
            let path = written_json_path(path)?;
            Some(JsonAnswer::Count(JsonReading {
                column,
                path: Some(path),
                unquoted: false,
            }))
        }
        _ => None,
    }
}

/// One path read out of a JSON column, or the whole column.
struct JsonReading<'e> {
    column: &'e Ident,
    path: Option<&'e str>,
    unquoted: bool,
}

impl JsonReading<'_> {
    fn render(&self, render_context: &mut SelectRenderContext<'_>) -> String {
        render_context
            .json_reading_columns
            .push(self.column.value.clone());
        let column = render_ident(self.column);
        let found = match self.path {
            Some(path) => format!("mysql_json_extract({column}, {})", render_text(path)),
            None => column,
        };
        if self.unquoted {
            format!("mysql_json_unquote({found})")
        } else {
            found
        }
    }
}

/// Reads `col -> 'path'`, `col ->> 'path'`, `JSON_EXTRACT(col, 'path')` and
/// `JSON_UNQUOTE(JSON_EXTRACT(col, 'path'))`.
///
/// Only one path is read: MySQL's `JSON_EXTRACT` takes several and answers an
/// array of what they found, which is a different value.
fn read_json_reading(expr: &Expr) -> Option<JsonReading<'_>> {
    match expr {
        Expr::BinaryOp {
            left,
            op: op @ (BinaryOperator::Arrow | BinaryOperator::LongArrow),
            right,
        } => {
            let Expr::Identifier(column) = left.as_ref() else {
                return None;
            };
            Some(JsonReading {
                column,
                path: Some(written_json_path(right)?),
                unquoted: matches!(op, BinaryOperator::LongArrow),
            })
        }
        Expr::Nested(inner) => read_json_reading(inner),
        _ => {
            if let Some([Expr::Identifier(column), path]) =
                plain_call(expr, "JSON_EXTRACT").as_deref()
            {
                return Some(JsonReading {
                    column,
                    path: Some(written_json_path(path)?),
                    unquoted: false,
                });
            }
            let unquoted = plain_call(expr, "JSON_UNQUOTE")?;
            let [extracted] = unquoted.as_slice() else {
                return None;
            };
            let extracted = plain_call(extracted, "JSON_EXTRACT")?;
            let [Expr::Identifier(column), path] = extracted.as_slice() else {
                return None;
            };
            Some(JsonReading {
                column,
                path: Some(written_json_path(path)?),
                unquoted: true,
            })
        }
    }
}

/// Reads a JSON column itself or a JSON value read out of one, which is what
/// `JSON_TYPE` and `JSON_LENGTH` take: text a reading unquoted is not a
/// document, and MySQL would read it as one again.
fn read_document_reading(expr: &Expr) -> Option<JsonReading<'_>> {
    if let Expr::Identifier(column) = expr {
        return Some(JsonReading {
            column,
            path: None,
            unquoted: false,
        });
    }
    read_json_reading(expr).filter(|reading| !reading.unquoted)
}

/// Reads a path written out as one this reads the way MySQL does.
fn written_json_path(expr: &Expr) -> Option<&str> {
    let Expr::Value(value) = expr else {
        return None;
    };
    let (Value::SingleQuotedString(path) | Value::DoubleQuotedString(path)) = &value.value else {
        return None;
    };
    crate::is_a_json_path_this_reads(path).then_some(path.as_str())
}

/// Returns the arguments of a plain call to the named function, each an
/// expression, or nothing when the expression is not one.
fn plain_call<'e>(expr: &'e Expr, name: &str) -> Option<Vec<&'e Expr>> {
    let Expr::Function(function) = expr else {
        return None;
    };
    let [ObjectNamePart::Identifier(called)] = function.name.0.as_slice() else {
        return None;
    };
    if called.quote_style.is_some()
        || !called.value.eq_ignore_ascii_case(name)
        || function.over.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
    {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    arguments
        .args
        .iter()
        .map(|argument| match argument {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
                Some(expr)
            }
            _ => None,
        })
        .collect()
}

/// Renders a comparison of text a JSON reading answers.
///
/// Measured on MySQL 8.4.11, the text carries `utf8mb4_bin`: against a word
/// `json_unquote(...) = 'EN'` does not find `en`, and `'en  '` does, the
/// collation padding with spaces. Against a number both sides are read as
/// doubles: `'1.50'` equals 1.5 and `'true'` equals 0. The dialect makes both
/// comparisons, choosing by what the value is — which for a `?` is only known
/// once it binds, and the frontend holds what it binds to a word or a number.
fn render_text_comparison(
    rendered: &str,
    op: &BinaryOperator,
    operator: CheckedSelectComparisonOperator,
    other: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if matches!(other, Expr::Collate { .. }) {
        return unsupported("JSON text comparison with explicit collation");
    }
    let (rendered_other, rhs) = render_checked_select_comparison_rhs(other, render_context)?;
    let rendered_other = match &rhs {
        CheckedSelectComparisonRhs::Text(_)
        | CheckedSelectComparisonRhs::SignedInteger(_)
        | CheckedSelectComparisonRhs::Placeholder { .. } => rendered_other,
        CheckedSelectComparisonRhs::Decimal(written) => format!("({written})"),
        CheckedSelectComparisonRhs::Null => {
            return Ok(render_against_null(rendered, op, operator));
        }
        CheckedSelectComparisonRhs::Now(_) => {
            return unsupported("JSON text comparison with a clock reading")
        }
        CheckedSelectComparisonRhs::Column { .. } => {
            unreachable!("the value reader never answers a column")
        }
    };
    let compared = format!("mysql_json_text_compare({rendered}, {rendered_other})");
    let condition = if operator == CheckedSelectComparisonOperator::NullSafeEqual {
        if matches!(rhs, CheckedSelectComparisonRhs::Placeholder { .. }) {
            return unsupported("JSON text comparison <=> with a bound value");
        }
        format!("coalesce({compared} = 0, 0)")
    } else {
        format!(
            "{compared} {} 0",
            checked_select_comparison_sql_operator(op)
        )
    };
    record_json_comparison(
        render_context,
        operator,
        rhs,
        crate::CheckedComparisonAnswer::JsonText,
    );
    Ok(format!("({condition})"))
}

/// Renders a comparison of the count `JSON_LENGTH` answers, which the engine
/// compares the way MySQL does against a whole number or a decimal.
///
/// A word is refused: measured on MySQL 8.4.11, `json_length(doc) = '6'` reads
/// the word as a number, where the engine would compare a count with text.
fn render_count_comparison(
    rendered: &str,
    op: &BinaryOperator,
    operator: CheckedSelectComparisonOperator,
    other: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let (rendered_other, rhs) = render_checked_select_comparison_rhs(other, render_context)?;
    let rendered_other = match &rhs {
        CheckedSelectComparisonRhs::SignedInteger(_)
        | CheckedSelectComparisonRhs::Placeholder { .. } => rendered_other,
        CheckedSelectComparisonRhs::Decimal(written) => format!("({written})"),
        CheckedSelectComparisonRhs::Null => {
            return Ok(render_against_null(rendered, op, operator));
        }
        CheckedSelectComparisonRhs::Text(_) | CheckedSelectComparisonRhs::Now(_) => {
            return unsupported("JSON_LENGTH comparison with something other than a number")
        }
        CheckedSelectComparisonRhs::Column { .. } => {
            unreachable!("the value reader never answers a column")
        }
    };
    if operator == CheckedSelectComparisonOperator::NullSafeEqual
        && matches!(rhs, CheckedSelectComparisonRhs::Placeholder { .. })
    {
        return unsupported("JSON_LENGTH comparison <=> with a bound value");
    }
    record_json_comparison(
        render_context,
        operator,
        rhs,
        crate::CheckedComparisonAnswer::JsonCount,
    );
    Ok(format!(
        "({rendered} {} {rendered_other})",
        checked_select_comparison_sql_operator(op)
    ))
}

/// Renders a comparison of a JSON value read out of a column, which MySQL
/// makes by JSON's rules — the same ones a whole `JSON` column is compared by.
///
/// Measured on MySQL 8.4.11: `doc->'$.a' = '1'` finds the JSON string `"1"`
/// and not the number 1, `doc->'$.a' = 1` the number and not the string, and a
/// word is compared byte for byte with no padding, so `'en '` does not find
/// `"en"`. `doc->'$.a' = TRUE` finds the JSON `true`, where TRUE is otherwise
/// the number 1, so a written boolean is refused, and so is a bound value,
/// whose JSON reading MySQL settles by what earlier executions bound.
fn render_document_comparison(
    rendered: &str,
    op: &BinaryOperator,
    operator: CheckedSelectComparisonOperator,
    other: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if matches!(other, Expr::Value(value) if matches!(value.value, Value::Boolean(_))) {
        return unsupported("JSON comparison with a written boolean");
    }
    if matches!(other, Expr::Collate { .. }) {
        return unsupported("JSON comparison with explicit collation");
    }
    let (rendered_other, rhs) = render_checked_select_comparison_rhs(other, render_context)?;
    let sql_operator = checked_select_comparison_sql_operator(op);
    let orders = matches!(
        operator,
        CheckedSelectComparisonOperator::LessThan
            | CheckedSelectComparisonOperator::LessThanOrEqual
            | CheckedSelectComparisonOperator::GreaterThan
            | CheckedSelectComparisonOperator::GreaterThanOrEqual
    );
    Ok(match &rhs {
        CheckedSelectComparisonRhs::Null => {
            render_against_null(rendered, op, operator)
        }
        CheckedSelectComparisonRhs::Text(_) if orders => {
            format!("(mysql_json_compare_string({rendered}, {rendered_other}) {sql_operator} 0)")
        }
        CheckedSelectComparisonRhs::Text(_) => format!(
            "(CAST({rendered} AS BLOB) {sql_operator} CAST(mysql_json_quote({rendered_other}) AS BLOB))"
        ),
        CheckedSelectComparisonRhs::SignedInteger(_) if orders => {
            format!("(mysql_json_compare_integer({rendered}, {rendered_other}) {sql_operator} 0)")
        }
        CheckedSelectComparisonRhs::SignedInteger(_) => {
            let equal = format!("mysql_json_equals_integer({rendered}, {rendered_other})");
            match operator {
                CheckedSelectComparisonOperator::Equal => format!("({equal})"),
                CheckedSelectComparisonOperator::NotEqual => format!("(NOT {equal})"),
                CheckedSelectComparisonOperator::NullSafeEqual => {
                    format!("(coalesce({equal}, 0))")
                }
                _ => unreachable!("an ordering was answered above"),
            }
        }
        _ => {
            return unsupported(
                "JSON comparison requires a written string, a signed integer or NULL",
            )
        }
    })
}

/// Renders a comparison against a written NULL, which answers no value unless
/// it is `<=>`, which asks whether the reading answered none.
fn render_against_null(
    rendered: &str,
    op: &BinaryOperator,
    operator: CheckedSelectComparisonOperator,
) -> String {
    if operator == CheckedSelectComparisonOperator::NullSafeEqual {
        return format!("({rendered} IS NULL)");
    }
    format!(
        "({rendered} {} NULL)",
        checked_select_comparison_sql_operator(op)
    )
}

/// Records what a JSON reading was compared with, so the frontend holds a
/// bound value to the kinds the comparison was rendered for.
fn record_json_comparison(
    render_context: &mut SelectRenderContext<'_>,
    operator: CheckedSelectComparisonOperator,
    rhs: CheckedSelectComparisonRhs,
    answers: crate::CheckedComparisonAnswer,
) {
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: None,
            inner_source: None,
            column_name: String::new(),
            operator,
            rhs,
            collated: false,
            answers: Some(answers),
        });
}

/// Renders `JSON_CONTAINS(col, candidate[, 'path'])`, or nothing when the
/// expression is not one.
///
/// Measured on MySQL 8.4.11: a candidate that is not a document is error
/// 3141, so a written one is refused here and a bound one refused by the
/// dialect when it binds; a path the column does not have answers no value.
fn render_contains(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    let Some(arguments) = plain_call(expr, "JSON_CONTAINS") else {
        return Ok(None);
    };
    let (column, candidate, path) = match arguments.as_slice() {
        [Expr::Identifier(column), candidate] => (column, *candidate, None),
        [Expr::Identifier(column), candidate, path] => {
            let Some(path) = written_json_path(path) else {
                return unsupported("JSON_CONTAINS path");
            };
            (column, *candidate, Some(path))
        }
        _ => return unsupported("JSON_CONTAINS over something other than a column"),
    };
    let rendered_candidate = match candidate {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(written) | Value::DoubleQuotedString(written) => {
                if crate::normalize_json(written).is_err() {
                    return unsupported("JSON_CONTAINS looking for text that is not a document");
                }
                render_text(written)
            }
            Value::Placeholder(marker) if marker == "?" => {
                let ordinal = render_context.next_parameter_ordinal()?;
                record_json_comparison(
                    render_context,
                    CheckedSelectComparisonOperator::Equal,
                    CheckedSelectComparisonRhs::Placeholder { ordinal },
                    crate::CheckedComparisonAnswer::JsonDocument,
                );
                "?".to_owned()
            }
            _ => return unsupported("JSON_CONTAINS looking for something other than a document"),
        },
        _ => return unsupported("JSON_CONTAINS looking for something other than a document"),
    };
    let target = JsonReading {
        column,
        path,
        unquoted: false,
    }
    .render(render_context);
    Ok(Some(format!(
        "mysql_json_holds({target}, {rendered_candidate})"
    )))
}

/// Renders `JSON_CONTAINS_PATH(col, 'one' | 'all', 'path', ...)`, or nothing
/// when the expression is not one.
///
/// Measured on MySQL 8.4.11: a member holding the JSON null is there, the
/// keyword is read without regard to case, and a NULL column answers no value.
fn render_contains_path(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    let Some(arguments) = plain_call(expr, "JSON_CONTAINS_PATH") else {
        return Ok(None);
    };
    let [Expr::Identifier(column), keyword, paths @ ..] = arguments.as_slice() else {
        return unsupported("JSON_CONTAINS_PATH over something other than a column");
    };
    let every = match keyword {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)
                if word.eq_ignore_ascii_case("one") =>
            {
                false
            }
            Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)
                if word.eq_ignore_ascii_case("all") =>
            {
                true
            }
            _ => return unsupported("JSON_CONTAINS_PATH keyword"),
        },
        _ => return unsupported("JSON_CONTAINS_PATH keyword"),
    };
    if paths.is_empty() {
        return unsupported("JSON_CONTAINS_PATH without a path");
    }
    let mut found = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(path) = written_json_path(path) else {
            return unsupported("JSON_CONTAINS_PATH path");
        };
        let reading = JsonReading {
            column,
            path: Some(path),
            unquoted: false,
        }
        .render(render_context);
        found.push(format!("{reading} IS NOT NULL"));
    }
    let joined = found.join(if every { " AND " } else { " OR " });
    Ok(Some(format!(
        "(CASE WHEN {} IS NULL THEN NULL ELSE CAST(({joined}) AS INTEGER) END)",
        render_ident(column)
    )))
}

fn render_text(written: &str) -> String {
    format!("'{}'", written.replace('\'', "''"))
}
