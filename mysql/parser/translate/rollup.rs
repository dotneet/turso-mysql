//! `GROUP BY ... WITH ROLLUP`, which answers a total for each group, a total
//! for each group of the keys before the last, and so on up to one for every
//! row — each super total with the keys it rolls up answered as NULL.
//!
//! The engine has no `ROLLUP`, so the statement is written out as what it
//! means: one grouped `SELECT` for each level, the rolled-up keys answered as
//! NULL, joined with `UNION ALL`. Measured on MySQL 8.4.11, the rows come back
//! sorted by the keys in the order the `GROUP BY` names them, NULL first, with
//! each super total after the groups it totals and the grand total last — so
//! each level carries, for each key, a flag saying whether it rolled that key
//! up and the key itself, and the rows are ordered by those pairs in turn. A
//! group whose key is NULL sorts first among the groups and a super total
//! after them, which is how the two NULLs are told apart.

use super::*;

/// The most keys taken, each of which multiplies the statement out once more.
const MOST_KEYS: usize = 3;

/// Renders a `SELECT` grouping `WITH ROLLUP`, or nothing for any other.
///
/// Taken: one to three keys, each a whole column; a projection of keys, each
/// at most once, and aggregates; a `HAVING` over aggregates alone. Refused:
/// an `ORDER BY`, which MySQL answers with shapes of its own; `GROUPING()`; a
/// `?`, which each level would bind again; and anything else a grouped
/// statement refuses.
pub(super) fn render_rollup(
    select: &sqlparser::ast::Select,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<(String, Vec<MySqlSelectSource>)>, ParseError> {
    let sqlparser::ast::GroupByExpr::Expressions(keys, modifiers) = &select.group_by else {
        return Ok(None);
    };
    match modifiers.as_slice() {
        [] => return Ok(None),
        [sqlparser::ast::GroupByWithModifier::Rollup] => {}
        _ => return unsupported("GROUP BY modifier"),
    }
    if keys.is_empty() || keys.len() > MOST_KEYS {
        return unsupported("WITH ROLLUP over this many keys");
    }
    let key_names = keys
        .iter()
        .map(|key| match key {
            Expr::Identifier(column) => Ok(column),
            Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(&parts[1]),
            _ => unsupported("WITH ROLLUP key that is not a whole column"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(having) = &select.having {
        if !aggregates_or_literals_only(having) {
            return unsupported("WITH ROLLUP HAVING naming a key");
        }
    }
    let mut names = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
            _ => return unsupported("WITH ROLLUP projection"),
        };
        let name = match (rolled_up_key(expr, keys), alias) {
            (Some(_), Some(alias)) => alias.value.clone(),
            (Some(key), None) => key_names[key].value.clone(),
            (None, _) if is_rolled_up_aggregate(expr) => match alias {
                Some(alias) => alias.value.clone(),
                None => {
                    source_text(render_context.source, expr).ok_or(ParseError::Unsupported {
                        feature: "WITH ROLLUP column whose source text cannot be recovered",
                    })?
                }
            },
            (None, _) => return unsupported("WITH ROLLUP projection"),
        };
        if name.starts_with("__rollup")
            || names
                .iter()
                .any(|named: &String| named.eq_ignore_ascii_case(&name))
        {
            return unsupported("WITH ROLLUP with two columns of one name");
        }
        names.push(name);
    }
    let parameters_before = render_context.parameter_count;
    let mut levels = Vec::with_capacity(keys.len() + 1);
    let mut sources = Vec::new();
    for rolled_up in 0..=keys.len() {
        let kept = keys.len() - rolled_up;
        let (level, level_sources) =
            render_select_body(&level_of(select, keys, kept, &names), render_context)?;
        if levels.is_empty() {
            sources = level_sources;
        }
        levels.push(level);
    }
    if render_context.parameter_count != parameters_before {
        return unsupported("WITH ROLLUP with a parameter");
    }
    let order = (0..keys.len())
        .map(|key| format!("\"__rollup_level_{key}\", \"__rollup_key_{key}\""))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Some((
        format!(
            "SELECT {} FROM ({}) AS \"__rollup\" ORDER BY {order}",
            names
                .iter()
                .map(|name| render_ident_str(name))
                .collect::<Vec<_>>()
                .join(", "),
            levels.join(" UNION ALL "),
        ),
        sources,
    )))
}

/// Answers which key a projected expression is, when it is one.
pub(super) fn rolled_up_key(expr: &Expr, keys: &[Expr]) -> Option<usize> {
    let (qualifier, column) = whole_column(expr)?;
    keys.iter().position(|key| {
        whole_column(key).is_some_and(|(key_qualifier, key_column)| {
            key_column.value.eq_ignore_ascii_case(&column.value)
                && match (qualifier, key_qualifier) {
                    (Some(qualifier), Some(key_qualifier)) => {
                        qualifier.value.eq_ignore_ascii_case(&key_qualifier.value)
                    }
                    _ => true,
                }
        })
    })
}

fn whole_column(expr: &Expr) -> Option<(Option<&Ident>, &Ident)> {
    match expr {
        Expr::Identifier(column) => Some((None, column)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Some((Some(&parts[0]), &parts[1])),
        _ => None,
    }
}

/// Reports whether an expression is an aggregate a rolled-up statement takes:
/// a count, and a total, average, largest or smallest over one column.
pub(super) fn is_rolled_up_aggregate(expr: &Expr) -> bool {
    matches!(
        static_select_metadata::classify_static_select_expr(expr),
        Some(
            StaticSelectMetadata::Count
                | StaticSelectMetadata::ColumnAggregate {
                    kind: ColumnAggregateKind::MinMax
                        | ColumnAggregateKind::Sum
                        | ColumnAggregateKind::Avg,
                    ..
                }
        )
    )
}

/// Writes the `SELECT` for one level: grouped by the first `kept` keys, the
/// rest answered as NULL, and each key's flag and value carried for the order.
fn level_of(
    select: &sqlparser::ast::Select,
    keys: &[Expr],
    kept: usize,
    names: &[String],
) -> sqlparser::ast::Select {
    let null = || Expr::Value(Value::Null.into());
    let number = |value: usize| Expr::Value(Value::Number(value.to_string(), false).into());
    let named = |expr: Expr, name: &str| SelectItem::ExprWithAlias {
        expr,
        alias: Ident::with_quote('"', name),
    };
    let mut projection = select
        .projection
        .iter()
        .zip(names)
        .map(|(item, name)| {
            let expr = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                _ => unreachable!("a rolled-up projection was checked to be expressions"),
            };
            match rolled_up_key(expr, keys) {
                Some(key) if key >= kept => named(null(), name),
                Some(_) => named(expr.clone(), name),
                None => item.clone(),
            }
        })
        .collect::<Vec<_>>();
    for (index, key) in keys.iter().enumerate() {
        let rolled_up = index >= kept;
        projection.push(named(
            number(usize::from(rolled_up)),
            &format!("__rollup_level_{index}"),
        ));
        projection.push(named(
            if rolled_up { null() } else { key.clone() },
            &format!("__rollup_key_{index}"),
        ));
    }
    let mut level = select.clone();
    level.projection = projection;
    level.group_by = sqlparser::ast::GroupByExpr::Expressions(keys[..kept].to_vec(), Vec::new());
    // Measured on MySQL 8.4.11: over no rows at all there are no groups and
    // no grand total either, where a `SELECT` grouping nothing answers a row.
    if kept == 0 {
        let some_rows = sqlparser::parser::Parser::new(&MySqlDialect {})
            .try_with_sql("COUNT(*) > 0")
            .and_then(|mut parser| parser.parse_expr())
            .expect("a written count parses");
        level.having = Some(match level.having.take() {
            Some(having) => Expr::BinaryOp {
                left: Box::new(Expr::Nested(Box::new(having))),
                op: BinaryOperator::And,
                right: Box::new(some_rows),
            },
            None => some_rows,
        });
    }
    level
}
