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
    pub(crate) orders_wildcard_ordinal: bool,
    pub(crate) compares_a_placeholder: bool,
    pub(crate) counts_distinct_column: bool,
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
    table_columns: &[String],
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
    let mut render_context = SelectRenderContext::new(sql, text_columns, table_columns);
    let (mut prefix, mut cte_tables) = (String::new(), Vec::new());
    if let Some(with) = &query.with {
        let (rendered, sources) = render_common_table_expressions(with, &mut render_context)?;
        prefix = rendered;
        cte_tables = sources;
    }
    // An `ORDER BY` ordinal names a projected column, so the projection has to
    // outlive the body that rendered it. A compound query orders by its first
    // branch.
    let ordered_projection: &[SelectItem] = match query.body.as_ref() {
        SetExpr::Select(select) => &select.projection,
        SetExpr::SetOperation { left, .. } => match unwrap_select_body(left.as_ref()) {
            Ok(select) => &select.projection,
            Err(_) => &[],
        },
        _ => &[],
    };
    let (mut normalized, mut source_tables) = match query.body.as_ref() {
        SetExpr::Select(select) => render_select_body(select, &mut render_context)?,
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            // MySQL's EXCEPT and INTERSECT arrived in 8.0.31 and answer rows a
            // UNION does not. Only their DISTINCT forms are taken: measured on
            // MySQL 8.4.11 over rows (1), (1), (2) against (2), `EXCEPT`
            // answers one 1 and `EXCEPT ALL` answers two, and the engine has no
            // spelling for the second, so it is refused rather than collapsed
            // into the first.
            let keyword = match (op, set_quantifier) {
                (
                    sqlparser::ast::SetOperator::Union,
                    sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
                ) => "UNION",
                (sqlparser::ast::SetOperator::Union, sqlparser::ast::SetQuantifier::All) => {
                    "UNION ALL"
                }
                (
                    sqlparser::ast::SetOperator::Except,
                    sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
                ) => "EXCEPT",
                (
                    sqlparser::ast::SetOperator::Intersect,
                    sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
                ) => "INTERSECT",
                _ => return unsupported("SELECT set operation"),
            };
            let (left, right) = (
                unwrap_select_body(left.as_ref())?,
                unwrap_select_body(right.as_ref())?,
            );
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
    for comparison in &render_context.checked_comparisons {
        if let Some(qualifier) = comparison.qualifier() {
            let non_subquery_sources: Vec<_> = source_tables
                .iter()
                .filter(|source| !source.subquery)
                .collect();
            match non_subquery_sources.as_slice() {
                [source] if qualifier.eq_ignore_ascii_case(source.reference()) => {}
                _ => {
                    return unsupported(
                        "SELECT comparison qualifier must match the single table reference",
                    );
                }
            }
        }
    }
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
        orders_wildcard_ordinal: render_context.orders_wildcard_ordinal,
        compares_a_placeholder: render_context.compares_a_placeholder,
        counts_distinct_column: render_context.counts_distinct_column,
        checked_subquery_comparisons: render_context.checked_subquery_comparisons,
        source_table,
        source_tables,
        checked_comparisons: render_context.checked_comparisons,
        parameter_count: render_context.parameter_count,
    })
}

/// Unwraps parenthesised query wrappers around a compound branch, refusing any
/// branch that carries options like `ORDER BY` or `LIMIT` that cannot be
/// flattened into the set operation.
fn unwrap_select_body(expr: &SetExpr) -> Result<&sqlparser::ast::Select, ParseError> {
    match expr {
        SetExpr::Select(select) => Ok(select),
        SetExpr::Query(query) => {
            if query.fetch.is_some()
                || !query.locks.is_empty()
                || query.for_clause.is_some()
                || query.settings.is_some()
                || query.format_clause.is_some()
                || !query.pipe_operators.is_empty()
                || query.with.is_some()
                || query.order_by.is_some()
                || query.limit_clause.is_some()
            {
                return unsupported("compound branch query clause");
            }
            unwrap_select_body(query.body.as_ref())
        }
        _ => unsupported("SELECT set operation branch"),
    }
}

/// Renders one `SELECT` body, which is either the whole statement or one
/// branch of a `UNION`.
fn render_select_body(
    select: &sqlparser::ast::Select,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    // A `WINDOW w AS (...)` names a window the calls then reach for by name.
    // Writing each call's window out where it stands is what it means, and it
    // leaves every check and every rendering below reading one shape.
    let resolved;
    let select = if select.named_window.is_empty() {
        select
    } else {
        resolved = resolve_named_windows(select)?;
        &resolved
    };
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

    let mut merged_columns = Vec::new();
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
                match constraint {
                    CheckedJoinConstraint::On(expr) => {
                        rendered.push_str(" ON ");
                        rendered.push_str(&render_join_predicate(expr)?);
                    }
                    CheckedJoinConstraint::Using(columns) => {
                        rendered.push_str(" USING (");
                        rendered.push_str(&render_join_using(columns, &mut merged_columns)?);
                        rendered.push(')');
                    }
                    CheckedJoinConstraint::Everything => {}
                }
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
        reject_unqualified_join_projection(&select.projection, &merged_columns)?;
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
            // MySQL reads a HAVING with no GROUP BY over one implicit group of
            // every row, and the engine answers the same. Measured on MySQL
            // 8.4.11 over three rows: `SELECT COUNT(*) FROM t HAVING
            // COUNT(*) > 1` answers 3 and `... > 5` answers no rows at all.
            //
            // What MySQL refuses is a bare column once the statement is
            // aggregated: 1140 for one in the projection, 1054 for one in the
            // HAVING. Both are refused here too, so only aggregates and
            // literals reach the engine.
            for item in &select.projection {
                let projected = match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                    _ => {
                        return unsupported("HAVING without a GROUP BY over a wildcard projection")
                    }
                };
                if !aggregates_or_literals_only(projected) {
                    return unsupported("HAVING without a GROUP BY over an ungrouped column");
                }
            }
            if !aggregates_or_literals_only(having) {
                return unsupported("HAVING without a GROUP BY naming an ungrouped column");
            }
        }
        normalized.push_str(" HAVING ");
        normalized.push_str(&render_having_predicate(having, render_context)?);
    }
    Ok((normalized, source_tables))
}

/// Measures the `OVER ...` that follows a windowed call's arguments.
///
/// A call's span stops at its arguments, and MySQL names the column after the
/// whole call, `OVER` and all — which is either a window written out in
/// parentheses or the name of one.
fn window_clause_len(tail: &str) -> Option<usize> {
    let over = tail.to_ascii_uppercase().find("OVER")?;
    let rest = &tail[over + "OVER".len()..];
    let named = rest.len() - rest.trim_start().len();
    let bytes = rest.as_bytes();
    if bytes.get(named) == Some(&b'(') {
        let mut depth = 0usize;
        for (offset, byte) in rest.bytes().enumerate().skip(named) {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(over + "OVER".len() + offset + 1);
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    let quote = bytes.get(named).copied().filter(|byte| *byte == b'`');
    let name_len = match quote {
        Some(quote) => rest[named + 1..]
            .bytes()
            .position(|byte| byte == quote)
            .map(|len| len + 2)?,
        None => rest[named..]
            .bytes()
            .position(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'))
            .unwrap_or(rest.len() - named),
    };
    (name_len > 0).then_some(over + "OVER".len() + named + name_len)
}

/// Writes each `OVER <name>` out as the window that name stands for.
///
/// A name that stands for another name, and a window written on top of a named
/// one — `w AS (base ORDER BY ...)` — are refused: both are a second spelling
/// of the same thing, and one shape is enough to check.
fn resolve_named_windows(
    select: &sqlparser::ast::Select,
) -> Result<sqlparser::ast::Select, ParseError> {
    use sqlparser::ast::{NamedWindowExpr, WindowType};
    let mut windows = Vec::with_capacity(select.named_window.len());
    for definition in &select.named_window {
        let NamedWindowExpr::WindowSpec(spec) = &definition.1 else {
            return unsupported("WINDOW naming another window");
        };
        if spec.window_name.is_some() {
            return unsupported("WINDOW built on another window");
        }
        windows.push((definition.0.value.clone(), spec.clone()));
    }
    let mut resolved = select.clone();
    resolved.named_window.clear();
    // Only a note on where the clause was written, and there is no clause left.
    resolved.window_before_qualify = false;
    for item in &mut resolved.projection {
        let (SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) = item else {
            continue;
        };
        let Expr::Function(function) = expr else {
            continue;
        };
        let Some(WindowType::NamedWindow(name)) = function.over.as_ref() else {
            continue;
        };
        let Some((_, spec)) = windows
            .iter()
            .find(|(defined, _)| defined.eq_ignore_ascii_case(&name.value))
        else {
            return unsupported("OVER an undefined window name");
        };
        function.over = Some(WindowType::WindowSpec(spec.clone()));
    }
    Ok(resolved)
}

/// Reads the join keyword and what a checked join matches on.
///
/// Only an `ON` that equates whole columns, or a `USING` naming whole columns,
/// is taken. The two engines agree about a column-to-column equality without
/// any coercion question, which is what makes a join crossable while a literal
/// comparison still goes through the checked path.
fn checked_join(
    operator: &sqlparser::ast::JoinOperator,
) -> Result<(&'static str, CheckedJoinConstraint<'_>), ParseError> {
    use sqlparser::ast::{JoinConstraint, JoinOperator};
    let (keyword, constraint) = match operator {
        // A `CROSS JOIN` is the one join that matches on nothing at all.
        JoinOperator::CrossJoin(JoinConstraint::None) => {
            return Ok(("CROSS JOIN", CheckedJoinConstraint::Everything));
        }
        JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => ("JOIN", constraint),
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            ("LEFT JOIN", constraint)
        }
        JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint) => {
            ("RIGHT JOIN", constraint)
        }
        _ => return unsupported("SELECT JOIN form"),
    };
    match constraint {
        JoinConstraint::On(expr) => Ok((keyword, CheckedJoinConstraint::On(expr))),
        JoinConstraint::Using(columns) => Ok((keyword, CheckedJoinConstraint::Using(columns))),
        _ => unsupported("SELECT JOIN form"),
    }
}

/// What a checked join matches its two tables on.
enum CheckedJoinConstraint<'a> {
    On(&'a Expr),
    Using(&'a [sqlparser::ast::ObjectName]),
    /// A `CROSS JOIN`, which matches every row of one table against every row
    /// of the other.
    Everything,
}

/// Renders a `USING` list, and collects the names it merges.
///
/// Both engines merge the named column into one result column, so the engine's
/// own `USING` is what gets written. A merged name is the one unqualified name
/// a joined projection may carry, so each is collected for that check.
fn render_join_using(
    columns: &[sqlparser::ast::ObjectName],
    merged: &mut Vec<String>,
) -> Result<String, ParseError> {
    use sqlparser::ast::ObjectNamePart;
    if columns.is_empty() {
        return unsupported("SELECT JOIN USING without a column");
    }
    let mut rendered = Vec::with_capacity(columns.len());
    for column in columns {
        let [ObjectNamePart::Identifier(ident)] = column.0.as_slice() else {
            return unsupported("SELECT JOIN USING requires a plain column name");
        };
        rendered.push(render_ident(ident));
        merged.push(ident.value.clone());
    }
    Ok(rendered.join(", "))
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
/// that a join names its tables. What a `USING` merges is the exception.
fn reject_unqualified_join_projection(
    projection: &[SelectItem],
    merged_columns: &[String],
) -> Result<(), ParseError> {
    for item in projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            _ => continue,
        };
        let Expr::Identifier(ident) = expr else {
            continue;
        };
        // A name a `USING` merges stands for one column in the result, so it is
        // not ambiguous and needs no table.
        if !merged_columns
            .iter()
            .any(|merged| merged.eq_ignore_ascii_case(&ident.value))
        {
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
    // Two subqueries over the same table are still one table to authorize and
    // to look a column up in, so the source is recorded once. Recording it
    // twice would make a column name look ambiguous where it is not.
    for source in sources {
        let already = render_context.subquery_tables.iter().any(|held| {
            held.reference == source.reference
                && held.table == source.table
                && held.projected_columns == source.projected_columns
        });
        if !already {
            render_context.subquery_tables.push(source);
        }
    }
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
/// Answers whether an expression is built only from aggregate calls and
/// literals, with no column of its own.
///
/// A statement carrying a `HAVING` and no `GROUP BY` is aggregated over one
/// implicit group, and a bare column has no single row to come from. MySQL
/// says so with 1140 for one in the projection and 1054 for one in the
/// `HAVING`, so neither is let through.
fn aggregates_or_literals_only(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => {
            static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some()
        }
        Expr::Value(_) => true,
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            aggregates_or_literals_only(inner)
        }
        Expr::BinaryOp { left, right, .. } => {
            aggregates_or_literals_only(left) && aggregates_or_literals_only(right)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            aggregates_or_literals_only(expr)
                && aggregates_or_literals_only(low)
                && aggregates_or_literals_only(high)
        }
        _ => false,
    }
}

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
                        qualifier: None,
                        column_name: column.value.clone(),
                        operator: checked_select_comparison_operator(op)
                            .expect("comparison operator guard"),
                        rhs,
                        collated: false,
                    });
            }
            Ok(format!(
                "({} {} {rendered_right})",
                render_aggregate_call(function, render_context),
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
    // A call reaching for a window by name is the same call with the window
    // written out, and this reads the same shape out of either. A statement
    // whose names do not resolve is refused where it is rendered.
    let resolved = resolve_named_windows(select);
    let select = match &resolved {
        Ok(resolved) if !select.named_window.is_empty() => resolved,
        _ => select,
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
    let is_pure_wildcard = matches!(
        projection,
        [SelectItem::Wildcard(options)] if wildcard_options_are_empty(options)
    );
    let has_wildcard = projection.iter().any(|item| {
        matches!(
            item,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
        )
    });
    expressions
        .iter()
        .map(|expression| {
            if expression.options.nulls_first.is_some() || expression.with_fill.is_some() {
                return unsupported("SELECT ORDER BY option");
            }
            let direction = if expression.options.asc == Some(false) {
                "DESC"
            } else {
                "ASC"
            };
            if let Some(ordinal) = order_by_ordinal(&expression.expr) {
                if ordinal == 0 {
                    return unsupported("SELECT ORDER BY ordinal outside the projection");
                }
                if has_wildcard {
                    if !is_pure_wildcard {
                        return unsupported(
                            "SELECT ORDER BY an ordinal over a wildcard projection",
                        );
                    }
                    if render_context.table_columns.is_empty() {
                        render_context.orders_wildcard_ordinal = true;
                        return Ok(format!("{ordinal} {direction}"));
                    }
                    if ordinal > render_context.table_columns.len() {
                        return unsupported("SELECT ORDER BY ordinal outside the projection");
                    }
                    render_context.orders_wildcard_ordinal = true;
                    render_context.orders_a_bare_column = true;
                    let column_name = &render_context.table_columns[ordinal - 1];
                    let collation = if render_context.is_text_column(column_name) {
                        " COLLATE NOCASE"
                    } else {
                        ""
                    };
                    return Ok(format!(
                        "{}{collation} {direction}",
                        render_ident_str(column_name)
                    ));
                }
                let expr = projected_expr(projection, ordinal)?;
                return render_order_by_expr(expr, direction, render_context);
            }
            render_order_by_expr(&expression.expr, direction, render_context)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|expressions| expressions.join(", "))
}

fn render_order_by_expr(
    expr: &Expr,
    direction: &str,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::Identifier(_) => {}
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {}
        Expr::Function(function)
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some() => {}
        Expr::BinaryOp { .. } if static_select_metadata::classify_arithmetic(expr).is_some() => {}
        _ => return unsupported("SELECT ORDER BY expression"),
    }
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

/// What one checked `INSERT` renders to.
pub(crate) struct RenderedInsert {
    pub(crate) sqlite_sql: String,
    /// Every table the statement reads, which an `INSERT ... SELECT` has and
    /// the `VALUES` forms do not.
    pub(crate) read_tables: Vec<MySqlSelectSource>,
    /// The table the `SELECT`'s own `WHERE` compares against, when there is one.
    pub(crate) compared_table: Option<String>,
    /// The comparisons that `WHERE` recorded, to be held to the column's type
    /// exactly as a `SELECT`'s are.
    pub(crate) checked_comparisons: Vec<CheckedSelectComparison>,
}

pub(crate) fn translate_insert(insert: &Insert, sql: &str) -> Result<RenderedInsert, ParseError> {
    if !insert.optimizer_hints.is_empty()
        || insert.or.is_some()
        // MySQL's REPLACE already decides what a collision does, so IGNORE on
        // top of it is not a shape it accepts either.
        || (insert.ignore && insert.replace_into)
        || !insert.into
        || insert.table_alias.is_some()
        || insert.overwrite
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.has_table_keyword
        // MySQL's own ON DUPLICATE KEY UPDATE is read below; the ON CONFLICT
        // spelling is the engine's, not something a MySQL client writes.
        || matches!(insert.on, Some(sqlparser::ast::OnInsert::OnConflict(_)))
        // REPLACE and IGNORE already decide what a collision does, so an
        // upsert on top of either is not a shape MySQL accepts.
        || (insert.on.is_some() && (insert.replace_into || insert.ignore))
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
    // MySQL's `INSERT ... SET a = 1, b = 2` names its columns and values in one
    // place instead of two, and means exactly what the column-list form means.
    // Measured on MySQL 8.4.11: `INSERT INTO s SET id = 1, a = 2, b = 'x'`
    // stores the same row `INSERT INTO s (id, a, b) VALUES (1, 2, 'x')` does,
    // and a column the SET leaves out takes its default. Rendering it as the
    // other form is what keeps one set of rules for both.
    if !insert.assignments.is_empty() {
        return render_insert_assignments(&table, insert);
    }
    let columns = insert
        .columns
        .iter()
        .map(render_unqualified_name)
        .collect::<Result<Vec<_>, _>>()?;
    let verb = insert_verb(insert);
    let source = insert.source.as_deref().ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES",
    })?;
    // `INSERT ... SELECT` reads rows rather than listing them. The SELECT goes
    // through the same translator a bare one does, so it is held to the same
    // rules and names the same tables — which the caller has to see, or the
    // table it reads goes unauthorized.
    if !matches!(source.body.as_ref(), SetExpr::Values(_)) {
        if columns.is_empty() {
            return unsupported("INSERT SELECT without an explicit column list");
        }
        let rendered = translate_select_query(source, sql, &[], &[])?;
        // A SELECT that needs a second rendering pass to learn its column types
        // has no way to ask for one from here, so it is refused rather than
        // rendered from the first pass alone.
        if rendered.orders_a_bare_column || rendered.compares_a_placeholder {
            return unsupported("INSERT SELECT needing column types");
        }
        if !rendered.checked_subquery_comparisons.is_empty() {
            return unsupported("INSERT SELECT with a subquery comparison");
        }
        return Ok(RenderedInsert {
            sqlite_sql: format!(
                "{verb} {table} ({}) {}",
                columns.join(", "),
                rendered.sqlite_sql
            ),
            read_tables: rendered.source_tables,
            compared_table: rendered.source_table.map(|table| table.as_str().to_owned()),
            checked_comparisons: rendered.checked_comparisons,
        });
    }
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
    if columns.is_empty() {
        if values.rows.len() == 1 && values.rows[0].is_empty() {
            // The empty-row form writes DEFAULT VALUES, which has no room for
            // an upsert clause after it. Refusing keeps the clause from being
            // dropped on the floor.
            if insert.on.is_some() {
                return unsupported("INSERT DEFAULT VALUES with ON DUPLICATE KEY UPDATE");
            }
            return Ok(RenderedInsert {
                sqlite_sql: format!("{verb} {table} DEFAULT VALUES"),
                read_tables: Vec::new(),
                compared_table: None,
                checked_comparisons: Vec::new(),
            });
        }
        return unsupported("INSERT without an explicit column list");
    }
    reject_ignored_null(
        insert,
        &values
            .rows
            .iter()
            .flat_map(|row| row.iter())
            .collect::<Vec<_>>(),
    )?;
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
    Ok(RenderedInsert {
        sqlite_sql: format!(
            "{verb} {table} ({}) VALUES {}{}",
            columns.join(", "),
            rows.join(", "),
            render_duplicate_key_update(insert)?
        ),
        read_tables: Vec::new(),
        compared_table: None,
        checked_comparisons: Vec::new(),
    })
}

/// Renders MySQL's `ON DUPLICATE KEY UPDATE` as the engine's `ON CONFLICT DO
/// UPDATE`.
///
/// The two mean the same thing. MySQL's fires on a collision with any unique
/// key, and the engine's, written without a conflict target, does too.
/// `VALUES(col)` names the value the row would have been given, which the
/// engine spells `excluded.col`.
///
/// The affected count is the one thing that differs, and it is measured on
/// MySQL 8.4.11: a new row counts 1, a row the update changes counts 2 because
/// MySQL counts the attempted insert and the update, and a row the update
/// leaves identical counts 0. The engine counts the changed row once, so the
/// middle case reports 1 here.
fn render_duplicate_key_update(insert: &Insert) -> Result<String, ParseError> {
    let Some(on) = &insert.on else {
        return Ok(String::new());
    };
    let sqlparser::ast::OnInsert::DuplicateKeyUpdate(assignments) = on else {
        return unsupported("INSERT ON CONFLICT clause");
    };
    if assignments.is_empty() {
        return unsupported("INSERT ON DUPLICATE KEY UPDATE without assignments");
    }
    let mut rendered = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(name) = &assignment.target else {
            return unsupported("INSERT ON DUPLICATE KEY UPDATE assignment target");
        };
        rendered.push(format!(
            "{} = {}",
            render_unqualified_name(name)?,
            render_duplicate_key_value(&assignment.value)?
        ));
    }
    Ok(format!(" ON CONFLICT DO UPDATE SET {}", rendered.join(", ")))
}

/// Renders one `ON DUPLICATE KEY UPDATE` value.
///
/// `VALUES(col)` is MySQL's way of naming the value the row would have carried;
/// the engine names the same thing `excluded.col`. Everything else goes through
/// the rules an ordinary DML value goes through.
fn render_duplicate_key_value(value: &Expr) -> Result<String, ParseError> {
    if let Expr::Function(function) = value {
        let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
            return unsupported("INSERT ON DUPLICATE KEY UPDATE value");
        };
        if !name.value.eq_ignore_ascii_case("VALUES") || name.quote_style.is_some() {
            return unsupported("INSERT ON DUPLICATE KEY UPDATE value");
        }
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            return unsupported("INSERT ON DUPLICATE KEY UPDATE value");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] = arguments.args.as_slice()
        else {
            return unsupported("VALUES() requires one unqualified column");
        };
        return Ok(format!("\"excluded\".{}", render_ident(column)));
    }
    render_dml_expr(value)
}

/// Renders `INSERT ... SET a = 1, b = 2` as the column-list form it means.
///
/// The SET form carries the column list and the values interleaved, so it has
/// to be unpicked before it can go through the rules a `VALUES` row goes
/// through. Only one row can be written this way, which is the whole of the
/// difference between the two forms.
fn render_insert_assignments(table: &str, insert: &Insert) -> Result<RenderedInsert, ParseError> {
    if !insert.columns.is_empty() || insert.source.is_some() {
        return unsupported("INSERT SET with a column list or a source query");
    }
    if insert.on.is_some() {
        return unsupported("INSERT SET with ON DUPLICATE KEY UPDATE");
    }
    if insert.assignments.is_empty() {
        return unsupported("INSERT SET without assignments");
    }
    let mut columns = Vec::with_capacity(insert.assignments.len());
    let mut values = Vec::with_capacity(insert.assignments.len());
    for assignment in &insert.assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(name) = &assignment.target else {
            return unsupported("INSERT SET assignment target");
        };
        columns.push(render_unqualified_name(name)?);
        values.push(render_dml_expr(&assignment.value)?);
    }
    reject_ignored_null(
        insert,
        &insert
            .assignments
            .iter()
            .map(|assignment| &assignment.value)
            .collect::<Vec<_>>(),
    )?;
    // REPLACE and IGNORE both take the SET form too, and mean there what they
    // mean on the other one.
    let verb = insert_verb(insert);
    Ok(RenderedInsert {
        sqlite_sql: format!(
            "{verb} {table} ({}) VALUES ({})",
            columns.join(", "),
            values.join(", ")
        ),
        read_tables: Vec::new(),
        compared_table: None,
        checked_comparisons: Vec::new(),
    })
}

/// Chooses the engine's insert verb for what the statement said about
/// collisions.
///
/// MySQL's `REPLACE` deletes the rows a unique key collides with and inserts,
/// which is what the engine's own `OR REPLACE` does. `INSERT IGNORE` skips a
/// colliding row, which is `OR IGNORE`. Measured on MySQL 8.4.11: an
/// `INSERT IGNORE` whose row collides on the primary key leaves the stored row
/// alone and counts 0, and a two-row one where only the second is new counts 1.
fn insert_verb(insert: &Insert) -> &'static str {
    if insert.replace_into {
        "INSERT OR REPLACE INTO"
    } else if insert.ignore {
        "INSERT OR IGNORE INTO"
    } else {
        "INSERT INTO"
    }
}

/// Refuses an `INSERT IGNORE` that writes a NULL.
///
/// This is the one place the two engines' IGNORE part company. MySQL treats a
/// NULL in a NOT NULL column as something to coerce rather than refuse —
/// measured on 8.4.11, `INSERT IGNORE` of NULL into a NOT NULL INT stores 0 —
/// while the engine's `OR IGNORE` skips the row and stores nothing. A row that
/// exists in one and not the other is a difference a client cannot see, so the
/// statement is refused instead. A NULL bound for a column that accepts one
/// would agree, but the column is not known here, so all of them are refused.
fn reject_ignored_null(insert: &Insert, values: &[&Expr]) -> Result<(), ParseError> {
    if !insert.ignore {
        return Ok(());
    }
    if values
        .iter()
        .any(|value| matches!(value, Expr::Value(value) if matches!(value.value, Value::Null)))
    {
        return unsupported("INSERT IGNORE writing NULL");
    }
    Ok(())
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

    if update.order_by.is_empty() && update.limit.is_some() {
        return unsupported("UPDATE LIMIT without ORDER BY");
    }

    if !update.order_by.is_empty() {
        let order_by_sql = render_dml_order_by(&update.order_by, render_context)?;
        let limit_sql = if let Some(limit_expr) = &update.limit {
            let limit_val = render_select_row_count(limit_expr)?;
            format!(" LIMIT {limit_val}")
        } else {
            String::new()
        };
        let sub_where = if let Some(selection) = &update.selection {
            format!(
                " WHERE {}",
                render_dml_predicate(selection, render_context)?
            )
        } else {
            String::new()
        };
        Ok(format!(
            "UPDATE {table} SET {} WHERE _rowid_ IN (SELECT _rowid_ FROM {table}{sub_where} ORDER BY {order_by_sql}{limit_sql})",
            assignments.join(", ")
        ))
    } else {
        let mut normalized = format!("UPDATE {table} SET {}", assignments.join(", "));
        if let Some(selection) = &update.selection {
            normalized.push_str(" WHERE ");
            normalized.push_str(&render_dml_predicate(selection, render_context)?);
        }
        Ok(normalized)
    }
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

    if delete.order_by.is_empty() && delete.limit.is_some() {
        return unsupported("DELETE LIMIT without ORDER BY");
    }

    if !delete.order_by.is_empty() {
        let order_by_sql = render_dml_order_by(&delete.order_by, render_context)?;
        let limit_sql = if let Some(limit_expr) = &delete.limit {
            let limit_val = render_select_row_count(limit_expr)?;
            format!(" LIMIT {limit_val}")
        } else {
            String::new()
        };
        let sub_where = if let Some(selection) = &delete.selection {
            format!(
                " WHERE {}",
                render_dml_predicate(selection, render_context)?
            )
        } else {
            String::new()
        };
        Ok(format!(
            "DELETE FROM {table} WHERE _rowid_ IN (SELECT _rowid_ FROM {table}{sub_where} ORDER BY {order_by_sql}{limit_sql})"
        ))
    } else {
        let mut normalized = format!("DELETE FROM {table}");
        if let Some(selection) = &delete.selection {
            normalized.push_str(" WHERE ");
            normalized.push_str(&render_dml_predicate(selection, render_context)?);
        }
        Ok(normalized)
    }
}

fn render_dml_order_by(
    order_by: &[sqlparser::ast::OrderByExpr],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if order_by.is_empty() {
        return unsupported("DML ORDER BY option");
    }
    order_by
        .iter()
        .map(|expression| {
            if expression.options.nulls_first.is_some() || expression.with_fill.is_some() {
                return unsupported("DML ORDER BY option");
            }
            let direction = if expression.options.asc == Some(false) {
                "DESC"
            } else {
                "ASC"
            };
            if order_by_ordinal(&expression.expr).is_some() {
                return unsupported("DML ORDER BY ordinal");
            }
            match &expression.expr {
                Expr::Identifier(ident) => {
                    render_context
                        .ordered_columns
                        .push((None, ident.value.clone()));
                    Ok(format!("{} {direction}", render_ident(ident)))
                }
                Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                    let qualifier = MySqlTableName::parse(&parts[0].value)?;
                    render_context.ordered_columns.push((
                        Some(qualifier.as_str().to_owned()),
                        parts[1].value.clone(),
                    ));
                    Ok(format!(
                        "{}.{} {direction}",
                        render_ident(&parts[0]),
                        render_ident(&parts[1])
                    ))
                }
                _ => unsupported("DML ORDER BY expression"),
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|expressions| expressions.join(", "))
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
        Expr::InList {
            expr,
            list,
            negated,
        } => render_checked_in_list(expr, list, *negated, render_context),
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
    table_columns: &'a [String],
    orders_a_bare_column: bool,
    orders_wildcard_ordinal: bool,
    compares_a_placeholder: bool,
    counts_distinct_column: bool,
    checked_subquery_comparisons: Vec<CheckedSubqueryComparison>,
    pub(crate) checked_comparisons: Vec<CheckedSelectComparison>,
    pub(crate) ordered_columns: Vec<(Option<String>, String)>,
    parameter_count: usize,
}

impl<'a> SelectRenderContext<'a> {
    pub(crate) fn new(
        source: &'a str,
        text_columns: &'a [String],
        table_columns: &'a [String],
    ) -> Self {
        Self {
            source,
            text_columns,
            table_columns,
            subquery_tables: Vec::new(),
            orders_a_bare_column: false,
            orders_wildcard_ordinal: false,
            compares_a_placeholder: false,
            counts_distinct_column: false,
            checked_subquery_comparisons: Vec::new(),
            checked_comparisons: Vec::new(),
            ordered_columns: Vec::new(),
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
            let name = source_text(render_context.source, expr)
                .unwrap_or_else(|| mysql_aggregate_column_name(function))
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?,
            ))
        }
        // MySQL names an unaliased call after its source text, as it does an
        // expression, so the engine's own spelling has to be aliased away.
        SelectItem::UnnamedExpr(expr @ Expr::Function(function))
            if static_select_metadata::scalar_call(function).is_some()
                || static_select_metadata::classify_window_call(function).is_some() =>
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
            expr @ (Expr::Substring { .. }
            | Expr::Trim { .. }
            | Expr::Floor { .. }
            | Expr::Ceil { .. }
            | Expr::Subquery(_)),
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
    format!("{}({})", function.name, aggregate_argument_name(function))
}

/// Renders a checked aggregate call, keeping the spelling it was written with.
///
/// MySQL names the result column after the call as written, case included:
/// measured, `count(*)` keeps its lower case. The engine names it the same way
/// from this text, so nothing else has to carry the name.
fn render_aggregate_call(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> String {
    // The engine calls the sample standard deviation `stddev`, where MySQL
    // keeps that name for the population one. Every other aggregate here is
    // spelled the same in both.
    let name = if function.name.to_string().eq_ignore_ascii_case("STDDEV_SAMP") {
        "stddev".to_owned()
    } else {
        function.name.to_string()
    };
    format!(
        "{name}({})",
        render_aggregate_argument(function, render_context)
    )
}

fn aggregate_argument_name(function: &sqlparser::ast::Function) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked aggregate was checked to have an argument list")
    };
    let prefix = match arguments.duplicate_treatment {
        Some(sqlparser::ast::DuplicateTreatment::Distinct) => "DISTINCT ",
        _ => "",
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)] => {
            format!("{prefix}*")
        }
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => format!("{prefix}{}", column.value),
        _ => unreachable!("a checked aggregate was checked to take one wildcard or column"),
    }
}

fn render_aggregate_argument(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked aggregate was checked to have an argument list")
    };
    let is_distinct = matches!(
        arguments.duplicate_treatment,
        Some(sqlparser::ast::DuplicateTreatment::Distinct)
    );
    let prefix = if is_distinct { "DISTINCT " } else { "" };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)] => {
            format!("{prefix}*")
        }
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => {
            if is_distinct {
                render_context.counts_distinct_column = true;
            }
            let collation = if is_distinct && render_context.is_text_column(&column.value) {
                " COLLATE NOCASE"
            } else {
                ""
            };
            format!("{prefix}{}{collation}", render_ident(column))
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
            Ok(render_aggregate_call(function, render_context))
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
            else_result,
            ..
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            let mut rendered = "CASE".to_owned();
            for when in conditions {
                rendered.push_str(" WHEN ");
                rendered.push_str(&render_select_predicate(&when.condition, render_context)?);
                rendered.push_str(" THEN ");
                rendered.push_str(&render_select_expr(&when.result, render_context)?);
            }
            // Both engines answer NULL for a row that matches nothing, so a
            // missing ELSE is written as a missing ELSE.
            if let Some(else_result) = else_result {
                rendered.push_str(" ELSE ");
                rendered.push_str(&render_select_expr(else_result, render_context)?);
            }
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
        // The engine's three names are what MySQL's one name with a side is.
        // What to trim is one character, which is the only width where
        // removing whole copies and removing any of the characters agree.
        Expr::Trim {
            trim_where,
            trim_what,
            expr: target,
            ..
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            use sqlparser::ast::TrimWhereField;
            let name = match trim_where {
                Some(TrimWhereField::Leading) => "ltrim",
                Some(TrimWhereField::Trailing) => "rtrim",
                Some(TrimWhereField::Both) | None => "trim",
            };
            let target = render_select_expr(target, render_context)?;
            match trim_what {
                Some(trim_what) => {
                    let removed = render_select_expr(trim_what, render_context)?;
                    Ok(format!("{name}({target}, {removed})"))
                }
                None => Ok(format!("{name}({target})")),
            }
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
        // A scalar subquery goes through the same reader a subquery in a
        // `WHERE` does, so the table it reads is named and authorized like any
        // other, and its own rules are the ones a bare `SELECT` is held to.
        Expr::Subquery(query)
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let (rendered, _) = render_subquery(query, render_context)?;
            Ok(format!("({rendered})"))
        }
        Expr::Function(function) if static_select_metadata::scalar_call(function).is_some() => {
            render_scalar_call(function, render_context)
        }
        Expr::Function(function)
            if static_select_metadata::classify_window_call(function).is_some() =>
        {
            render_window_call(function, render_context)
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

/// Renders `ROW_NUMBER()`, `RANK()` or `DENSE_RANK()` over its window.
///
/// Both engines spell the three the same way, so only the window is rewritten:
/// a text column is partitioned and ordered under the case-ignoring collation
/// MySQL's default gives it, the same treatment an outer `ORDER BY` gets.
fn render_window_call(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        unreachable!("a checked window call was checked to have one name");
    };
    let Some(over) = function.over.as_ref() else {
        unreachable!("a checked window call was checked to have a window");
    };
    let Some(spec) = static_select_metadata::checked_window_spec(over) else {
        unreachable!("a checked window call was checked to have a checked window");
    };
    let FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked window call was checked to have an argument list");
    };
    // `NTILE` carries a count and `LAG` and `LEAD` a column; the rest carry
    // nothing, and each spelling is the engine's own.
    let arguments = arguments
        .args
        .iter()
        .map(|argument| match argument {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
                render_select_expr(expr, render_context)
            }
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard) => {
                Ok("*".to_owned())
            }
            _ => unreachable!("a checked window call was checked to have plain arguments"),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let mut window = String::new();
    if !spec.partition_by.is_empty() {
        window.push_str("PARTITION BY ");
        let terms = spec
            .partition_by
            .iter()
            .map(|expr| render_window_column(expr, render_context))
            .collect::<Result<Vec<_>, _>>()?;
        window.push_str(&terms.join(", "));
    }
    if !spec.order_by.is_empty() {
        if !window.is_empty() {
            window.push(' ');
        }
        window.push_str("ORDER BY ");
        let terms = spec
            .order_by
            .iter()
            .map(|term| {
                let direction = if term.options.asc == Some(false) {
                    "DESC"
                } else {
                    "ASC"
                };
                Ok(format!(
                    "{} {direction}",
                    render_window_column(&term.expr, render_context)?
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        window.push_str(&terms.join(", "));
    }
    if let Some(frame) = spec.window_frame.as_ref() {
        if !window.is_empty() {
            window.push(' ');
        }
        // The shorthand `ROWS <bound>` means `BETWEEN <bound> AND CURRENT ROW`,
        // so it is written out — measured on MySQL 8.4.11, the two answer the
        // same rows, and so do they in the engine.
        window.push_str(&format!(
            "{} BETWEEN {} AND {}",
            frame.units,
            frame.start_bound,
            frame
                .end_bound
                .as_ref()
                .map_or_else(|| "CURRENT ROW".to_owned(), ToString::to_string)
        ));
    }
    Ok(format!(
        "{}({arguments}) OVER ({window})",
        name.value.to_ascii_lowercase()
    ))
}

/// Renders one column a window partitions or orders by.
fn render_window_column(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let Expr::Identifier(column) = expr else {
        unreachable!("a checked window was checked to name plain columns");
    };
    render_context.orders_a_bare_column = true;
    let collation = if render_context.is_text_column(&column.value) {
        " COLLATE NOCASE"
    } else {
        ""
    };
    Ok(format!("{}{collation}", render_ident(column)))
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
    } else if name.value.eq_ignore_ascii_case("CURDATE")
        || name.value.eq_ignore_ascii_case("CURRENT_DATE")
    {
        // The engine writes `date('now')` as `YYYY-MM-DD`, which is the form
        // MySQL answers and the form a DATE column holds.
        return Ok("date('now')".to_owned());
    } else if name.value.eq_ignore_ascii_case("CURTIME")
        || name.value.eq_ignore_ascii_case("CURRENT_TIME")
    {
        return Ok("time('now')".to_owned());
    } else if let Some(field) = strftime_field(&name.value) {
        // The engine reads a part of a moment out as text, where MySQL
        // answers a number, so the cast is what keeps the two agreeing.
        return Ok(format!(
            "CAST(strftime('{field}', {}) AS INTEGER)",
            single_column_argument(function)
        ));
    } else if name.value.eq_ignore_ascii_case("DATE_ADD")
        || name.value.eq_ignore_ascii_case("DATE_SUB")
    {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked call was checked to have an argument list");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Interval(interval),
        ))] = arguments.args.as_slice()
        else {
            unreachable!("a checked shift was checked to take a column and an interval");
        };
        let Some((unit, whole_days)) = static_select_metadata::checked_interval_unit(interval)
        else {
            unreachable!("a checked shift was checked to name a unit");
        };
        let sign = if name.value.eq_ignore_ascii_case("DATE_SUB") {
            "-"
        } else {
            "+"
        };
        // The engine spells the shift as a modifier, and its two readers
        // answer the day alone or the whole moment. Measured on MySQL
        // 8.4.11: an interval of whole days keeps the column's own kind —
        // a DATE stays a DATE and a DATETIME keeps its time — while an
        // interval carrying a time answers a moment either way. Which
        // reader to ask therefore depends on the column, which this layer
        // does not know the type of; the stored text says it instead,
        // a DATE being exactly the ten characters of `YYYY-MM-DD`.
        let column = render_ident(column);
        let modifier = format!("'{sign}{} {unit}'", interval.value);
        if !whole_days {
            return Ok(format!("datetime({column}, {modifier})"));
        }
        return Ok(format!(
            "CASE WHEN length({column}) = 10 THEN date({column}, {modifier}) \
ELSE datetime({column}, {modifier}) END"
        ));
    } else if name.value.eq_ignore_ascii_case("DATEDIFF") {
        // MySQL counts whole days between the dates alone, dropping any
        // time either carries, which `date()` does here.
        let [left, right] = two_column_arguments(function);
        return Ok(format!(
            "CAST(julianday(date({left})) - julianday(date({right})) AS INTEGER)"
        ));
    } else if name.value.eq_ignore_ascii_case("LOWER") {
        "lower"
    } else if name.value.eq_ignore_ascii_case("UPPER") {
        "upper"
    } else if name.value.eq_ignore_ascii_case("REVERSE") {
        "string_reverse"
    } else if name.value.eq_ignore_ascii_case("HEX") {
        "hex"
    } else if name.value.eq_ignore_ascii_case("ABS") {
        "abs"
    } else if name.value.eq_ignore_ascii_case("SIGN") {
        "sign"
    } else if name.value.eq_ignore_ascii_case("SQRT") {
        "sqrt"
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
            single_column_argument(function)
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
    } else if name.value.eq_ignore_ascii_case("POW") || name.value.eq_ignore_ascii_case("POWER") {
        return Ok(format!(
            "pow({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("MOD") {
        return Ok(format!(
            "CAST(mod({}, {}) AS INTEGER)",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("INSTR") {
        return Ok(format!(
            "instr({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("LOCATE") {
        return Ok(format!(
            "instr({}, {})",
            scalar_argument(function, 1)?,
            scalar_argument(function, 0)?
        ));
    } else if name.value.eq_ignore_ascii_case("REPLACE") {
        return Ok(format!(
            "replace({}, {}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("REPEAT") {
        return Ok(format!(
            "repeat({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("LPAD") || name.value.eq_ignore_ascii_case("RPAD") {
        return Ok(format!(
            "{}({}, {}, {})",
            name.value.to_lowercase(),
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("IFNULL")
        || name.value.eq_ignore_ascii_case("COALESCE")
        || name.value.eq_ignore_ascii_case("NULLIF")
    {
        return Ok(format!(
            "{}({})",
            name.value.to_lowercase(),
            render_scalar_arguments(function)?
        ));
    } else if name.value.eq_ignore_ascii_case("GREATEST") {
        return Ok(format!("max({})", render_scalar_arguments(function)?));
    } else if name.value.eq_ignore_ascii_case("LEAST") {
        return Ok(format!("min({})", render_scalar_arguments(function)?));
    } else {
        unreachable!("a checked scalar call was already recognized");
    };
    Ok(format!("{engine}({})", single_column_argument(function)))
}

/// Names the strftime field a MySQL reading call asks for.
fn strftime_field(name: &str) -> Option<&'static str> {
    for (call, field) in [
        ("YEAR", "%Y"),
        ("MONTH", "%m"),
        ("DAY", "%d"),
        ("HOUR", "%H"),
        ("MINUTE", "%M"),
        ("SECOND", "%S"),
    ] {
        if name.eq_ignore_ascii_case(call) {
            return Some(field);
        }
    }
    None
}

/// Renders the two columns a checked two-column call names.
fn two_column_arguments(function: &sqlparser::ast::Function) -> [String; 2] {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked call was checked to have an argument list");
    };
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Identifier(left),
    )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Identifier(right),
    ))] = arguments.args.as_slice()
    else {
        unreachable!("a checked call was checked to take two columns");
    };
    [render_ident(left), render_ident(right)]
}

fn single_column_argument(function: &sqlparser::ast::Function) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked call was checked to have an argument list");
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => render_ident(column),
        _ => unreachable!("a checked call was checked to take one column"),
    }
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
        Expr::Substring { .. } | Expr::Trim { .. } | Expr::Floor { .. } | Expr::Ceil { .. }
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
    // A bare `CURRENT_DATE` is a call with no parentheses at all, so there is
    // no closing one to reach for and its span is already the whole name.
    let closes_with_a_paren = match expr {
        Expr::Function(function) => {
            !matches!(function.args, sqlparser::ast::FunctionArguments::None)
        }
        Expr::Substring { .. } | Expr::Trim { .. } | Expr::Floor { .. } | Expr::Ceil { .. } => true,
        _ => false,
    };
    if closes_with_a_paren && !source.get(start..end)?.trim_end().ends_with(')') {
        let closing = bytes[end..].iter().position(|byte| *byte == b')')? + end;
        end = closing + 1;
    }
    // A windowed call's span stops at its arguments, and MySQL's name for the
    // column carries the whole `OVER (...)` after them.
    if matches!(expr, Expr::Function(function) if function.over.is_some()) {
        end += window_clause_len(source.get(end..)?)?;
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
        // A subquery's span covers the `SELECT` and not the parentheses around
        // it, and MySQL names the column after both.
        Expr::Subquery(_) => 1,
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
        BinaryOperator::Spaceship => Some(BinaryOperator::Spaceship),
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
    let (qualifier, column) = match expr {
        Expr::Identifier(ident) => (None, ident),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => (Some(&parts[0]), &parts[1]),
        _ => return unsupported("SELECT IN requires one column"),
    };
    if list.is_empty() {
        return unsupported("SELECT IN over an empty list");
    }
    let column_name = column.value.clone();
    let mut members = Vec::with_capacity(list.len());
    for element in list {
        members.push(render_checked_select_comparison_rhs(
            element,
            render_context,
        )?);
    }
    // One text member collates the whole list, because MySQL compares every
    // member under the column's collation rather than each member's own. A `?`
    // carries no type until it is bound, so it is collated only once the
    // caller has said the column is text.
    // Under the single-source assumption verified in `translate_select_query`,
    // the unqualified column name is sufficient to look up the column's type.
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
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let rendered = format!(
        "({rendered_column}{} {}IN ({}))",
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
                qualifier: qualifier.map(|q| q.value.clone()),
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
    let is_valid_column = match expr {
        Expr::Identifier(_) => true,
        Expr::CompoundIdentifier(parts) => parts.len() == 2,
        _ => false,
    };
    if !is_valid_column {
        return unsupported("BETWEEN requires a column as its subject");
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
    let (qualifier, column, op_reversed, rhs_expr) = match (left, right) {
        (Expr::Identifier(column), _) => (None, column, op.clone(), right),
        (Expr::CompoundIdentifier(parts), _) if parts.len() == 2 => {
            (Some(&parts[0]), &parts[1], op.clone(), right)
        }
        (_, Expr::Identifier(column)) => {
            let reversed =
                reverse_checked_comparison_operator(op).ok_or(ParseError::Unsupported {
                    feature: "reversed SELECT comparison operator",
                })?;
            (None, column, reversed, left)
        }
        (_, Expr::CompoundIdentifier(parts)) if parts.len() == 2 => {
            let reversed =
                reverse_checked_comparison_operator(op).ok_or(ParseError::Unsupported {
                    feature: "reversed SELECT comparison operator",
                })?;
            (Some(&parts[0]), &parts[1], reversed, left)
        }
        _ => return unsupported("SELECT comparison requires one column"),
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
    // Under the single-source assumption verified in `translate_select_query`,
    // the unqualified column name is sufficient to look up the column's type.
    let collated = match rhs {
        CheckedSelectComparisonRhs::Text(_) => true,
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            render_context.compares_a_placeholder = true;
            render_context.is_text_column(&column_name)
        }
        _ => false,
    };
    let collation = if collated { " COLLATE NOCASE" } else { "" };
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let rendered = format!(
        "({rendered_column}{collation} {} {rendered_rhs})",
        checked_select_comparison_sql_operator(&op_reversed)
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: qualifier.map(|q| q.value.clone()),
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
    let (qualifier, column) = match expr {
        Expr::Identifier(ident) => (None, ident),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => (Some(&parts[0]), &parts[1]),
        _ => return unsupported("SELECT LIKE requires one column"),
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
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let rendered = format!(
        "({rendered_column} {}LIKE '{}')",
        if negated { "NOT " } else { "" },
        text.replace('\'', "''")
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: qualifier.map(|q| q.value.clone()),
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
            | BinaryOperator::Spaceship
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
        BinaryOperator::Spaceship => CheckedSelectComparisonOperator::NullSafeEqual,
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
        BinaryOperator::Spaceship => "IS",
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
