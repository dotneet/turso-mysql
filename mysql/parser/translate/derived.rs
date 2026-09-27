//! What a derived table or a CTE projects, which is what its columns answer
//! for the statement reading it.
//!
//! A result column reaching the frontend names the derived table and an
//! ordinal, and the only way to answer what it is is to read that ordinal
//! from the body: a column of the body's table, under its own name or an
//! alias, or an answer the body worked out — a count, a total, a day.
//!
//! Measured on MySQL 8.4.11, MySQL reads a body two ways. One that aggregates
//! is written out into a table of its own first, and each column is that
//! table's column; any other body is read straight through. Which of the two
//! a body gets decides the shapes its columns report, so it is recorded here.

use super::*;

/// The columns a derived table or a CTE projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlDerivedColumns {
    /// The name the body reads its table under — its alias when it gives
    /// one — which MySQL reports as each column's original table.
    inner_reference: String,
    /// Whether MySQL writes the body out into a table of its own before the
    /// statement reads it, which it does for a body that aggregates.
    materialized: bool,
    /// The name each projected column goes by, in order. Empty for a body
    /// projecting `*`, whose columns are its table's own in their own order.
    names: Vec<String>,
    /// What each projected column answers when it is not one of the table's
    /// columns.
    answers: Vec<Option<StaticSelectMetadata>>,
}

impl MySqlDerivedColumns {
    /// Returns the name the body reads its table under.
    pub fn inner_reference(&self) -> &str {
        &self.inner_reference
    }

    /// Reports whether MySQL writes the body out into a table of its own.
    pub const fn materialized(&self) -> bool {
        self.materialized
    }

    /// Returns the name each projected column goes by, in order, or nothing
    /// for a body projecting its whole table.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Returns what the column at `ordinal` answers, when the body worked it
    /// out rather than reading it from its table.
    pub fn answer(&self, ordinal: usize) -> Option<&StaticSelectMetadata> {
        self.answers.get(ordinal).and_then(Option::as_ref)
    }
}

/// Reads what a derived table's or a CTE's body projects.
///
/// Returns the table column behind each projected column — or the column's
/// own name where the body worked it out — and what the columns are. A body
/// projecting `*` projects its table's columns in their own order, so it
/// names none. Anything whose MySQL shape has not been measured is refused:
/// an answer worked out in a body that does not aggregate, which MySQL reads
/// straight through, a call no temporary table has been measured storing, a
/// `DISTINCT`, and two columns going by one name, which MySQL answers 1060
/// for.
pub(super) fn derived_columns(
    query: &sqlparser::ast::Query,
    inner: &MySqlSelectSource,
    render_context: &SelectRenderContext<'_>,
) -> Result<(Vec<String>, MySqlDerivedColumns), ParseError> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("derived table body");
    };
    if select.distinct.is_some() {
        return unsupported("derived table body with DISTINCT");
    }
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return unsupported("derived table body grouping");
    };
    let materialized =
        projects_an_aggregate(select) || !group_by.is_empty() || select.having.is_some();
    let inner_reference = inner.reference.clone();
    if let [SelectItem::Wildcard(options)] = select.projection.as_slice() {
        if wildcard_options_are_empty(options) && !materialized {
            return Ok((
                Vec::new(),
                MySqlDerivedColumns {
                    inner_reference,
                    materialized,
                    names: Vec::new(),
                    answers: Vec::new(),
                },
            ));
        }
    }
    let mut projected_columns = Vec::with_capacity(select.projection.len());
    let mut names = Vec::with_capacity(select.projection.len());
    let mut answers = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
            _ => return unsupported("derived table projection"),
        };
        let column = match expr {
            Expr::Identifier(column) => Some(column),
            Expr::CompoundIdentifier(parts)
                if parts.len() == 2 && parts[0].value.eq_ignore_ascii_case(&inner_reference) =>
            {
                Some(&parts[1])
            }
            _ => None,
        };
        if let Some(column) = column {
            projected_columns.push(column.value.clone());
            names.push(alias.unwrap_or(column).value.clone());
            answers.push(None);
            continue;
        }
        if !materialized {
            return unsupported("derived table working out a column without aggregating");
        }
        let answer = static_select_metadata::classify_static_select_expr(expr)
            .filter(is_stored_in_a_derived_table)
            .ok_or(ParseError::Unsupported {
                feature: "derived table projection",
            })?;
        // MySQL names an unaliased answer after the expression as written,
        // which is the name the body's own rendering gives it too.
        let name = match alias {
            Some(alias) => alias.value.clone(),
            None => source_text(render_context.source, expr).ok_or(ParseError::Unsupported {
                feature: "derived table column whose source text cannot be recovered",
            })?,
        };
        projected_columns.push(name.clone());
        names.push(name);
        answers.push(Some(answer));
    }
    for (index, name) in names.iter().enumerate() {
        if names[..index]
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(name))
        {
            return unsupported("derived table with two columns of one name");
        }
    }
    Ok((
        projected_columns,
        MySqlDerivedColumns {
            inner_reference,
            materialized,
            names,
            answers,
        },
    ))
}

/// Reports whether an answer is one MySQL has been measured storing in the
/// table it writes an aggregating body out into.
fn is_stored_in_a_derived_table(answer: &StaticSelectMetadata) -> bool {
    matches!(
        answer,
        StaticSelectMetadata::Count
            | StaticSelectMetadata::ColumnAggregate {
                kind: ColumnAggregateKind::MinMax
                    | ColumnAggregateKind::Sum
                    | ColumnAggregateKind::Avg,
                ..
            }
            | StaticSelectMetadata::ScalarCall {
                function: ScalarFunction::CastsToDay,
                ..
            }
    )
}

/// Holds each comparison the statement makes on a derived table's column to
/// what that column is.
///
/// A comparison names the column by the name the derived table gives it,
/// which the frontend cannot find in the table. A column of the table is named
/// by its own name instead, so its declared type holds the value. A count
/// answers a whole number. A largest or smallest answers its column's own
/// kind, so it is held to that column. Any other answer has not been measured
/// against a value and is refused.
pub(super) fn resolve_comparisons_through_derived_columns(
    comparisons: &mut [CheckedSelectComparison],
    sources: &[MySqlSelectSource],
) -> Result<(), ParseError> {
    let readable = sources
        .iter()
        .filter(|source| !source.subquery && source.branch == 0)
        .collect::<Vec<_>>();
    for comparison in comparisons {
        if comparison.inner_source.is_some() || comparison.answers.is_some() {
            continue;
        }
        let source = match &comparison.qualifier {
            Some(qualifier) => readable
                .iter()
                .find(|source| source.reference.eq_ignore_ascii_case(qualifier)),
            None => match readable.as_slice() {
                [source] => Some(source),
                _ => None,
            },
        };
        let Some(derived) = source.and_then(|source| source.derived.as_ref()) else {
            continue;
        };
        if derived.names.is_empty() {
            continue;
        }
        let Some(ordinal) = derived
            .names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(&comparison.column_name))
        else {
            return unsupported("SELECT comparison names no column of the derived table");
        };
        match derived.answer(ordinal) {
            None => {
                comparison.column_name = source
                    .expect("the derived table was found above")
                    .projected_columns[ordinal]
                    .clone();
            }
            Some(StaticSelectMetadata::Count) => {
                comparison.answers = Some(CheckedComparisonAnswer::WholeNumber);
            }
            Some(StaticSelectMetadata::ColumnAggregate {
                kind: ColumnAggregateKind::MinMax,
                column_name,
            }) => {
                comparison.column_name = column_name.clone();
            }
            Some(_) => {
                return unsupported("SELECT comparison against a total, an average or a call");
            }
        }
    }
    Ok(())
}
