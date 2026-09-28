//! `WITH RECURSIVE` over a counted sequence — `WITH RECURSIVE n(x) AS (SELECT
//! 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n` — which is
//! how a statement asks for a run of numbers.
//!
//! The engine runs a recursive CTE the way SQLite does, and answers the rows
//! MySQL answers for one, but it has no limit on how deep the recursion goes.
//! Measured on MySQL 8.4.11, MySQL answers 3636 once a recursion runs past
//! `cte_max_recursion_depth`, 1000 by default — a body producing 999 rows
//! past its first is answered and one producing 1000 is not — and 1690 once a
//! value runs past a `BIGINT`. So only a sequence whose length and values can
//! be worked out from the statement itself is taken: every column starts at a
//! written whole number and steps by one, one column steps up by a written
//! positive number, and the recursion stops at a written bound on that column.
//! A body whose depth depends on the rows it reads — walking a tree of
//! parents — would loop for ever in the engine over rows MySQL answers 3636
//! for, and is refused.

use super::*;

/// A recursive CTE counting through a sequence of whole numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CountedSequence {
    name: Ident,
    columns: Vec<SequenceColumn>,
    keeps_repeated_rows: bool,
    /// The column the recursion stops on, and the bound that stops it.
    bounded: usize,
    bound: (BinaryOperator, i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SequenceColumn {
    name: Ident,
    first: i64,
    step: (BinaryOperator, i64),
    /// How many characters MySQL reports the column as: measured on 8.4.11,
    /// the digits its first value is written with, and one more — `1` reports
    /// 2, `10` reports 3, and `-1` reports 2 as well.
    length: u32,
}

/// Measured on MySQL 8.4.11: a body producing 999 rows past its first is
/// answered, and one producing 1000 answers 3636.
const MOST_ROWS_PAST_THE_FIRST: i64 = 999;

/// Reads a `WITH RECURSIVE` clause that counts through a sequence.
pub(super) fn read_counted_sequence(
    with: &sqlparser::ast::With,
) -> Result<CountedSequence, ParseError> {
    let [cte] = with.cte_tables.as_slice() else {
        return unsupported("WITH RECURSIVE naming more than one table");
    };
    if cte.from.is_some() || cte.materialized.is_some() || cte.alias.at.is_some() {
        return unsupported("WITH RECURSIVE option");
    }
    let query = cte.query.as_ref();
    if query.with.is_some()
        || query.order_by.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
    {
        return unsupported("WITH RECURSIVE body clause");
    }
    let SetExpr::SetOperation {
        left,
        op: sqlparser::ast::SetOperator::Union,
        set_quantifier,
        right,
    } = query.body.as_ref()
    else {
        return unsupported("WITH RECURSIVE body that is not a UNION");
    };
    let keeps_repeated_rows = match set_quantifier {
        sqlparser::ast::SetQuantifier::All => true,
        sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct => false,
        _ => return unsupported("WITH RECURSIVE union option"),
    };
    let (SetExpr::Select(first), SetExpr::Select(next)) = (left.as_ref(), right.as_ref()) else {
        return unsupported("WITH RECURSIVE branch");
    };
    let firsts = first_values(first)?;
    let names = match cte.alias.columns.as_slice() {
        [] => first
            .projection
            .iter()
            .map(|item| match item {
                SelectItem::ExprWithAlias { alias, .. } => Ok(alias.clone()),
                _ => unsupported("WITH RECURSIVE column without a name"),
            })
            .collect::<Result<Vec<_>, _>>()?,
        columns => columns
            .iter()
            .map(|column| {
                if column.data_type.is_some() {
                    return unsupported("WITH RECURSIVE column type");
                }
                Ok(column.name.clone())
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    if names.len() != firsts.len() {
        return unsupported("WITH RECURSIVE naming a different number of columns");
    }
    let steps = next_steps(next, &cte.alias.name, &names)?;
    let Some(selection) = &next.selection else {
        return unsupported("WITH RECURSIVE body without a bound");
    };
    let (driver, bound) = bound_of(selection, &names)?;
    let (BinaryOperator::Plus, step) = &steps[driver] else {
        return unsupported("WITH RECURSIVE bound on a column that does not count up");
    };
    let counted = rows_past_the_first(firsts[driver], *step, &bound)
        .filter(|counted| *counted <= MOST_ROWS_PAST_THE_FIRST)
        .ok_or(ParseError::Unsupported {
            feature: "WITH RECURSIVE past the depth MySQL answers 3636 for",
        })?;
    let mut columns = Vec::with_capacity(names.len());
    for ((name, first), step) in names.into_iter().zip(firsts).zip(steps) {
        if !stays_a_bigint(first, &step, counted) {
            return unsupported(
                "WITH RECURSIVE past the largest BIGINT, which MySQL answers 1690 for",
            );
        }
        columns.push(SequenceColumn {
            name,
            first,
            step,
            length: first.unsigned_abs().to_string().len() as u32 + 1,
        });
    }
    Ok(CountedSequence {
        name: cte.alias.name.clone(),
        columns,
        keeps_repeated_rows,
        bounded: driver,
        bound,
    })
}

impl CountedSequence {
    /// Renders the clause the engine runs.
    pub(super) fn render(&self) -> String {
        let driver = &self.columns[self.bounded].name;
        format!(
            "WITH RECURSIVE {}({}) AS (SELECT {} UNION{} SELECT {} FROM {} WHERE {} {} {}) ",
            render_ident(&self.name),
            self.columns
                .iter()
                .map(|column| render_ident(&column.name))
                .collect::<Vec<_>>()
                .join(", "),
            self.columns
                .iter()
                .map(|column| column.first.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if self.keeps_repeated_rows { " ALL" } else { "" },
            self.columns
                .iter()
                .map(|column| {
                    let (operator, by) = &column.step;
                    format!("{} {operator} {by}", render_ident(&column.name))
                })
                .collect::<Vec<_>>()
                .join(", "),
            render_ident(&self.name),
            render_ident(driver),
            self.bound.0,
            self.bound.1,
        )
    }

    /// Reports whether the sequence has a column by this name.
    pub(super) fn has_column(&self, name: &str) -> bool {
        self.columns
            .iter()
            .any(|column| column.name.value.eq_ignore_ascii_case(name))
    }

    /// Answers the static shape of each of the sequence's columns, in order,
    /// as the statement reading it under `reference` reports them.
    pub(super) fn column_answers<'a>(
        &'a self,
        reference: &'a str,
    ) -> impl Iterator<Item = (&'a str, StaticSelectMetadata)> + 'a {
        self.columns.iter().map(move |column| {
            (
                column.name.value.as_str(),
                StaticSelectMetadata::CountedColumn {
                    table: reference.to_owned(),
                    column: column.name.value.clone(),
                    length: column.length,
                },
            )
        })
    }
}

/// Holds the statement reading a counted sequence to reading it alone, and
/// takes the sequence out of the tables the statement reads.
///
/// The sequence reads no table, so there is nothing to authorize or to look a
/// column's type up in. Each of its columns holds a whole number, which is
/// what a comparison on one is held to.
pub(super) fn read_the_sequence(
    sequence: &CountedSequence,
    sources: &mut Vec<MySqlSelectSource>,
    comparisons: &mut [CheckedSelectComparison],
) -> Result<(), ParseError> {
    let names_the_sequence = |source: &MySqlSelectSource| {
        source
            .table
            .as_str()
            .eq_ignore_ascii_case(&sequence.name.value)
    };
    let [source] = sources.as_slice() else {
        return unsupported("WITH RECURSIVE read beside another table");
    };
    if !names_the_sequence(source) || source.subquery || source.derived.is_some() {
        return unsupported("WITH RECURSIVE sequence the statement does not read");
    }
    let reference = source.reference.clone();
    for comparison in comparisons {
        if !comparison.inner_sources.is_empty() || comparison.answers.is_some() {
            continue;
        }
        if comparison
            .qualifier
            .as_ref()
            .is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(&reference))
            || !sequence.has_column(&comparison.column_name)
        {
            return unsupported("SELECT comparison names no column of the sequence");
        }
        comparison.answers = Some(CheckedComparisonAnswer::WholeNumber);
    }
    sources.clear();
    Ok(())
}

/// Answers the static shape of each result column of a statement reading a
/// counted sequence, or nothing for any other statement.
///
/// A column of the sequence, `*` included, reports the sequence's own shape;
/// anything else is classified the way any projection is.
pub(super) fn counted_projection(
    query: &sqlparser::ast::Query,
    select: &sqlparser::ast::Select,
) -> Option<Vec<StaticSelectProjectionMetadata>> {
    let with = query.with.as_ref().filter(|with| with.recursive)?;
    let sequence = read_counted_sequence(with).ok()?;
    let [source] = select.from.as_slice() else {
        return None;
    };
    let TableFactor::Table { name, alias, .. } = &source.relation else {
        return None;
    };
    if !matches!(name.0.as_slice(), [ObjectNamePart::Identifier(name)] if name.value.eq_ignore_ascii_case(&sequence.name.value))
    {
        return None;
    }
    let reference = alias
        .as_ref()
        .map_or(sequence.name.value.as_str(), |alias| {
            alias.name.value.as_str()
        });
    let mut projections = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        let named = match item {
            SelectItem::Wildcard(_) => {
                projections.extend(
                    sequence
                        .column_answers(reference)
                        .map(|(_, answer)| StaticSelectProjectionMetadata::Literal(answer)),
                );
                continue;
            }
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => match expr {
                Expr::Identifier(column) => Some(column),
                Expr::CompoundIdentifier(parts)
                    if parts.len() == 2 && parts[0].value.eq_ignore_ascii_case(reference) =>
                {
                    Some(&parts[1])
                }
                _ => None,
            },
            _ => None,
        };
        let answer = named.and_then(|named| {
            sequence
                .column_answers(reference)
                .find(|(column, _)| column.eq_ignore_ascii_case(&named.value))
                .map(|(_, answer)| answer)
        });
        projections.push(match (answer, item) {
            (Some(answer), _) => StaticSelectProjectionMetadata::Literal(answer),
            (None, SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) => {
                static_select_metadata::classify_static_select_expr(expr).map_or(
                    StaticSelectProjectionMetadata::Other,
                    StaticSelectProjectionMetadata::Literal,
                )
            }
            (None, _) => StaticSelectProjectionMetadata::Other,
        });
    }
    Some(projections)
}

/// Reads the written whole numbers a sequence starts at.
fn first_values(first: &sqlparser::ast::Select) -> Result<Vec<i64>, ParseError> {
    if !first.from.is_empty()
        || first.selection.is_some()
        || first.having.is_some()
        || first.distinct.is_some()
        || !matches!(&first.group_by, sqlparser::ast::GroupByExpr::Expressions(keys, _) if keys.is_empty())
    {
        return unsupported("WITH RECURSIVE first row that reads a table");
    }
    first
        .projection
        .iter()
        .map(|item| {
            let expr = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                _ => return unsupported("WITH RECURSIVE first row"),
            };
            direct_signed_integer(expr).ok_or(ParseError::Unsupported {
                feature: "WITH RECURSIVE first value that is not a written whole number",
            })
        })
        .collect()
}

/// Reads how each column steps from one row to the next: by adding,
/// subtracting or multiplying a written whole number.
fn next_steps(
    next: &sqlparser::ast::Select,
    name: &Ident,
    columns: &[Ident],
) -> Result<Vec<(BinaryOperator, i64)>, ParseError> {
    let [source] = next.from.as_slice() else {
        return unsupported("WITH RECURSIVE step reading another table");
    };
    let TableFactor::Table {
        name: read, alias, ..
    } = &source.relation
    else {
        return unsupported("WITH RECURSIVE step reading another table");
    };
    if !source.joins.is_empty()
        || alias.is_some()
        || !matches!(read.0.as_slice(), [ObjectNamePart::Identifier(read)] if read.value.eq_ignore_ascii_case(&name.value))
        || next.having.is_some()
        || next.distinct.is_some()
        || !matches!(&next.group_by, sqlparser::ast::GroupByExpr::Expressions(keys, _) if keys.is_empty())
    {
        return unsupported("WITH RECURSIVE step");
    }
    if next.projection.len() != columns.len() {
        return unsupported("WITH RECURSIVE step projecting a different number of columns");
    }
    next.projection
        .iter()
        .zip(columns)
        .map(|(item, column)| {
            let SelectItem::UnnamedExpr(Expr::BinaryOp { left, op, right }) = item else {
                return unsupported("WITH RECURSIVE step that is not arithmetic");
            };
            let Expr::Identifier(stepped) = left.as_ref() else {
                return unsupported("WITH RECURSIVE step");
            };
            if !stepped.value.eq_ignore_ascii_case(&column.value)
                || !matches!(
                    op,
                    BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply
                )
            {
                return unsupported("WITH RECURSIVE step on another column");
            }
            let by = direct_signed_integer(right).ok_or(ParseError::Unsupported {
                feature: "WITH RECURSIVE step by a number that is not written out",
            })?;
            Ok((op.clone(), by))
        })
        .collect()
}

/// Reads the bound that stops the recursion, and which column it bounds.
fn bound_of(
    selection: &Expr,
    columns: &[Ident],
) -> Result<(usize, (BinaryOperator, i64)), ParseError> {
    let Expr::BinaryOp { left, op, right } = selection else {
        return unsupported("WITH RECURSIVE bound");
    };
    let Expr::Identifier(bounded) = left.as_ref() else {
        return unsupported("WITH RECURSIVE bound");
    };
    if !matches!(op, BinaryOperator::Lt | BinaryOperator::LtEq) {
        return unsupported("WITH RECURSIVE bound");
    }
    let bound = direct_signed_integer(right).ok_or(ParseError::Unsupported {
        feature: "WITH RECURSIVE bound that is not a written whole number",
    })?;
    let driver = columns
        .iter()
        .position(|column| column.value.eq_ignore_ascii_case(&bounded.value))
        .ok_or(ParseError::Unsupported {
            feature: "WITH RECURSIVE bound on a column the sequence does not have",
        })?;
    Ok((driver, (op.clone(), bound)))
}

/// Counts the rows the recursion produces past its first: one for each row
/// whose bounded column still meets the bound.
fn rows_past_the_first(first: i64, step: i64, bound: &(BinaryOperator, i64)) -> Option<i64> {
    if step <= 0 {
        return None;
    }
    let (operator, bound) = bound;
    let room = i128::from(*bound) - i128::from(first);
    let step = i128::from(step);
    let rows = match operator {
        BinaryOperator::Lt if room > 0 => (room + step - 1) / step,
        BinaryOperator::LtEq if room >= 0 => room / step + 1,
        BinaryOperator::Lt | BinaryOperator::LtEq => 0,
        _ => return None,
    };
    i64::try_from(rows).ok()
}

/// Reports whether every value a column steps through fits in a `BIGINT`.
fn stays_a_bigint(first: i64, (operator, by): &(BinaryOperator, i64), rows: i64) -> bool {
    let mut value = first;
    for _ in 0..rows {
        let next = match operator {
            BinaryOperator::Plus => value.checked_add(*by),
            BinaryOperator::Minus => value.checked_sub(*by),
            BinaryOperator::Multiply => value.checked_mul(*by),
            _ => None,
        };
        let Some(next) = next else {
            return false;
        };
        value = next;
    }
    true
}
