//! Source columns for result metadata that the engine loses in sorters.

use sqlparser::ast::{Expr, Select, SelectItem, SetExpr, SetQuantifier, Statement, Value};

use crate::{read_one_statement, ParseError, SessionSqlMode};

/// The source of one explicitly written SELECT result expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlSelectProjectionOrigin {
    Column {
        /// Table name or alias, when the expression qualifies its column.
        table: Option<String>,
        /// The source column, independent of the result alias.
        column: String,
    },
    NonNullLiteral,
    Null,
    Other,
}

/// Returns result expressions in branch and projection order.
///
/// A wildcard is marked `Other`: its expansion cannot safely be aligned with
/// the written projection positions until the engine has prepared the query.
pub fn select_projection_origins(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<Vec<MySqlSelectProjectionOrigin>>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Statement::Query(query) = read_statement.as_ref().as_ref().map_err(Clone::clone)? else {
        return Err(ParseError::ExpectedSelect);
    };
    let branches = match query.body.as_ref() {
        SetExpr::Select(select) => vec![select.as_ref()],
        SetExpr::SetOperation { .. } => compound_branches(query.body.as_ref())?,
        _ => {
            return Err(ParseError::Unsupported {
                feature: "SELECT projection",
            });
        }
    };
    Ok(branches.into_iter().map(projection_origins).collect())
}

/// Reads a chain of set operations into its branches, in the order they are
/// written; `a UNION b UNION c` nests the first two on the left.
fn compound_branches(expr: &SetExpr) -> Result<Vec<&Select>, ParseError> {
    let unsupported = ParseError::Unsupported {
        feature: "compound branch projection",
    };
    match expr {
        SetExpr::SetOperation { left, right, .. } => {
            let mut branches = match left.as_ref() {
                SetExpr::SetOperation { .. } => compound_branches(left)?,
                left => vec![branch_select(left).ok_or_else(|| unsupported.clone())?],
            };
            branches.push(branch_select(right).ok_or(unsupported)?);
            Ok(branches)
        }
        _ => Err(unsupported),
    }
}

/// Answers whether a compound query drops a row equal to one it already
/// answers — a plain `UNION`, `EXCEPT` or `INTERSECT` — rather than keeping
/// every row the way `UNION ALL` does. A query that is not compound drops
/// nothing.
pub fn compound_drops_repeated_rows(sql: &str, mode: SessionSqlMode) -> Result<bool, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Statement::Query(query) = read_statement.as_ref().as_ref().map_err(Clone::clone)? else {
        return Err(ParseError::ExpectedSelect);
    };
    Ok(match query.body.as_ref() {
        SetExpr::SetOperation { set_quantifier, .. } => {
            !matches!(set_quantifier, SetQuantifier::All)
        }
        _ => false,
    })
}

fn branch_select(expr: &SetExpr) -> Option<&Select> {
    match expr {
        SetExpr::Select(select) => Some(select),
        SetExpr::Query(query) => branch_select(query.body.as_ref()),
        _ => None,
    }
}

fn projection_origins(select: &Select) -> Vec<MySqlSelectProjectionOrigin> {
    select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                expression_origin(expr)
            }
            _ => MySqlSelectProjectionOrigin::Other,
        })
        .collect()
}

fn expression_origin(expr: &Expr) -> MySqlSelectProjectionOrigin {
    match expr {
        Expr::Identifier(column) => MySqlSelectProjectionOrigin::Column {
            table: None,
            column: column.value.clone(),
        },
        Expr::CompoundIdentifier(parts) if parts.len() == 2 || parts.len() == 3 => {
            MySqlSelectProjectionOrigin::Column {
                table: Some(parts[parts.len() - 2].value.clone()),
                column: parts[parts.len() - 1].value.clone(),
            }
        }
        Expr::Value(value) => match &value.value {
            Value::Number(_, _)
            | Value::Boolean(_)
            | Value::SingleQuotedString(_)
            | Value::DoubleQuotedString(_) => MySqlSelectProjectionOrigin::NonNullLiteral,
            Value::Null => MySqlSelectProjectionOrigin::Null,
            _ => MySqlSelectProjectionOrigin::Other,
        },
        Expr::Nested(inner) => expression_origin(inner),
        _ => MySqlSelectProjectionOrigin::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_each_compound_branch_and_source_name_behind_aliases() {
        let branches = select_projection_origins(
            "(SELECT a.id AS result, 1 FROM a) UNION (SELECT b.n, NULL FROM b)",
            SessionSqlMode::default(),
        )
        .unwrap();
        assert_eq!(
            branches,
            vec![
                vec![
                    MySqlSelectProjectionOrigin::Column {
                        table: Some("a".into()),
                        column: "id".into(),
                    },
                    MySqlSelectProjectionOrigin::NonNullLiteral,
                ],
                vec![
                    MySqlSelectProjectionOrigin::Column {
                        table: Some("b".into()),
                        column: "n".into(),
                    },
                    MySqlSelectProjectionOrigin::Null,
                ],
            ]
        );
    }

    #[test]
    fn keeps_every_branch_of_a_chain_in_the_order_written() {
        let branches = select_projection_origins(
            "SELECT a FROM t UNION SELECT b FROM t UNION (SELECT c FROM t)",
            SessionSqlMode::default(),
        )
        .unwrap();
        let column = |name: &str| {
            vec![MySqlSelectProjectionOrigin::Column {
                table: None,
                column: name.into(),
            }]
        };
        assert_eq!(branches, vec![column("a"), column("b"), column("c")]);
    }
}
