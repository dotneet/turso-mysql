//! A column qualified by the one table a statement reads, where only a bare
//! column is taken.
//!
//! TypeORM's query builder writes every column with its alias —
//! `SUM(user.balance)` and `JSON_EXTRACT(user.profile, '$.city')` over
//! `FROM users user` — and Django writes `SUM(users.balance)` over
//! `FROM users`. An aggregate answers a shape worked out from the column it
//! reads, and a JSON reading is held to reading a `JSON` column; both look the
//! column up by its name, so a qualified one is refused wherever the
//! qualifier could name another table. Where the statement reads one table,
//! and the name is not inside a subquery, the qualifier can only name that
//! table, and the bare column is the same column: measured on MySQL 8.4.11,
//! `SUM(u.balance)` answers what `SUM(balance)` answers, value and metadata
//! alike, the result column still named after the call as written.
//!
//! Only the projection and the `WHERE` are rewritten, and only those two
//! readings, because each looks the name up among the table's own columns
//! afterwards: a name that is no column of the table is still refused. The
//! engine reads a bare name in a `WHERE`, a `HAVING` or an `ORDER BY` as one
//! of the projection's aliases when no column has it, where MySQL answers
//! 1054 for the qualified name.

use super::*;

/// Leaves the table's name out of each aggregate's argument and each JSON
/// reading's column in a statement reading one table, where the name is that
/// table's.
pub(crate) fn leave_the_one_table_out(query: &mut sqlparser::ast::Query) {
    if query.with.is_some() {
        return;
    }
    let SetExpr::Select(select) = query.body.as_mut() else {
        return;
    };
    let [sqlparser::ast::TableWithJoins { relation, joins }] = select.from.as_slice() else {
        return;
    };
    if !joins.is_empty() {
        return;
    }
    let TableFactor::Table { name, alias, .. } = relation else {
        return;
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return;
    };
    let reference = alias
        .as_ref()
        .map_or_else(|| table.value.clone(), |alias| alias.name.value.clone());
    for item in &mut select.projection {
        if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } = item {
            leave_the_table_out(expr, &reference);
        }
    }
    if let Some(selection) = &mut select.selection {
        leave_the_table_out(selection, &reference);
    }
}

/// Follows an expression down to its aggregates and JSON readings, never
/// into a subquery, whose own tables a qualifier inside it could name.
fn leave_the_table_out(expr: &mut Expr, reference: &str) {
    match expr {
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            leave_the_table_out(inner, reference);
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Arrow | BinaryOperator::LongArrow,
            ..
        } => bare_column(left, reference),
        Expr::BinaryOp { left, right, .. } => {
            leave_the_table_out(left, reference);
            leave_the_table_out(right, reference);
        }
        // SQLAlchemy reads a JSON member through a `CASE` around two readings.
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            for part in operand.iter_mut().chain(else_result.iter_mut()) {
                leave_the_table_out(part, reference);
            }
            for when in conditions {
                leave_the_table_out(&mut when.condition, reference);
                leave_the_table_out(&mut when.result, reference);
            }
        }
        Expr::Function(function) => {
            let reads_a_column =
                is_an_aggregate_this_reads(function) || is_a_json_reading(function);
            let sqlparser::ast::FunctionArguments::List(arguments) = &mut function.args else {
                return;
            };
            for (place, argument) in arguments.args.iter_mut().enumerate() {
                let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    argument,
                )) = argument
                else {
                    continue;
                };
                if place == 0 && reads_a_column {
                    bare_column(argument, reference);
                }
                leave_the_table_out(argument, reference);
            }
        }
        _ => {}
    }
}

fn bare_column(expr: &mut Expr, reference: &str) {
    let Expr::CompoundIdentifier(parts) = expr else {
        return;
    };
    if let [qualifier, column] = parts.as_slice() {
        if qualifier.value.eq_ignore_ascii_case(reference) {
            *expr = Expr::Identifier(column.clone());
        }
    }
}

fn is_an_aggregate_this_reads(function: &sqlparser::ast::Function) -> bool {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    ["COUNT", "SUM", "AVG", "MIN", "MAX"]
        .iter()
        .any(|aggregate| name.value.eq_ignore_ascii_case(aggregate))
}

fn is_a_json_reading(function: &sqlparser::ast::Function) -> bool {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    name.value
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("JSON_"))
}
