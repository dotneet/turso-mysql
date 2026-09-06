//! Translating a checked MySQL `SELECT`, `INSERT`, `UPDATE` or `DELETE` into
//! the SQLite text the engine runs.
//!
//! This is the widest single job in the crate. A `SELECT` has to be rewritten
//! clause by clause, and along the way it has to record what the frontend
//! cannot see later: which table each result column came from, and which
//! projections have a shape the server still has to resolve into a wire type.
//!
//! The DML statements sit here too rather than in a file of their own, because
//! their `WHERE` clause is rendered by the same code, through the same
//! [`SelectRenderContext`].

use super::*;

/// One table a `SELECT` reads, with the name the engine reports for it.
///
/// A join reports each column against the reference in the statement, which is
/// the alias when there is one, so both spellings have to be kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlSelectSource {
    reference: String,
    table: MySqlTableName,
    outer: bool,
    branch: usize,
    subquery: bool,
    projected_columns: Vec<String>,
}

impl MySqlSelectSource {
    /// Returns the name the engine reports for this table's columns.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Returns the table itself.
    pub const fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Reports whether an outer join can leave this table's columns NULL.
    ///
    /// Measured on MySQL 8.4.11: a `NOT NULL` column on the outer side of a
    /// `LEFT JOIN` reports no `NOT_NULL` flag, while its key flags stay, and
    /// the inner side keeps everything. A `RIGHT JOIN` is the mirror image.
    pub const fn outer(&self) -> bool {
        self.outer
    }

    /// Returns the columns a `WITH` name projects, in order.
    ///
    /// Empty for an ordinary table, whose columns are the table's own. A CTE
    /// can project a table's columns in any order, so a result column naming
    /// the CTE and an ordinal is resolved through this list rather than
    /// straight into the table.
    pub fn projected_columns(&self) -> &[String] {
        &self.projected_columns
    }

    /// Reports whether a subquery reads this table rather than the statement
    /// itself.
    ///
    /// It is still authorized and still refused when it names an internal
    /// catalog table; what it does not do is name any of the result columns.
    pub const fn subquery(&self) -> bool {
        self.subquery
    }

    /// Returns which branch of a `UNION` reads this table, counting from zero.
    ///
    /// Every table a single statement reads is branch zero, joins included.
    /// A second branch means the result columns belong to no one table, which
    /// is what MySQL reports for a `UNION`.
    pub const fn branch(&self) -> usize {
        self.branch
    }
}

pub(crate) struct RenderedSelect {
    pub(crate) sqlite_sql: String,
    pub(crate) orders_a_bare_column: bool,
    pub(crate) compares_a_placeholder: bool,
    pub(crate) checked_subquery_comparisons: Vec<CheckedSubqueryComparison>,
    pub(crate) source_table: Option<MySqlTableName>,
    pub(crate) source_tables: Vec<MySqlSelectSource>,
    pub(crate) checked_comparisons: Vec<CheckedSelectComparison>,
    pub(crate) parameter_count: usize,
}

pub(crate) fn translate_select_query(
    query: &sqlparser::ast::Query,
    sql: &str,
    text_columns: &[String],
) -> Result<RenderedSelect, ParseError> {
    if query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("SELECT query clause");
    }
    let mut render_context = SelectRenderContext::new(sql, text_columns);
    let (mut prefix, mut cte_tables) = (String::new(), Vec::new());
    if let Some(with) = &query.with {
        let (rendered, sources) = render_common_table_expressions(with, &mut render_context)?;
        prefix = rendered;
        cte_tables = sources;
    }
    // An `ORDER BY` ordinal names a projected column, so the projection has to
    // outlive the body that rendered it. A UNION orders by its first branch.
    let ordered_projection: &[SelectItem] = match query.body.as_ref() {
        SetExpr::Select(select) => &select.projection,
        SetExpr::SetOperation { left, .. } => match left.as_ref() {
            SetExpr::Select(select) => &select.projection,
            _ => &[],
        },
        _ => &[],
    };
    let (mut normalized, mut source_tables) = match query.body.as_ref() {
        SetExpr::Select(select) => render_select_body(select, &mut render_context)?,
        // MySQL's other set operations, EXCEPT and INTERSECT, arrived in 8.0.31
        // and answer rows a UNION does not.
        SetExpr::SetOperation {
            left,
            op: sqlparser::ast::SetOperator::Union,
            set_quantifier,
            right,
        } => {
            let keyword = match set_quantifier {
                sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct => {
                    "UNION"
                }
                sqlparser::ast::SetQuantifier::All => "UNION ALL",
                _ => return unsupported("SELECT UNION quantifier"),
            };
            let (SetExpr::Select(left), SetExpr::Select(right)) = (left.as_ref(), right.as_ref())
            else {
                return unsupported("SELECT UNION branch");
            };
            let (left, mut sources) = render_select_body(left, &mut render_context)?;
            let (right, right_sources) = render_select_body(right, &mut render_context)?;
            sources.extend(right_sources.into_iter().map(|mut source| {
                source.branch = 1;
                source
            }));
            (format!("{left} {keyword} {right}"), sources)
        }
        _ => return unsupported("compound SELECT query"),
    };
    // A statement that names a CTE reads the CTE's own table under the CTE's
    // name, which is how its result columns find their metadata.
    for source in &mut source_tables {
        if let Some(cte) = cte_tables
            .iter()
            .find(|cte| cte.reference.eq_ignore_ascii_case(&source.reference))
        {
            source.table = cte.table.clone();
            source.projected_columns.clone_from(&cte.projected_columns);
        }
    }
    source_tables.append(&mut render_context.subquery_tables);
    normalized.insert_str(0, &prefix);
    let source_table = match source_tables
        .iter()
        .filter(|source| !source.subquery)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [source] => Some(source.table.clone()),
        _ => None,
    };
    if let Some(order_by) = &query.order_by {
        normalized.push_str(" ORDER BY ");
        normalized.push_str(&render_select_order_by(
            order_by,
            ordered_projection,
            &mut render_context,
        )?);
    }
    if let Some(limit) = &query.limit_clause {
        normalized.push_str(&render_select_limit(limit)?);
    }
    Ok(RenderedSelect {
        sqlite_sql: normalized,
        orders_a_bare_column: render_context.orders_a_bare_column,
        compares_a_placeholder: render_context.compares_a_placeholder,
        checked_subquery_comparisons: render_context.checked_subquery_comparisons,
        source_table,
        source_tables,
        checked_comparisons: render_context.checked_comparisons,
        parameter_count: render_context.parameter_count,
    })
}

/// Renders one `SELECT` body, which is either the whole statement or one
/// branch of a `UNION`.
fn render_select_body(
    select: &sqlparser::ast::Select,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || !matches!(
            select.distinct,
            None | Some(sqlparser::ast::Distinct::Distinct)
        )
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(_, modifiers) if modifiers.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("SELECT feature");
    }

    let projection = select
        .projection
        .iter()
        .map(|item| render_select_item(item, render_context))
        .collect::<Result<Vec<_>, _>>()?;
    if projection.is_empty() {
        return unsupported("SELECT without projections");
    }

    let (from, source_tables) = match select.from.as_slice() {
        [] => (None, Vec::new()),
        [from] => {
            let (mut rendered, source) = render_select_table(&from.relation)?;
            let mut sources = vec![source];
            for join in &from.joins {
                let (joined, mut source) = render_select_table(&join.relation)?;
                let (keyword, constraint) = checked_join(&join.join_operator)?;
                match keyword {
                    // The side that can go missing is the one whose columns
                    // stop being NOT NULL.
                    "LEFT JOIN" => source.outer = true,
                    "RIGHT JOIN" => {
                        for earlier in &mut sources {
                            earlier.outer = true;
                        }
                    }
                    _ => {}
                }
                sources.push(source);
                rendered.push(' ');
                rendered.push_str(keyword);
                rendered.push(' ');
                rendered.push_str(&joined);
                rendered.push_str(" ON ");
                rendered.push_str(&render_join_predicate(constraint)?);
            }
            (Some(rendered), sources)
        }
        // MySQL's comma join is a cross join, which this would have to bound
        // before it could answer one.
        _ => return unsupported("multiple SELECT table sources"),
    };
    if source_tables
        .iter()
        .filter(|source| !source.subquery)
        .count()
        > 1
    {
        reject_unqualified_join_projection(&select.projection)?;
    }

    let mut normalized = format!(
        "SELECT {}{}",
        if select.distinct.is_some() {
            "DISTINCT "
        } else {
            ""
        },
        projection.join(", ")
    );
    if let Some(from) = from {
        normalized.push_str(" FROM ");
        normalized.push_str(&from);
    }
    if let Some(selection) = &select.selection {
        normalized.push_str(" WHERE ");
        normalized.push_str(&render_select_predicate(selection, render_context)?);
    }
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        unreachable!("the GROUP BY shape was checked above");
    };
    if !group_by.is_empty() {
        normalized.push_str(" GROUP BY ");
        normalized.push_str(&render_select_group_by(group_by, &select.projection)?);
    }
    if let Some(having) = &select.having {
        if group_by.is_empty() {
            // MySQL takes a HAVING with no GROUP BY, over the one implicit
            // group. What that means for an ungrouped column has not been
            // measured, so it waits.
            return unsupported("HAVING without a GROUP BY");
        }
        normalized.push_str(" HAVING ");
        normalized.push_str(&render_having_predicate(having, render_context)?);
    }
    Ok((normalized, source_tables))
}

/// Reads the join keyword and the `ON` a checked join is written with.
///
/// Only an `ON` that equates whole columns is taken. The two engines agree
/// about a column-to-column equality without any coercion question, which is
/// what makes a join crossable while a literal comparison still goes through
/// the checked path.
fn checked_join(
    operator: &sqlparser::ast::JoinOperator,
) -> Result<(&'static str, &Expr), ParseError> {
    use sqlparser::ast::{JoinConstraint, JoinOperator};
    let (keyword, JoinConstraint::On(expr)) = (match operator {
        JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => ("JOIN", constraint),
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            ("LEFT JOIN", constraint)
        }
        JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint) => {
            ("RIGHT JOIN", constraint)
        }
        _ => return unsupported("SELECT JOIN form"),
    }) else {
        return unsupported("SELECT JOIN form");
    };
    Ok((keyword, expr))
}

fn render_join_predicate(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Nested(inner) => Ok(format!("({})", render_join_predicate(inner)?)),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => Ok(format!(
            "({} AND {})",
            render_join_predicate(left)?,
            render_join_predicate(right)?
        )),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => Ok(format!(
            "({} = {})",
            render_join_column(left)?,
            render_join_column(right)?
        )),
        _ => unsupported("SELECT JOIN ON predicate"),
    }
}

fn render_join_column(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        _ => unsupported("SELECT JOIN ON requires a qualified column on each side"),
    }
}

/// Holds a joined projection to qualified columns.
///
/// An unqualified name in a join is ambiguous whenever both tables carry it,
/// and every metadata lookup this frontend does is by name, so the rule is
/// simply that a join names its tables.
fn reject_unqualified_join_projection(projection: &[SelectItem]) -> Result<(), ParseError> {
    for item in projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            _ => continue,
        };
        if matches!(expr, Expr::Identifier(_)) {
            return unsupported("SELECT JOIN requires a qualified column in the projection");
        }
    }
    Ok(())
}

/// Renders a `WITH` clause, and returns what each name stands for.
///
/// Each body has to read one table and project its columns in order, because a
/// result column reaching the frontend names the CTE and an ordinal, and the
/// only way to answer what type it has is to read that ordinal from the table
/// the CTE reads.
fn render_common_table_expressions(
    with: &sqlparser::ast::With,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    if with.recursive {
        return unsupported("WITH RECURSIVE");
    }
    let mut rendered = Vec::with_capacity(with.cte_tables.len());
    let mut sources = Vec::with_capacity(with.cte_tables.len());
    for cte in &with.cte_tables {
        if cte.from.is_some()
            || cte.materialized.is_some()
            || !cte.alias.columns.is_empty()
            || cte.alias.at.is_some()
        {
            return unsupported("WITH option");
        }
        let (body, projected) = render_subquery(&cte.query, render_context)?;
        let Some(source) = render_context.subquery_tables.pop() else {
            return unsupported("WITH body requires one table");
        };
        let _ = projected;
        if source.projected_columns.is_empty() {
            // A wildcard or an expression leaves no name to resolve a result
            // column's ordinal through.
            return unsupported("WITH body requires a projection of whole columns");
        }
        rendered.push(format!("{} AS ({body})", render_ident(&cte.alias.name)));
        sources.push(MySqlSelectSource {
            reference: cte.alias.name.value.clone(),
            table: source.table,
            outer: false,
            branch: 0,
            subquery: false,
            projected_columns: source.projected_columns,
        });
    }
    Ok((format!("WITH {} ", rendered.join(", ")), sources))
}

/// Renders `column IN (SELECT column FROM table)`.
///
/// The two columns have to be the same kind, which only the frontend can see,
/// so the pair is recorded for it to check — a membership test raises the same
/// coercion question a literal comparison does.
fn render_in_subquery(
    expr: &Expr,
    subquery: &sqlparser::ast::Query,
    negated: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let Expr::Identifier(column) = expr else {
        return unsupported("SELECT IN requires one unqualified column");
    };
    let (rendered, projected) = render_subquery(subquery, render_context)?;
    let Some((inner_table, inner_column_name)) = projected else {
        return unsupported("SELECT IN requires a subquery projecting one column");
    };
    render_context
        .checked_subquery_comparisons
        .push(CheckedSubqueryComparison {
            column_name: column.value.clone(),
            inner_table,
            inner_column_name,
        });
    Ok(format!(
        "({} {}IN ({rendered}))",
        render_ident(column),
        if negated { "NOT " } else { "" }
    ))
}

/// Renders one subquery, and returns the single column it projects when it
/// projects one.
///
/// Its tables are kept apart from the statement's own: they are authorized and
/// refused the same way, but they name none of the result columns, so the rules
/// about a joined projection do not apply to them.
fn render_subquery(
    subquery: &sqlparser::ast::Query,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Option<(String, String)>), ParseError> {
    if subquery.with.is_some()
        || subquery.order_by.is_some()
        || subquery.limit_clause.is_some()
        || subquery.fetch.is_some()
        || !subquery.locks.is_empty()
        || subquery.for_clause.is_some()
        || subquery.settings.is_some()
        || subquery.format_clause.is_some()
        || !subquery.pipe_operators.is_empty()
    {
        return unsupported("SELECT subquery clause");
    }
    let SetExpr::Select(select) = subquery.body.as_ref() else {
        return unsupported("SELECT subquery body");
    };
    let (rendered, mut sources) = render_select_body(select, render_context)?;
    let [source] = sources.as_slice() else {
        return unsupported("SELECT subquery requires one table");
    };
    let projected = match select.projection.as_slice() {
        [SelectItem::UnnamedExpr(Expr::Identifier(column))] => {
            Some((source.table.as_str().to_owned(), column.value.clone()))
        }
        _ => None,
    };
    let projected_columns = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(Expr::Identifier(column)) => Some(column.value.clone()),
            SelectItem::ExprWithAlias {
                expr: Expr::Identifier(column),
                ..
            } => Some(column.value.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    for source in &mut sources {
        source.subquery = true;
        source.projected_columns = projected_columns.clone().unwrap_or_default();
    }
    render_context.subquery_tables.append(&mut sources);
    Ok((rendered, projected))
}

/// Renders a `HAVING`, which sees an aggregate where a `WHERE` sees a column.
///
/// A comparison on a grouping column goes through the same checked path a
/// `WHERE` comparison does. One on an aggregate cannot, since there is no
/// column to compare types against — so the aggregate's own argument column is
/// recorded instead, which is what makes an integer literal safe to compare
/// against. `COUNT` records nothing, because it answers an integer whatever it
/// counts.
fn render_having_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_having_predicate(left, render_context)?,
                render_having_predicate(right, render_context)?
            ))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_having_predicate(expr, render_context)?
        )),
        Expr::Nested(expr) => Ok(format!(
            "({})",
            render_having_predicate(expr, render_context)?
        )),
        Expr::BinaryOp { left, op, right }
            if is_checked_select_comparison_operator(op)
                && matches!(left.as_ref(), Expr::Function(function)
                    if static_select_metadata::is_count_call(function)
                        || static_select_metadata::column_aggregate_argument(function).is_some()) =>
        {
            let Expr::Function(function) = left.as_ref() else {
                unreachable!("the guard requires a checked aggregate");
            };
            let (rendered_right, rhs) =
                render_checked_select_comparison_rhs(right, render_context)?;
            if !matches!(rhs, CheckedSelectComparisonRhs::SignedInteger(_)) {
                return unsupported("HAVING comparison requires an exact signed integer");
            }
            if let Some((_, column)) = static_select_metadata::column_aggregate_argument(function) {
                render_context
                    .checked_comparisons
                    .push(CheckedSelectComparison {
                        column_name: column.value.clone(),
                        operator: checked_select_comparison_operator(op)
                            .expect("comparison operator guard"),
                        rhs,
                        collated: false,
                    });
            }
            Ok(format!(
                "({} {} {rendered_right})",
                render_aggregate_call(function),
                checked_select_comparison_sql_operator(op)
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => render_checked_between(*negated, expr, low, high, render_context),
        _ => unsupported("HAVING predicate"),
    }
}

/// Renders a `GROUP BY` over plain columns and holds the projection to
/// MySQL's `ONLY_FULL_GROUP_BY`.
///
/// That mode is in MySQL 8.4's default `sql_mode`, and this server takes a
/// client's `SET sql_mode` naming it, so the rule has to be real here: every
/// projection that is not an aggregate or a literal has to be one of the
/// grouping columns, or the row it lands in is one of several and MySQL
/// answers 1055.
fn render_select_group_by(
    group_by: &[Expr],
    projection: &[SelectItem],
) -> Result<String, ParseError> {
    let mut columns = Vec::with_capacity(group_by.len());
    for expr in group_by {
        let Some(column) = grouped_column(expr) else {
            return unsupported("GROUP BY requires a whole column");
        };
        columns.push(column);
    }
    for item in projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            // A wildcard names columns this cannot see, so it cannot be held to
            // the rule and is refused rather than let through.
            _ => return unsupported("GROUP BY with a wildcard projection"),
        };
        if static_select_metadata::classify_static_select_expr(expr).is_some() {
            continue;
        }
        let Some(projected) = grouped_column(expr) else {
            return unsupported("GROUP BY with an unchecked projection");
        };
        if !columns
            .iter()
            .any(|column| names_same_column(*column, projected))
        {
            return unsupported("GROUP BY leaves a projected column out of the grouping");
        }
    }
    Ok(columns
        .into_iter()
        .map(|(table, column)| match table {
            Some(table) => format!("{}.{}", render_ident(table), render_ident(column)),
            None => render_ident(column),
        })
        .collect::<Vec<_>>()
        .join(", "))
}

/// Reads a whole column, qualified or not, from a `GROUP BY` key or a
/// projection.
type GroupedColumn<'a> = (Option<&'a Ident>, &'a Ident);

fn grouped_column(expr: &Expr) -> Option<GroupedColumn<'_>> {
    match expr {
        Expr::Identifier(column) => Some((None, column)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Some((Some(&parts[0]), &parts[1])),
        _ => None,
    }
}

/// Reports whether a grouping key and a projected column name the same column.
///
/// A client may qualify one and not the other, which MySQL takes whenever the
/// bare name is unambiguous. The engine answers the ambiguous case itself, so
/// matching on the column name is enough here.
fn names_same_column(key: GroupedColumn<'_>, projected: GroupedColumn<'_>) -> bool {
    if !key.1.value.eq_ignore_ascii_case(&projected.1.value) {
        return false;
    }
    match (key.0, projected.0) {
        (Some(key), Some(projected)) => key.value.eq_ignore_ascii_case(&projected.value),
        _ => true,
    }
}

pub(crate) fn select_static_result_metadata(
    query: &sqlparser::ast::Query,
) -> Vec<StaticSelectProjectionMetadata> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Vec::new();
    };
    select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                classify_static_select_expr(expr).map_or(
                    StaticSelectProjectionMetadata::Other,
                    StaticSelectProjectionMetadata::Literal,
                )
            }
            SelectItem::ExprWithAliases { .. } => StaticSelectProjectionMetadata::Other,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                StaticSelectProjectionMetadata::Wildcard
            }
        })
        .collect()
}

fn render_select_order_by(
    order_by: &sqlparser::ast::OrderBy,
    projection: &[SelectItem],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return unsupported("SELECT ORDER BY option");
    };
    if expressions.is_empty() || order_by.interpolate.is_some() {
        return unsupported("SELECT ORDER BY option");
    }
    expressions
        .iter()
        .map(|expression| {
            if expression.options.nulls_first.is_some() || expression.with_fill.is_some() {
                return unsupported("SELECT ORDER BY option");
            }
            // MySQL reads a bare positive integer here as "the nth projected
            // column", and only a bare one: `ORDER BY -1` and `ORDER BY 1+1`
            // are constant expressions that order nothing. Resolving it to the
            // projection it names is what makes it order the way that column
            // would, collation included.
            let expr = match order_by_ordinal(&expression.expr) {
                Some(ordinal) => projected_expr(projection, ordinal)?,
                None => &expression.expr,
            };
            // A grouped query orders by what it selected, which is as often an
            // aggregate as a column. Both render the same way they do in a
            // projection, so the engine sees the same expression twice.
            match expr {
                Expr::Identifier(_) => {}
                Expr::CompoundIdentifier(parts) if parts.len() == 2 => {}
                Expr::Function(function)
                    if static_select_metadata::is_count_call(function)
                        || static_select_metadata::column_aggregate_argument(function)
                            .is_some() => {}
                Expr::BinaryOp { .. }
                    if static_select_metadata::classify_arithmetic(expr).is_some() => {}
                _ => return unsupported("SELECT ORDER BY expression"),
            }
            let direction = if expression.options.asc == Some(false) {
                "DESC"
            } else {
                "ASC"
            };
            // MySQL's default collation ignores case when it orders, just as
            // it does when it compares, so a text column is ordered the same
            // way its WHERE compares it.
            let collation = match expr {
                Expr::Identifier(column) => {
                    render_context.orders_a_bare_column = true;
                    if render_context.is_text_column(&column.value) {
                        " COLLATE NOCASE"
                    } else {
                        ""
                    }
                }
                _ => "",
            };
            Ok(format!(
                "{}{collation} {direction}",
                render_select_expr(expr, render_context)?
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|expressions| expressions.join(", "))
}

/// Reads the ordinal out of an `ORDER BY 2`, if that is what this is.
fn order_by_ordinal(expr: &Expr) -> Option<usize> {
    let Expr::Value(value) = expr else {
        return None;
    };
    let Value::Number(number, false) = &value.value else {
        return None;
    };
    number.parse::<usize>().ok()
}

/// Finds the expression an `ORDER BY` ordinal names.
///
/// MySQL answers 1054 for an ordinal outside the projection, and this refuses
/// instead. A wildcard is refused because its columns are not written down
/// here, so there is nothing to count through.
fn projected_expr(projection: &[SelectItem], ordinal: usize) -> Result<&Expr, ParseError> {
    if ordinal == 0 || ordinal > projection.len() {
        return unsupported("SELECT ORDER BY ordinal outside the projection");
    }
    match &projection[ordinal - 1] {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        _ => unsupported("SELECT ORDER BY an ordinal over a wildcard projection"),
    }
}

fn render_select_limit(clause: &sqlparser::ast::LimitClause) -> Result<String, ParseError> {
    use sqlparser::ast::{LimitClause, OffsetRows};

    let (limit, offset) = match clause {
        LimitClause::LimitOffset {
            limit: Some(limit),
            offset,
            limit_by,
        } if limit_by.is_empty() => {
            if offset
                .as_ref()
                .is_some_and(|offset| offset.rows != OffsetRows::None)
            {
                return unsupported("SELECT OFFSET option");
            }
            (limit, offset.as_ref().map(|offset| &offset.value))
        }
        LimitClause::OffsetCommaLimit { offset, limit } => (limit, Some(offset)),
        _ => return unsupported("SELECT LIMIT option"),
    };
    let mut rendered = format!(" LIMIT {}", render_select_row_count(limit)?);
    if let Some(offset) = offset {
        rendered.push_str(&format!(" OFFSET {}", render_select_row_count(offset)?));
    }
    Ok(rendered)
}

fn render_select_row_count(expr: &Expr) -> Result<i64, ParseError> {
    if let Expr::Value(value) = expr {
        if let Value::Number(number, false) = &value.value {
            if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
                if let Ok(number) = number.parse::<i64>() {
                    return Ok(number);
                }
            }
        }
    }
    unsupported("SELECT LIMIT/OFFSET requires an integer literal in 0..=9223372036854775807")
}

pub(crate) fn translate_insert(insert: &Insert) -> Result<String, ParseError> {
    if !insert.optimizer_hints.is_empty()
        || insert.or.is_some()
        || insert.ignore
        || !insert.into
        || insert.table_alias.is_some()
        || insert.overwrite
        || !insert.assignments.is_empty()
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.has_table_keyword
        || insert.on.is_some()
        || insert.returning.is_some()
        || insert.output.is_some()
        || insert.priority.is_some()
        || insert.insert_alias.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
        || insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return unsupported("INSERT option");
    }
    let sqlparser::ast::TableObject::TableName(table) = &insert.table else {
        return unsupported("INSERT table source");
    };
    let table = render_unqualified_name(table)?;
    let columns = insert
        .columns
        .iter()
        .map(render_unqualified_name)
        .collect::<Result<Vec<_>, _>>()?;
    let source = insert.source.as_deref().ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES",
    })?;
    if source.with.is_some()
        || source.order_by.is_some()
        || source.limit_clause.is_some()
        || source.fetch.is_some()
        || !source.locks.is_empty()
        || source.for_clause.is_some()
        || source.settings.is_some()
        || source.format_clause.is_some()
        || !source.pipe_operators.is_empty()
    {
        return unsupported("INSERT source query option");
    }
    let SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("INSERT source");
    };
    if values.explicit_row || values.value_keyword || values.rows.is_empty() {
        return unsupported("INSERT VALUES option");
    }
    // MySQL's REPLACE deletes the rows a unique key collides with and inserts,
    // which is what the engine's own OR REPLACE does.
    let verb = if insert.replace_into {
        "INSERT OR REPLACE INTO"
    } else {
        "INSERT INTO"
    };
    if columns.is_empty() {
        if values.rows.len() == 1 && values.rows[0].is_empty() {
            return Ok(format!("{verb} {table} DEFAULT VALUES"));
        }
        return unsupported("INSERT without an explicit column list");
    }
    let rows = values
        .rows
        .iter()
        .map(|row| {
            if row.is_empty() || row.len() != columns.len() {
                return unsupported("INSERT VALUES column count");
            }
            row.iter()
                .map(render_dml_expr)
                .collect::<Result<Vec<_>, _>>()
                .map(|values| format!("({})", values.join(", ")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "{verb} {table} ({}) VALUES {}",
        columns.join(", "),
        rows.join(", ")
    ))
}

pub(crate) fn translate_update(
    update: &Update,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if !update.optimizer_hints.is_empty()
        || !update.table.joins.is_empty()
        || update.from.is_some()
        || update.returning.is_some()
        || update.output.is_some()
        || update.or.is_some()
        || !update.order_by.is_empty()
        || update.limit.is_some()
    {
        return unsupported("UPDATE option");
    }
    let table = render_update_table(&update.table.relation)?;
    if update.assignments.is_empty() {
        return unsupported("UPDATE without assignments");
    }
    let assignments = update
        .assignments
        .iter()
        .map(|assignment| {
            let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
                return unsupported("UPDATE assignment target");
            };
            Ok(format!(
                "{} = {}",
                render_unqualified_name(column)?,
                render_dml_expr(&assignment.value)?
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut normalized = format!("UPDATE {table} SET {}", assignments.join(", "));
    if let Some(selection) = &update.selection {
        normalized.push_str(" WHERE ");
        normalized.push_str(&render_dml_predicate(selection, render_context)?);
    }
    Ok(normalized)
}

pub(crate) fn checked_update(update: &Update) -> Result<CheckedUpdate, ParseError> {
    let table_name = update_table_name(&update.table.relation)?;
    let assignments = update
        .assignments
        .iter()
        .map(|assignment| {
            let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
                return unsupported("UPDATE assignment target");
            };
            let [ObjectNamePart::Identifier(column)] = column.0.as_slice() else {
                return unsupported("qualified UPDATE assignment target");
            };
            Ok(CheckedUpdateAssignment {
                column_name: column.value.clone(),
                value: checked_update_assignment_value(&column.value, &assignment.value),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CheckedUpdate {
        table_name,
        assignments,
    })
}

fn checked_update_assignment_value(
    column_name: &str,
    value: &Expr,
) -> CheckedUpdateAssignmentValue {
    if matches!(
        value,
        Expr::Identifier(identifier) if identifier.value.eq_ignore_ascii_case(column_name)
    ) {
        return CheckedUpdateAssignmentValue::SelfAssignment;
    }
    direct_signed_integer(value)
        .map(CheckedUpdateAssignmentValue::SignedInteger)
        .unwrap_or(CheckedUpdateAssignmentValue::Other)
}

fn direct_signed_integer(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => value.parse().ok(),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(value, false) => value.parse().ok(),
                _ => None,
            },
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(value, false) => value.parse::<u64>().ok().and_then(|magnitude| {
                    if magnitude == (i64::MAX as u64) + 1 {
                        Some(i64::MIN)
                    } else {
                        i64::try_from(magnitude).ok().map(|value| -value)
                    }
                }),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Reads the one table a `DELETE` names, when it names one plainly.
pub(crate) fn delete_source_table(delete: &Delete) -> Option<String> {
    let FromTable::WithFromKeyword(tables) = &delete.from else {
        return None;
    };
    let [table] = tables.as_slice() else {
        return None;
    };
    let TableFactor::Table { name, .. } = &table.relation else {
        return None;
    };
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return None;
    };
    MySqlTableName::parse(&ident.value)
        .ok()
        .map(|name| name.as_str().to_owned())
}

pub(crate) fn translate_delete(
    delete: &Delete,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if !delete.optimizer_hints.is_empty()
        || !delete.tables.is_empty()
        || delete.using.is_some()
        || delete.returning.is_some()
        || delete.output.is_some()
        || !delete.order_by.is_empty()
        || delete.limit.is_some()
    {
        return unsupported("DELETE option");
    }
    let table = match &delete.from {
        FromTable::WithFromKeyword(from) => match from.as_slice() {
            [from] if from.joins.is_empty() => render_update_table(&from.relation)?,
            _ => return unsupported("DELETE table source"),
        },
        FromTable::WithoutKeyword(_) => return unsupported("DELETE without FROM"),
    };
    let mut normalized = format!("DELETE FROM {table}");
    if let Some(selection) = &delete.selection {
        normalized.push_str(" WHERE ");
        normalized.push_str(&render_dml_predicate(selection, render_context)?);
    }
    Ok(normalized)
}

fn render_update_table(table: &TableFactor) -> Result<String, ParseError> {
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = table
    else {
        return unsupported("UPDATE table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("UPDATE table option");
    }
    render_unqualified_name(name)
}

fn update_table_name(table: &TableFactor) -> Result<String, ParseError> {
    let TableFactor::Table { name, .. } = table else {
        return unsupported("UPDATE table source");
    };
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return unsupported("qualified UPDATE table name");
    };
    Ok(name.value.clone())
}

fn render_dml_expr(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Identifier(ident) => Ok(render_ident(ident)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => render_dml_number(value),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            Value::Placeholder(marker) if marker == "?" => Ok("?".to_string()),
            _ => unsupported("DML literal"),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(value, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let Ok(magnitude) = value.parse::<u64>() else {
                return Ok(format!("(-{})", render_dml_number(value)?));
            };
            if magnitude > (i64::MAX as u64) + 1 {
                return unsupported("DML numeric literal outside signed 64-bit integer range");
            }
            Ok(format!("(-{magnitude})"))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => Ok(format!("(+{})", render_dml_expr(expr)?)),
        Expr::Nested(expr) => Ok(format!("({})", render_dml_expr(expr)?)),
        _ => unsupported("DML expression"),
    }
}

/// Renders a numeric literal a DML statement may carry.
///
/// An integer is normalized through `i64` so that `007` reads back as `7`. A
/// fractional literal is passed through as written, because it is a `DOUBLE`'s
/// value and the engine reads the same IEEE 754 binary64 MySQL does. The
/// dialect's assignment validator holds an integer column to integers, so a
/// fractional value cannot land in one.
fn render_dml_number(value: &str) -> Result<String, ParseError> {
    if let Ok(integer) = value.parse::<i64>() {
        return Ok(integer.to_string());
    }
    if value.parse::<f64>().is_ok_and(f64::is_finite) {
        return Ok(value.to_owned());
    }
    unsupported("DML numeric literal outside signed 64-bit integer range")
}

/// Renders the `WHERE` of an `UPDATE` or a `DELETE`.
///
/// A comparison goes through the same checked path a `SELECT` comparison does,
/// and is recorded in `render_context` so the frontend can hold it to the same
/// rule: the two engines only agree about a comparison on a signed integer
/// column, which is what that rule was measured for.
fn render_dml_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_dml_predicate(left, render_context)?,
                render_dml_predicate(right, render_context)?
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => render_checked_like(
            *negated,
            *any,
            expr,
            pattern,
            escape_char.as_ref(),
            render_context,
        ),
        Expr::IsNull(expr) => Ok(format!("({} IS NULL)", render_dml_expr(expr)?)),
        Expr::IsNotNull(expr) => Ok(format!("({} IS NOT NULL)", render_dml_expr(expr)?)),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_dml_predicate(expr, render_context)?
        )),
        Expr::Nested(expr) => Ok(format!("({})", render_dml_predicate(expr, render_context)?)),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => render_dml_expr(expr),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => render_checked_between(*negated, expr, low, high, render_context),
        _ => unsupported("DML WHERE predicate"),
    }
}

#[derive(Default)]
pub(crate) struct SelectRenderContext<'a> {
    /// The statement as the client wrote it. MySQL names an unaliased
    /// expression column after its source text, spacing included, so the
    /// rendered alias has to come from here rather than from the AST.
    source: &'a str,
    /// Every table a subquery reads, which the statement authorizes alongside
    /// its own.
    subquery_tables: Vec<MySqlSelectSource>,
    /// The columns the caller knows to be text, when it knows.
    ///
    /// Only the frontend can see a column's type, so a first parse renders
    /// without this and a second one, for the statements that need it, renders
    /// with it. `orders_a_bare_column` says which those are.
    text_columns: &'a [String],
    orders_a_bare_column: bool,
    compares_a_placeholder: bool,
    pub(crate) checked_comparisons: Vec<CheckedSelectComparison>,
    checked_subquery_comparisons: Vec<CheckedSubqueryComparison>,
    parameter_count: usize,
}

impl<'a> SelectRenderContext<'a> {
    pub(crate) fn new(source: &'a str, text_columns: &'a [String]) -> Self {
        Self {
            source,
            text_columns,
            subquery_tables: Vec::new(),
            orders_a_bare_column: false,
            compares_a_placeholder: false,
            checked_subquery_comparisons: Vec::new(),
            checked_comparisons: Vec::new(),
            parameter_count: 0,
        }
    }

    fn is_text_column(&self, name: &str) -> bool {
        self.text_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(name))
    }

    fn next_parameter_ordinal(&mut self) -> Result<usize, ParseError> {
        let ordinal = self.parameter_count;
        self.parameter_count =
            self.parameter_count
                .checked_add(1)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT parameter count outside usize range",
                })?;
        Ok(ordinal)
    }
}

fn render_select_item(
    item: &SelectItem,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match item {
        // The engine names a result column after the expression text, which
        // quotes an identifier. MySQL names it after the call as written, so an
        // unnamed count carries that name as an alias.
        SelectItem::UnnamedExpr(expr @ Expr::Function(function))
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some() =>
        {
            Ok(format!(
                "{} AS \"{}\"",
                render_select_expr(expr, render_context)?,
                mysql_aggregate_column_name(function).replace('"', "\"\"")
            ))
        }
        // MySQL names an unaliased call after its source text, as it does an
        // expression, so the engine's own spelling has to be aliased away.
        SelectItem::UnnamedExpr(expr @ Expr::Function(function))
            if static_select_metadata::scalar_call(function).is_some() =>
        {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT call whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        // MySQL names an unaliased expression column after the source text, so
        // `1+1` keeps its spelling where the engine would print `1 + 1`.
        SelectItem::UnnamedExpr(expr @ Expr::Case { .. })
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT CASE whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(
            expr @ (Expr::Substring { .. } | Expr::Floor { .. } | Expr::Ceil { .. }),
        ) if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT scalar expression whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(expr)
            if matches!(
                static_select_metadata::classify_static_select_expr(expr),
                Some(StaticSelectMetadata::Arithmetic(_))
            ) =>
        {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT expression whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(expr) => render_select_expr(expr, render_context),
        SelectItem::ExprWithAlias { expr, alias } => Ok(format!(
            "{} AS {}",
            render_select_expr(expr, render_context)?,
            render_ident(alias)
        )),
        SelectItem::Wildcard(options) if wildcard_options_are_empty(options) => Ok("*".to_string()),
        SelectItem::Wildcard(_) => unsupported("SELECT wildcard option"),
        _ => unsupported("SELECT projection"),
    }
}

/// Returns the name MySQL gives an unaliased aggregate column.
///
/// Measured on MySQL 8.4.11: the call as written, case included, and with the
/// argument unquoted — `COUNT(n)`, not `COUNT("n")`.
fn mysql_aggregate_column_name(function: &sqlparser::ast::Function) -> String {
    format!("{}({})", function.name, aggregate_argument(function, false))
}

/// Renders a checked aggregate call, keeping the spelling it was written with.
///
/// MySQL names the result column after the call as written, case included:
/// measured, `count(*)` keeps its lower case. The engine names it the same way
/// from this text, so nothing else has to carry the name.
fn render_aggregate_call(function: &sqlparser::ast::Function) -> String {
    format!("{}({})", function.name, aggregate_argument(function, true))
}

fn aggregate_argument(function: &sqlparser::ast::Function, quoted: bool) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked aggregate was checked to have an argument list")
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)] => {
            "*".to_owned()
        }
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => {
            if quoted {
                render_ident(column)
            } else {
                column.value.clone()
            }
        }
        _ => unreachable!("a checked aggregate was checked to take one wildcard or column"),
    }
}

fn wildcard_options_are_empty(options: &sqlparser::ast::WildcardAdditionalOptions) -> bool {
    options.opt_ilike.is_none()
        && options.opt_exclude.is_none()
        && options.opt_except.is_none()
        && options.opt_replace.is_none()
        && options.opt_rename.is_none()
        && options.opt_alias.is_none()
}

fn render_select_table(table: &TableFactor) -> Result<(String, MySqlSelectSource), ParseError> {
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = table
    else {
        return unsupported("SELECT table source");
    };
    if args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("SELECT table option");
    }
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("qualified SELECT table source");
    };
    let table = MySqlTableName::parse(&ident.value)?;
    let mut reference = ident.value.clone();
    let mut rendered = render_unqualified_name(name)?;
    if let Some(alias) = alias {
        if !alias.columns.is_empty() || alias.at.is_some() {
            return unsupported("SELECT table alias option");
        }
        reference.clone_from(&alias.name.value);
        rendered.push_str(" AS ");
        rendered.push_str(&render_ident(&alias.name));
    }
    Ok((
        rendered,
        MySqlSelectSource {
            reference,
            table,
            outer: false,
            branch: 0,
            subquery: false,
            projected_columns: Vec::new(),
        },
    ))
}

fn render_select_expr(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::Identifier(ident) => Ok(render_ident(ident)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => value
                .parse::<i64>()
                .map(|value| value.to_string())
                .map_err(|_| ParseError::Unsupported {
                    feature: "SELECT numeric literal outside signed 64-bit integer range",
                }),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            Value::Placeholder(marker) if marker == "?" => {
                render_context.next_parameter_ordinal()?;
                Ok("?".to_string())
            }
            _ => unsupported("SELECT literal"),
        },
        // The engine counts rows and non-null values the way MySQL does, so a
        // COUNT crosses without changing what it means.
        Expr::Function(function)
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some() =>
        {
            Ok(render_aggregate_call(function))
        }
        Expr::IsNull(expr) => Ok(format!(
            "({} IS NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::IsNotNull(expr) => Ok(format!(
            "({} IS NOT NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(value, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let magnitude = value.parse::<u64>().map_err(|_| ParseError::Unsupported {
                feature: "SELECT numeric literal outside signed 64-bit integer range",
            })?;
            if magnitude > (i64::MAX as u64) + 1 {
                return unsupported("SELECT numeric literal outside signed 64-bit integer range");
            }
            Ok(format!("(-{magnitude})"))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) => {
            Ok(format!("(+{})", render_select_expr(expr, render_context)?))
        }
        Expr::Nested(expr) => Ok(format!("({})", render_select_expr(expr, render_context)?)),
        Expr::Case {
            operand: None,
            conditions,
            else_result: Some(else_result),
            ..
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            let mut rendered = "CASE".to_owned();
            for when in conditions {
                rendered.push_str(" WHEN ");
                rendered.push_str(&render_select_predicate(&when.condition, render_context)?);
                rendered.push_str(" THEN ");
                rendered.push_str(&render_select_expr(&when.result, render_context)?);
            }
            rendered.push_str(" ELSE ");
            rendered.push_str(&render_select_expr(else_result, render_context)?);
            rendered.push_str(" END");
            Ok(rendered)
        }
        Expr::Substring {
            expr: target,
            substring_from: Some(substring_from),
            substring_for: Some(substring_for),
            ..
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            let target = render_select_expr(target, render_context)?;
            let from = render_select_expr(substring_from, render_context)?;
            let for_len = render_select_expr(substring_for, render_context)?;
            Ok(format!("substr({target}, {from}, {for_len})"))
        }
        Expr::Floor { expr: inner, field }
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let sqlparser::ast::CeilFloorKind::DateTimeField(
                sqlparser::ast::DateTimeField::NoDateTime,
            ) = field
            else {
                return unsupported("FLOOR option");
            };
            let inner = render_select_expr(inner, render_context)?;
            Ok(format!("CAST(floor({inner}) AS INTEGER)"))
        }
        Expr::Ceil { expr: inner, field }
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let sqlparser::ast::CeilFloorKind::DateTimeField(
                sqlparser::ast::DateTimeField::NoDateTime,
            ) = field
            else {
                return unsupported("CEIL option");
            };
            let inner = render_select_expr(inner, render_context)?;
            Ok(format!("CAST(ceil({inner}) AS INTEGER)"))
        }
        Expr::BinaryOp { left, op, right }
            if static_select_metadata::classify_arithmetic(expr).is_some() =>
        {
            let left = render_select_expr(left, render_context)?;
            let right = render_select_expr(right, render_context)?;
            // MySQL's `/` is decimal division and the engine's is integer
            // division, so `3/2` would answer 1 rather than 1.5 without this.
            if matches!(op, BinaryOperator::Divide) {
                return Ok(format!("(CAST({left} AS REAL) / {right})"));
            }
            Ok(format!(
                "({left} {} {right})",
                checked_arithmetic_sql_operator(op)
            ))
        }
        Expr::Function(function) if static_select_metadata::scalar_call(function).is_some() => {
            render_scalar_call(function, render_context)
        }
        Expr::Function(function)
            if matches!(function.name.0.as_slice(), [ObjectNamePart::Identifier(name)] if name.value.eq_ignore_ascii_case("LAST_INSERT_ID"))
                && !function.uses_odbc_syntax
                && matches!(&function.parameters, FunctionArguments::None)
                && matches!(&function.args, FunctionArguments::List(arguments) if arguments.args.is_empty() && arguments.duplicate_treatment.is_none() && arguments.clauses.is_empty())
                && function.filter.is_none()
                && function.null_treatment.is_none()
                && function.over.is_none()
                && function.within_group.is_empty() =>
        {
            Ok("last_insert_id()".to_string())
        }
        _ => unsupported("SELECT expression"),
    }
}

/// Renders a checked scalar call as the engine's own spelling of it.
///
/// MySQL's `LENGTH` counts bytes and its `CHAR_LENGTH` counts characters, which
/// the engine spells `octet_length` and `length`; the rest carry over by name.
/// `NOW()` reads the clock in UTC, which is the zone this server runs in.
fn render_scalar_call(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        unreachable!("a checked scalar call was checked to have one name");
    };
    let engine = if name.value.eq_ignore_ascii_case("LENGTH") {
        "octet_length"
    } else if name.value.eq_ignore_ascii_case("CHAR_LENGTH")
        || name.value.eq_ignore_ascii_case("CHARACTER_LENGTH")
    {
        "length"
    } else if name.value.eq_ignore_ascii_case("NOW")
        || name.value.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
    {
        return Ok("datetime('now')".to_owned());
    } else if name.value.eq_ignore_ascii_case("LOWER") {
        "lower"
    } else if name.value.eq_ignore_ascii_case("UPPER") {
        "upper"
    } else if name.value.eq_ignore_ascii_case("ABS") {
        "abs"
    } else if name.value.eq_ignore_ascii_case("ROUND") || name.value.eq_ignore_ascii_case("CEILING")
    {
        // The engine answers this as a float where MySQL answers a whole
        // number, and a float where a column promised an integer reads as an
        // overflow, so the cast is what keeps the two agreeing.
        let func = if name.value.eq_ignore_ascii_case("ROUND") {
            "round"
        } else {
            "ceil"
        };
        return Ok(format!(
            "CAST({func}({}) AS INTEGER)",
            aggregate_argument(function, true)
        ));
    } else if name.value.eq_ignore_ascii_case("IF") {
        // MySQL's `IF` is the call spelling of a two-branch `CASE`, which is
        // the shape the engine reads.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(condition)), ..] =
            arguments.args.as_slice()
        else {
            unreachable!("IF was checked to take three arguments");
        };
        return Ok(format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            render_select_predicate(condition, render_context)?,
            scalar_argument(function, 1)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("CONCAT") {
        // The engine's own `concat` skips a NULL argument where MySQL answers
        // NULL for the whole call; `||` is the operator that agrees.
        return Ok(format!(
            "({})",
            render_scalar_arguments(function)?.replace(", ", " || ")
        ));
    } else if name.value.eq_ignore_ascii_case("LEFT") {
        return Ok(format!(
            "substr({}, 1, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("RIGHT") {
        return Ok(format!(
            "substr({}, -{})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("IFNULL")
        || name.value.eq_ignore_ascii_case("COALESCE")
    {
        return Ok(format!(
            "{}({})",
            name.value.to_lowercase(),
            render_scalar_arguments(function)?
        ));
    } else {
        unreachable!("a checked scalar call was already recognized");
    };
    Ok(format!("{engine}({})", aggregate_argument(function, true)))
}

/// Renders one argument of a checked call by position.
fn scalar_argument(
    function: &sqlparser::ast::Function,
    index: usize,
) -> Result<String, ParseError> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr))) =
        arguments.args.get(index)
    else {
        return unsupported("SELECT call argument");
    };
    match expr {
        Expr::Identifier(column) => Ok(render_ident(column)),
        _ => render_dml_expr(expr),
    }
}

/// Renders every argument of a checked call, which only the two-argument
/// forms need.
fn render_scalar_arguments(function: &sqlparser::ast::Function) -> Result<String, ParseError> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    arguments
        .args
        .iter()
        .map(|argument| {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                argument
            else {
                return unsupported("SELECT call argument");
            };
            match expr {
                Expr::Identifier(column) => Ok(render_ident(column)),
                _ => render_dml_expr(expr),
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|arguments| arguments.join(", "))
}

fn checked_arithmetic_sql_operator(operator: &BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Plus => "+",
        BinaryOperator::Minus => "-",
        BinaryOperator::Multiply => "*",
        _ => unreachable!("a checked arithmetic operator was already recognized"),
    }
}

/// Returns the statement text one expression was written with.
///
/// sqlparser reports a span in 1-based line and column numbers, and drops the
/// parentheses around a nested expression, so both have to be undone here to
/// recover what the client actually typed.
fn source_text(source: &str, expr: &Expr) -> Option<String> {
    use sqlparser::ast::Spanned;
    let span = expr.span();
    let start = byte_offset(source, span.start)?;
    let end = byte_offset(source, span.end)?;
    if start > end || end > source.len() {
        return None;
    }
    let (mut start, mut end) = (start, end);
    let bytes = source.as_bytes();
    // A call's span covers its name and arguments but not its closing
    // parenthesis, and a CASE's stops before its END; MySQL's own name for the
    // column includes both.
    if matches!(
        expr,
        Expr::Substring { .. } | Expr::Floor { .. } | Expr::Ceil { .. }
    ) {
        let open_paren = bytes[..start].iter().rposition(|byte| *byte == b'(')?;
        let name_end = bytes[..open_paren]
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())?;
        let name_start = bytes[..name_end]
            .iter()
            .rposition(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'))
            .map_or(0, |pos| pos + 1);
        start = name_start;
    }
    if matches!(
        expr,
        Expr::Function(_) | Expr::Substring { .. } | Expr::Floor { .. } | Expr::Ceil { .. }
    ) && !source.get(start..end)?.trim_end().ends_with(')')
    {
        let closing = bytes[end..].iter().position(|byte| *byte == b')')? + end;
        end = closing + 1;
    }
    if matches!(expr, Expr::Case { .. })
        && !source
            .get(start..end)?
            .trim_end()
            .to_ascii_uppercase()
            .ends_with("END")
    {
        let tail = source.get(end..)?;
        let offset = tail.to_ascii_uppercase().find("END")?;
        end += offset + "END".len();
    }
    let mut depth = nested_depth(expr);
    while depth > 0 {
        let opening = bytes[..start]
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())?;
        let closing = bytes[end..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())?
            + end;
        if bytes[opening] != b'(' || bytes[closing] != b')' {
            return None;
        }
        start = opening;
        end = closing + 1;
        depth -= 1;
    }
    source.get(start..end).map(str::to_owned)
}

fn nested_depth(expr: &Expr) -> usize {
    match expr {
        Expr::Nested(inner) => 1 + nested_depth(inner),
        _ => 0,
    }
}

fn byte_offset(source: &str, location: sqlparser::tokenizer::Location) -> Option<usize> {
    let mut line = 1;
    let mut column = 1;
    for (offset, character) in source.char_indices() {
        if line == location.line && column == location.column {
            return Some(offset);
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line == location.line && column == location.column).then_some(source.len())
}

fn render_select_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::IsNull(expr) => Ok(format!(
            "({} IS NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::IsNotNull(expr) => Ok(format!(
            "({} IS NOT NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_select_predicate(left, render_context)?,
                render_select_predicate(right, render_context)?
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => render_checked_like(
            *negated,
            *any,
            expr,
            pattern,
            escape_char.as_ref(),
            render_context,
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_select_predicate(expr, render_context)?
        )),
        Expr::Nested(expr) => Ok(format!(
            "({})",
            render_select_predicate(expr, render_context)?
        )),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => {
            render_select_expr(expr, render_context)
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => render_in_subquery(expr, subquery, *negated, render_context),
        Expr::InList {
            expr,
            list,
            negated,
        } => render_checked_in_list(expr, list, *negated, render_context),
        Expr::Exists { subquery, negated } => {
            let (rendered, _) = render_subquery(subquery, render_context)?;
            Ok(format!(
                "({}EXISTS ({rendered}))",
                if *negated { "NOT " } else { "" }
            ))
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => render_checked_between(*negated, expr, low, high, render_context),
        _ => unsupported("SELECT WHERE predicate before coercion calibration"),
    }
}

fn reverse_checked_comparison_operator(op: &BinaryOperator) -> Option<BinaryOperator> {
    match op {
        BinaryOperator::Eq => Some(BinaryOperator::Eq),
        BinaryOperator::NotEq => Some(BinaryOperator::NotEq),
        BinaryOperator::Lt => Some(BinaryOperator::Gt),
        BinaryOperator::LtEq => Some(BinaryOperator::GtEq),
        BinaryOperator::Gt => Some(BinaryOperator::Lt),
        BinaryOperator::GtEq => Some(BinaryOperator::LtEq),
        _ => None,
    }
}

/// Renders `col IN (a, b)`, which MySQL answers by comparing the column
/// against each member under the column's own collation.
///
/// Measured on MySQL 8.4.11 over rows (1,'b'), (2,'A'), (3,'c'):
/// `name IN ('a','C')` answers 2 and 3, so a text list ignores case the way a
/// text `=` does. `id NOT IN (1, NULL)` answers nothing, which is ordinary
/// three-valued logic and what the engine already does.
///
/// Every member is recorded as its own checked comparison, so the frontend
/// holds each one to the column's type exactly as it holds a single `=`.
fn render_checked_in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let Expr::Identifier(column) = expr else {
        return unsupported("SELECT IN requires one unqualified column");
    };
    if list.is_empty() {
        return unsupported("SELECT IN over an empty list");
    }
    let column_name = column.value.clone();
    let mut members = Vec::with_capacity(list.len());
    for element in list {
        members.push(render_checked_select_comparison_rhs(element, render_context)?);
    }
    // One text member collates the whole list, because MySQL compares every
    // member under the column's collation rather than each member's own. A `?`
    // carries no type until it is bound, so it is collated only once the
    // caller has said the column is text.
    let collated = members.iter().any(|(_, rhs)| match rhs {
        CheckedSelectComparisonRhs::Text(_) => true,
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            render_context.is_text_column(&column_name)
        }
        _ => false,
    });
    if members
        .iter()
        .any(|(_, rhs)| matches!(rhs, CheckedSelectComparisonRhs::Placeholder { .. }))
    {
        render_context.compares_a_placeholder = true;
    }
    let operator = if negated {
        CheckedSelectComparisonOperator::NotIn
    } else {
        CheckedSelectComparisonOperator::In
    };
    let rendered = format!(
        "({}{} {}IN ({}))",
        render_ident(column),
        if collated { " COLLATE NOCASE" } else { "" },
        if negated { "NOT " } else { "" },
        members
            .iter()
            .map(|(rendered, _)| rendered.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (_, rhs) in members {
        render_context
            .checked_comparisons
            .push(CheckedSelectComparison {
                column_name: column_name.clone(),
                operator,
                rhs,
                collated,
            });
    }
    Ok(rendered)
}

fn render_checked_between(
    negated: bool,
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if !matches!(expr, Expr::Identifier(_)) {
        return unsupported("BETWEEN requires an unqualified column as its subject");
    }
    let lower = render_checked_select_comparison(expr, &BinaryOperator::GtEq, low, render_context)?;
    let upper =
        render_checked_select_comparison(expr, &BinaryOperator::LtEq, high, render_context)?;
    let condition = format!("({lower} AND {upper})");
    if negated {
        Ok(format!("(NOT {condition})"))
    } else {
        Ok(condition)
    }
}

fn render_checked_select_comparison(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let (column, op_reversed, rhs_expr) = match (left, right) {
        (Expr::Identifier(column), _) => (column, op.clone(), right),
        (_, Expr::Identifier(column)) => {
            let reversed =
                reverse_checked_comparison_operator(op).ok_or(ParseError::Unsupported {
                    feature: "reversed SELECT comparison operator",
                })?;
            (column, reversed, left)
        }
        _ => return unsupported("SELECT comparison requires one unqualified column"),
    };
    let column_name = column.value.clone();
    let (rendered_rhs, rhs) = render_checked_select_comparison_rhs(rhs_expr, render_context)?;
    let operator =
        checked_select_comparison_operator(&op_reversed).expect("comparison operator guard");
    // MySQL's default collation ignores case, so a text comparison asks the
    // engine for NOCASE rather than its byte order. This is left off every
    // other comparison because a collation the index does not carry stops the
    // planner from using it, and an integer comparison gains nothing from it.
    // A `?` carries no type of its own, so it is collated when the caller has
    // said the column is text.
    let collated = match rhs {
        CheckedSelectComparisonRhs::Text(_) => true,
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            render_context.compares_a_placeholder = true;
            render_context.is_text_column(&column_name)
        }
        _ => false,
    };
    let collation = if collated { " COLLATE NOCASE" } else { "" };
    let rendered = format!(
        "({}{collation} {} {rendered_rhs})",
        render_ident(column),
        checked_select_comparison_sql_operator(&op_reversed)
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            column_name,
            operator,
            rhs,
            collated,
        });
    Ok(rendered)
}

/// Renders a `LIKE` against one column, which the engine already matches the
/// way MySQL's default collation does: both ignore ASCII case.
fn render_checked_like(
    negated: bool,
    any: bool,
    expr: &Expr,
    pattern: &Expr,
    escape_char: Option<&sqlparser::ast::ValueWithSpan>,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if any || escape_char.is_some() {
        return unsupported("SELECT LIKE option");
    }
    let Expr::Identifier(column) = expr else {
        return unsupported("SELECT LIKE requires one unqualified column");
    };
    let Expr::Value(value) = pattern else {
        return unsupported("SELECT LIKE requires a string pattern");
    };
    let (Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)) = &value.value else {
        return unsupported("SELECT LIKE requires a string pattern");
    };
    // MySQL takes a backslash in a pattern as an escape and the engine takes it
    // literally, so a pattern that contains one would match different rows.
    if text.contains('\\') {
        return unsupported("SELECT LIKE pattern with a backslash");
    }
    let rendered = format!(
        "({} {}LIKE '{}')",
        render_ident(column),
        if negated { "NOT " } else { "" },
        text.replace('\'', "''")
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            column_name: column.value.clone(),
            operator: if negated {
                CheckedSelectComparisonOperator::NotLike
            } else {
                CheckedSelectComparisonOperator::Like
            },
            rhs: CheckedSelectComparisonRhs::Text(text.clone()),
            collated: false,
        });
    Ok(rendered)
}

fn render_checked_select_comparison_rhs(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, CheckedSelectComparisonRhs), ParseError> {
    match expr {
        Expr::Nested(expr) => {
            let (rendered, rhs) = render_checked_select_comparison_rhs(expr, render_context)?;
            Ok((format!("({rendered})"), rhs))
        }
        Expr::Value(value) => match &value.value {
            Value::Number(number, false) => {
                let value = number.parse::<i64>().map_err(|_| ParseError::Unsupported {
                    feature: "SELECT comparison literal outside signed 64-bit integer range",
                })?;
                Ok((
                    value.to_string(),
                    CheckedSelectComparisonRhs::SignedInteger(value),
                ))
            }
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Ok((
                format!("'{}'", text.replace('\'', "''")),
                CheckedSelectComparisonRhs::Text(text.clone()),
            )),
            Value::Null => Ok(("NULL".to_string(), CheckedSelectComparisonRhs::Null)),
            Value::Placeholder(marker) if marker == "?" => {
                let ordinal = render_context.next_parameter_ordinal()?;
                Ok((
                    "?".to_string(),
                    CheckedSelectComparisonRhs::Placeholder { ordinal },
                ))
            }
            _ => unsupported(
                "SELECT comparison requires an exact signed integer, a string, NULL, or ?",
            ),
        },
        Expr::UnaryOp { op, expr }
            if matches!(op, UnaryOperator::Minus | UnaryOperator::Plus)
                && matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(number, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let magnitude = number.parse::<u64>().map_err(|_| ParseError::Unsupported {
                feature: "SELECT comparison literal outside signed 64-bit integer range",
            })?;
            let value = if matches!(op, UnaryOperator::Minus) {
                if magnitude > (i64::MAX as u64) + 1 {
                    return unsupported(
                        "SELECT comparison literal outside signed 64-bit integer range",
                    );
                }
                if magnitude == (i64::MAX as u64) + 1 {
                    i64::MIN
                } else {
                    -(magnitude as i64)
                }
            } else {
                i64::try_from(magnitude).map_err(|_| ParseError::Unsupported {
                    feature: "SELECT comparison literal outside signed 64-bit integer range",
                })?
            };
            Ok((
                if matches!(op, UnaryOperator::Minus) {
                    format!("(-{magnitude})")
                } else {
                    format!("(+{magnitude})")
                },
                CheckedSelectComparisonRhs::SignedInteger(value),
            ))
        }
        _ => {
            unsupported("SELECT comparison requires an exact signed integer, a string, NULL, or ?")
        }
    }
}

fn is_checked_select_comparison_operator(operator: &BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
}

fn checked_select_comparison_operator(
    operator: &BinaryOperator,
) -> Option<CheckedSelectComparisonOperator> {
    Some(match operator {
        BinaryOperator::Eq => CheckedSelectComparisonOperator::Equal,
        BinaryOperator::NotEq => CheckedSelectComparisonOperator::NotEqual,
        BinaryOperator::Lt => CheckedSelectComparisonOperator::LessThan,
        BinaryOperator::LtEq => CheckedSelectComparisonOperator::LessThanOrEqual,
        BinaryOperator::Gt => CheckedSelectComparisonOperator::GreaterThan,
        BinaryOperator::GtEq => CheckedSelectComparisonOperator::GreaterThanOrEqual,
        _ => return None,
    })
}

fn checked_select_comparison_sql_operator(operator: &BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "<>",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        _ => unreachable!("comparison operator guard"),
    }
}

pub(crate) fn render_simple_view_query(
    query: &sqlparser::ast::Query,
) -> Result<String, ParseError> {
    if query.with.is_some()
        || query.order_by.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("CREATE VIEW query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("CREATE VIEW query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || select.selection.is_some()
        || !select.connect_by.is_empty()
        || !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("CREATE VIEW SELECT feature");
    }
    let [from] = select.from.as_slice() else {
        return unsupported("CREATE VIEW FROM clause");
    };
    if !from.joins.is_empty() {
        return unsupported("CREATE VIEW JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("CREATE VIEW table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("CREATE VIEW table option");
    }
    let table_name = render_unqualified_name(name)?;
    let columns = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(Expr::Identifier(column)) => Ok(render_ident(column)),
            _ => unsupported("CREATE VIEW projection"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE VIEW without projections");
    }
    Ok(format!("SELECT {} FROM {table_name}", columns.join(", ")))
}
