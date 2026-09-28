//! `GROUP BY`, and what MySQL's `ONLY_FULL_GROUP_BY` lets a grouped statement
//! name around it.
//!
//! That mode is in MySQL 8.4's default `sql_mode`, and this server takes a
//! client's `SET sql_mode` naming it, so the rule has to be real here. What
//! MySQL holds a grouped statement to, measured on 8.4.11:
//!
//! - A projected or ordered expression answers 1055 unless it is one of the
//!   grouping keys as written, or every column it names outside an aggregate is
//!   a grouping key that is a whole column. `UPPER(title)` passes under `GROUP
//!   BY title` and `UPPER(DATE(created_at))` does not pass under `GROUP BY
//!   DATE(created_at)`: MySQL matches a whole key, not a key inside a larger
//!   expression.
//! - A `HAVING` answers 1054 for a column that is not a whole-column key — even
//!   one inside a key, `HAVING DATE(created_at) > ...` under `GROUP BY
//!   DATE(created_at)`. It may name the projection's aliases.
//!
//! MySQL also lets a column through when the keys decide it: they hold a
//! table's primary key, or a unique key whose columns are all `NOT NULL`, and
//! so decide the whole row — `SELECT name ... GROUP BY id` — also across a
//! join that matches one table's column to another's. Which keys a table has
//! is the frontend's to know, so such a column is recorded here with what the
//! statement's joins say, in a [`MySqlColumnsTheKeysDecide`], and checked
//! there.

use super::*;

/// A column a grouped statement names, with the table name or the alias it
/// was written with, when it was written with one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlNamedColumn {
    table: Option<String>,
    column: String,
}

impl MySqlNamedColumn {
    /// Returns the table name or alias the column was written with.
    pub fn table(&self) -> Option<&str> {
        self.table.as_deref()
    }

    /// Returns the column's name.
    pub fn column(&self) -> &str {
        &self.column
    }

    fn written(column: GroupedColumn<'_>) -> Self {
        Self {
            table: column.0.map(|table| table.value.clone()),
            column: column.1.value.clone(),
        }
    }
}

/// The columns a grouped statement projects beside its keys, which MySQL
/// takes only when the keys decide them.
///
/// Measured on MySQL 8.4.11 under `ONLY_FULL_GROUP_BY`: keys holding a
/// table's primary key, or a unique key whose columns are all `NOT NULL`,
/// decide every column of that table — `SELECT name ... GROUP BY id` and
/// `GROUP BY email` pass, a unique key over a nullable column does not, and
/// neither does `GROUP BY id + 0`. A join's `ON` carries a decided column to
/// the column it is matched with: `users u JOIN posts p ON u.id = p.user_id
/// GROUP BY p.id` decides `u.name`. A `LEFT JOIN` carries one only from the
/// tables before it to the table it adds, and only when every column of those
/// tables its `ON` names is decided: `posts p LEFT JOIN users u ON u.id =
/// p.user_id GROUP BY p.id` decides `u.name`, `users u LEFT JOIN posts p ...
/// GROUP BY p.id` does not decide `u.name`, and neither does an `ON` joined by
/// `OR` or one also naming a column the keys do not decide. A table's keys
/// decide its own columns on either side of a `LEFT JOIN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlColumnsTheKeysDecide {
    keys: Vec<MySqlNamedColumn>,
    columns: Vec<MySqlNamedColumn>,
    joins: Vec<MySqlJoinedTable>,
}

impl MySqlColumnsTheKeysDecide {
    /// Returns the grouping keys that are whole columns.
    pub fn keys(&self) -> &[MySqlNamedColumn] {
        &self.keys
    }

    /// Returns the projected columns the keys have to decide.
    pub fn columns(&self) -> &[MySqlNamedColumn] {
        &self.columns
    }

    /// Returns each table the statement joins to its first, in order.
    pub fn joins(&self) -> &[MySqlJoinedTable] {
        &self.joins
    }
}

/// One table a statement joins, and what its `ON` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlJoinedTable {
    reference: String,
    left_join: bool,
    matched_columns: Vec<(MySqlNamedColumn, MySqlNamedColumn)>,
    other_columns: Option<Vec<MySqlNamedColumn>>,
}

impl MySqlJoinedTable {
    /// Returns the name the joined table goes by: its alias, or its own name.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Reports whether this is a `LEFT JOIN`, which keeps a row of the tables
    /// before it that it finds no match for.
    pub fn left_join(&self) -> bool {
        self.left_join
    }

    /// Returns each pair of columns the `ON` matches with `=`, standing on its
    /// own or among others joined by `AND`.
    pub fn matched_columns(&self) -> &[(MySqlNamedColumn, MySqlNamedColumn)] {
        &self.matched_columns
    }

    /// Returns every column the rest of the `ON` names, or nothing when the
    /// rest is written in a form not read here.
    pub fn other_columns(&self) -> Option<&[MySqlNamedColumn]> {
        self.other_columns.as_deref()
    }
}

/// Writes each `GROUP BY` key that is a place in the projection — Django's
/// `GROUP BY 1` — as the column standing there.
///
/// Measured on MySQL 8.4.11: `GROUP BY 1` groups by the first projected
/// column, answering the rows and the shapes naming the column answers, and a
/// `HAVING` may name it. A place holding an aggregate answers 1056, and one
/// that is no place — `0`, or past the last — 1054, so each is refused, as is
/// a place holding anything but a whole column, and a place beside `WITH
/// ROLLUP`, neither measured. Only the statement's own `GROUP BY` is read
/// here; one in a subquery or a derived table stays refused.
pub(crate) fn name_the_columns_grouped_by_place(
    query: &mut sqlparser::ast::Query,
) -> Result<(), ParseError> {
    let SetExpr::Select(select) = query.body.as_mut() else {
        return Ok(());
    };
    let sqlparser::ast::GroupByExpr::Expressions(keys, modifiers) = &mut select.group_by else {
        return Ok(());
    };
    for key in keys.iter_mut() {
        let Expr::Value(value) = &*key else {
            continue;
        };
        let Value::Number(place, false) = &value.value else {
            continue;
        };
        if !modifiers.is_empty() {
            return unsupported("GROUP BY a place in the projection WITH ROLLUP");
        }
        let column = place
            .parse::<usize>()
            .ok()
            .and_then(|place| place.checked_sub(1))
            .and_then(|place| select.projection.get(place))
            .and_then(|item| match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    grouped_column(expr).is_some().then(|| expr.clone())
                }
                _ => None,
            });
        let Some(column) = column else {
            return unsupported("GROUP BY a place in the projection holding no column");
        };
        *key = column;
    }
    Ok(())
}

/// Renders a `GROUP BY` and holds the projection to `ONLY_FULL_GROUP_BY`.
///
/// A key is a whole column, or one of the calls [`groups_the_way_mysql_does`]
/// takes. A key is rendered the way a projection renders the same expression,
/// so the engine groups on the value the client reads back.
pub(super) fn render_select_group_by(
    group_by: &[Expr],
    select: &sqlparser::ast::Select,
    outer_statement: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let projection = select.projection.as_slice();
    let mut rendered = Vec::with_capacity(group_by.len());
    for key in group_by {
        // A key naming the projection's alias for an expression groups by
        // that expression, which is held to what an expression key is.
        if let Some(aliased) = aliased_expression(key, projection) {
            if !groups_the_way_mysql_does(aliased) {
                return unsupported("GROUP BY key");
            }
        }
        if let Some((table, column)) = grouped_column(key) {
            rendered.push(match table {
                Some(table) => format!("{}.{}", render_ident(table), render_ident(column)),
                None => render_ident(column),
            });
            continue;
        }
        if !groups_the_way_mysql_does(key) {
            return unsupported("GROUP BY key");
        }
        rendered.push(render_select_expr(key, render_context)?);
    }
    let mut decided = Vec::new();
    for item in projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            // A wildcard names columns this cannot see, so it cannot be held to
            // the rule and is refused rather than let through.
            _ => return unsupported("GROUP BY with a wildcard projection"),
        };
        if answers_one_value_per_group(expr, group_by) || is_named_by_a_key(item, group_by) {
            continue;
        }
        match columns_beside_the_keys(expr, group_by) {
            Some(columns) if outer_statement => decided.extend(columns),
            _ => return unsupported("GROUP BY leaves a projected column out of the grouping"),
        }
    }
    if !decided.is_empty() {
        let Some(joins) = joined_tables(&select.from) else {
            return unsupported("GROUP BY keys deciding a column over this FROM");
        };
        render_context.columns_the_keys_decide = Some(MySqlColumnsTheKeysDecide {
            keys: group_by
                .iter()
                .filter_map(grouped_column)
                .map(MySqlNamedColumn::written)
                .collect(),
            columns: decided,
            joins,
        });
    }
    Ok(rendered.join(", "))
}

/// Reads a statement that aggregates without a `GROUP BY` and projects whole
/// columns beside its aggregates, each of which has to be decided by columns
/// the statement compares with a written value.
///
/// Measured on MySQL 8.4.11: all its rows are one group, so a column passes
/// when `col = value` in the `WHERE` or in an inner join's `ON` fixes it —
/// `WHERE u.name = 'Bob'` decides `u.name` — or fixes a key that decides it,
/// as a grouping key would: `JOIN users u ON p.user_id = u.id AND u.email =
/// 'alice@example.com'` decides `u.name`. The same comparison in a `LEFT
/// JOIN`'s `ON`, or joined by `OR`, decides nothing and answers 1140.
pub(super) fn columns_written_values_decide(
    select: &sqlparser::ast::Select,
) -> Option<MySqlColumnsTheKeysDecide> {
    use sqlparser::ast::{JoinConstraint, JoinOperator};
    let mut columns = Vec::new();
    for item in &select.projection {
        let (SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) = item else {
            return None;
        };
        if aggregates_or_literals_only(expr) {
            continue;
        }
        columns.push(MySqlNamedColumn::written(grouped_column(expr)?));
    }
    if columns.is_empty() {
        return None;
    }
    let joins = joined_tables(&select.from)?;
    let [from] = select.from.as_slice() else {
        return None;
    };
    let inner_join_conditions = from
        .joins
        .iter()
        .filter_map(|join| match &join.join_operator {
            JoinOperator::Join(JoinConstraint::On(on))
            | JoinOperator::Inner(JoinConstraint::On(on)) => Some(on),
            _ => None,
        });
    let keys = select
        .selection
        .iter()
        .chain(inner_join_conditions)
        .flat_map(conditions_joined_by_and)
        .filter_map(column_fixed_to_a_written_value)
        .map(MySqlNamedColumn::written)
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return None;
    }
    Some(MySqlColumnsTheKeysDecide {
        keys,
        columns,
        joins,
    })
}

/// The column of `col = value` or `value = col`, `value` a written number or
/// word.
fn column_fixed_to_a_written_value(condition: &Expr) -> Option<GroupedColumn<'_>> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = condition
    else {
        return None;
    };
    let is_written = |expr: &Expr| {
        matches!(
            without_parentheses(expr),
            Expr::Value(value) if matches!(
                value.value,
                Value::Number(_, false) | Value::SingleQuotedString(_)
            )
        )
    };
    match (
        grouped_column(without_parentheses(left)),
        grouped_column(without_parentheses(right)),
    ) {
        (Some(column), None) if is_written(right) => Some(column),
        (None, Some(column)) if is_written(left) => Some(column),
        _ => None,
    }
}

/// Returns the columns an expression names outside its aggregates that are
/// not grouping keys, when it names nothing else a group has more than one
/// of.
fn columns_beside_the_keys(expr: &Expr, group_by: &[Expr]) -> Option<Vec<MySqlNamedColumn>> {
    let whole_column_keys = group_by
        .iter()
        .filter(|key| grouped_column(key).is_some())
        .cloned()
        .collect::<Vec<_>>();
    let mut columns = Vec::new();
    reads_only(expr, &mut |read| {
        if group_by
            .iter()
            .any(|key| names_the_same_expression(key, read))
            || names_a_whole_column_key(read, &whole_column_keys)
        {
            return true;
        }
        match grouped_column(read) {
            Some(column) => {
                columns.push(MySqlNamedColumn::written(column));
                true
            }
            None => false,
        }
    })
    .then_some(columns)
}

/// Reads what each join of a `FROM` says, for a `FROM` of one table and the
/// tables joined to it by `JOIN` or `LEFT JOIN` with an `ON`, each a table
/// under its own name or an alias.
///
/// Anything else — a comma, `USING`, a `RIGHT JOIN`, a derived table — has
/// not been measured and answers nothing, which refuses the column.
fn joined_tables(from: &[sqlparser::ast::TableWithJoins]) -> Option<Vec<MySqlJoinedTable>> {
    use sqlparser::ast::{JoinConstraint, JoinOperator};
    let [sqlparser::ast::TableWithJoins { relation, joins }] = from else {
        return None;
    };
    table_reference(relation)?;
    joins
        .iter()
        .map(|join| {
            let (left_join, on) = match &join.join_operator {
                JoinOperator::Join(JoinConstraint::On(on))
                | JoinOperator::Inner(JoinConstraint::On(on)) => (false, on),
                JoinOperator::Left(JoinConstraint::On(on))
                | JoinOperator::LeftOuter(JoinConstraint::On(on)) => (true, on),
                _ => return None,
            };
            let mut matched_columns = Vec::new();
            let mut other_columns = Some(Vec::new());
            for condition in conditions_joined_by_and(on) {
                if let Expr::BinaryOp {
                    left,
                    op: BinaryOperator::Eq,
                    right,
                } = condition
                {
                    if let (Some(left), Some(right)) = (
                        grouped_column(without_parentheses(left)),
                        grouped_column(without_parentheses(right)),
                    ) {
                        matched_columns.push((
                            MySqlNamedColumn::written(left),
                            MySqlNamedColumn::written(right),
                        ));
                        continue;
                    }
                }
                if let Some(columns) = other_columns.as_mut() {
                    if !columns_named_in(condition, columns) {
                        other_columns = None;
                    }
                }
            }
            Some(MySqlJoinedTable {
                reference: table_reference(&join.relation)?,
                left_join,
                matched_columns,
                other_columns,
            })
        })
        .collect()
}

/// Returns the name a table in a `FROM` goes by, when it is a plain table.
pub(super) fn table_reference(relation: &TableFactor) -> Option<String> {
    let TableFactor::Table {
        name,
        alias,
        args: None,
        with_hints,
        version: None,
        partitions,
        ..
    } = relation
    else {
        return None;
    };
    if !with_hints.is_empty() || !partitions.is_empty() {
        return None;
    }
    if let Some(alias) = alias {
        return alias.columns.is_empty().then(|| alias.name.value.clone());
    }
    match name.0.last()? {
        ObjectNamePart::Identifier(table) => Some(table.value.clone()),
        _ => None,
    }
}

/// Splits a condition into the parts `AND` joins, through parentheses.
fn conditions_joined_by_and(condition: &Expr) -> Vec<&Expr> {
    match without_parentheses(condition) {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut conditions = conditions_joined_by_and(left);
            conditions.extend(conditions_joined_by_and(right));
            conditions
        }
        condition => vec![condition],
    }
}

fn without_parentheses(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => without_parentheses(inner),
        expr => expr,
    }
}

/// Adds every column an expression names to `columns`, and reports whether
/// it is written in a form this reads through.
pub(super) fn columns_named_in(expr: &Expr, columns: &mut Vec<MySqlNamedColumn>) -> bool {
    if let Some(column) = grouped_column(expr) {
        columns.push(MySqlNamedColumn::written(column));
        return true;
    }
    match expr {
        Expr::Value(_) => true,
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner) => columns_named_in(inner, columns),
        Expr::BinaryOp { left, right, .. } => {
            columns_named_in(left, columns) && columns_named_in(right, columns)
        }
        Expr::InList { expr, list, .. } => {
            columns_named_in(expr, columns)
                && list.iter().all(|member| columns_named_in(member, columns))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            columns_named_in(expr, columns)
                && columns_named_in(low, columns)
                && columns_named_in(high, columns)
        }
        Expr::Like { expr, pattern, .. } => {
            columns_named_in(expr, columns) && columns_named_in(pattern, columns)
        }
        Expr::Function(function) if function.over.is_none() && function.filter.is_none() => {
            match &function.args {
                sqlparser::ast::FunctionArguments::None => true,
                sqlparser::ast::FunctionArguments::List(arguments) => {
                    arguments.clauses.is_empty()
                        && arguments.args.iter().all(|argument| match argument {
                            sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Expr(argument),
                            ) => columns_named_in(argument, columns),
                            sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Wildcard,
                            ) => true,
                            _ => false,
                        })
                }
                sqlparser::ast::FunctionArguments::Subquery(_) => false,
            }
        }
        _ => false,
    }
}

/// Reports whether a grouping key is a call the engine groups the way MySQL
/// does.
///
/// Each of these answers a day, a moment's part as a whole number, or a
/// moment written out, and two rows land in one group in both engines exactly
/// when the answers are equal. A call answering words the client wrote itself
/// is not taken: MySQL groups words under the column's collation, where `a`
/// and `A` are one group, and the engine groups the answer by its bytes.
/// `DATE_FORMAT` and the day and month names answer words too, but only
/// words one format writes, and two of those that differ in case or accent
/// alone do not come out of it.
fn groups_the_way_mysql_does(key: &Expr) -> bool {
    matches!(
        static_select_metadata::classify_static_select_expr(key),
        Some(StaticSelectMetadata::ScalarCall {
            function: ScalarFunction::CastsToDay
                | ScalarFunction::ReadsTheYear
                | ScalarFunction::ReadsAMonthOrDay
                | ScalarFunction::ReadsTheHour
                | ScalarFunction::ReadsAMinuteOrSecond
                | ScalarFunction::ReadsTheQuarter
                | ScalarFunction::ReadsADayOfTheWeek
                | ScalarFunction::ReadsTheDayOfTheYear
                | ScalarFunction::ReadsTheLastDay
                | ScalarFunction::ReadsTheYearAsANumber
                | ScalarFunction::NamesTheDayOrMonth
                | ScalarFunction::WritesAMoment,
            columns,
            ..
        }) if !columns.is_empty()
    )
}

/// Holds a grouped statement's `HAVING` to what MySQL lets it name.
///
/// It is read before the projection's aliases are put in its place, because
/// an alias is a name MySQL takes there where the expression behind it may not
/// be.
pub(super) fn hold_the_grouped_having(
    having: &Expr,
    projection: &[SelectItem],
    group_by: &[Expr],
) -> Result<(), ParseError> {
    let names_an_alias = |name: &Ident| {
        projection.iter().any(|item| {
            matches!(item, SelectItem::ExprWithAlias { alias, .. }
                if alias.value.eq_ignore_ascii_case(&name.value))
        })
    };
    let whole_column_keys = group_by
        .iter()
        .filter(|key| grouped_column(key).is_some())
        .cloned()
        .collect::<Vec<_>>();
    if !reads_only(having, &mut |expr| match expr {
        Expr::Identifier(name) if names_an_alias(name) => true,
        _ => names_a_whole_column_key(expr, &whole_column_keys),
    }) {
        return unsupported("HAVING names a column that is not a whole grouping key");
    }
    Ok(())
}

/// Holds a grouped statement's `ORDER BY` to `ONLY_FULL_GROUP_BY`, the way its
/// projection is held.
///
/// An ordinal and a projection's alias name something the projection was
/// already held to.
pub(super) fn hold_the_grouped_order_by(
    order_by: &sqlparser::ast::OrderBy,
    select: &sqlparser::ast::Select,
) -> Result<(), ParseError> {
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return Ok(());
    };
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return Ok(());
    };
    if group_by.is_empty() {
        return Ok(());
    }
    for expression in expressions {
        let ordered = &expression.expr;
        let names_an_alias = matches!(ordered, Expr::Identifier(name)
            if select.projection.iter().any(|item| matches!(item,
                SelectItem::ExprWithAlias { alias, .. }
                    if alias.value.eq_ignore_ascii_case(&name.value))));
        if names_an_alias
            || order_by_ordinal(ordered).is_some()
            || answers_one_value_per_group(ordered, group_by)
        {
            continue;
        }
        return unsupported("GROUP BY leaves an ordered column out of the grouping");
    }
    Ok(())
}

/// Returns a statement's grouping keys when one of them is an expression.
///
/// MySQL groups by an expression in a temporary table, and a result column is
/// then the temporary table's column rather than the expression's own, which
/// is a shape of its own — measured on 8.4.11, `YEAR(created_at)` answers a
/// `LONG` there where it answers a `YEAR` on its own. A statement grouping by
/// whole columns alone may read its groups off an index instead, where MySQL
/// reports each answer's own shape, so that one is left alone.
pub(super) fn expression_grouping_keys(select: &sqlparser::ast::Select) -> Option<&[Expr]> {
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return None;
    };
    group_by
        .iter()
        .any(|key| {
            grouped_column(key).is_none() || aliased_expression(key, &select.projection).is_some()
        })
        .then_some(group_by.as_slice())
}

/// Returns the expression a key names through the projection's alias for it,
/// when the key is such a name and the expression is not a whole column.
fn aliased_expression<'a>(key: &Expr, projection: &'a [SelectItem]) -> Option<&'a Expr> {
    let Expr::Identifier(name) = key else {
        return None;
    };
    projection.iter().find_map(|item| match item {
        SelectItem::ExprWithAlias { expr, alias }
            if alias.value.eq_ignore_ascii_case(&name.value) && grouped_column(expr).is_none() =>
        {
            Some(expr)
        }
        _ => None,
    })
}

/// Reports whether a projected column is one of the grouping keys: written
/// as one, or named by one through its alias.
pub(super) fn is_named_by_a_key(item: &SelectItem, group_by: &[Expr]) -> bool {
    let (expr, alias) = match item {
        SelectItem::UnnamedExpr(expr) => (expr, None),
        SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
        _ => return false,
    };
    group_by.iter().any(|key| {
        names_the_same_expression(key, expr)
            || matches!((key, alias), (Expr::Identifier(key), Some(alias))
                if key.value.eq_ignore_ascii_case(&alias.value))
    })
}

/// Reports whether an expression answers one value for each group.
fn answers_one_value_per_group(expr: &Expr, group_by: &[Expr]) -> bool {
    let whole_column_keys = group_by
        .iter()
        .filter(|key| grouped_column(key).is_some())
        .cloned()
        .collect::<Vec<_>>();
    reads_only(expr, &mut |read| {
        group_by
            .iter()
            .any(|key| names_the_same_expression(key, read))
            || names_a_whole_column_key(read, &whole_column_keys)
    })
}

fn names_a_whole_column_key(expr: &Expr, whole_column_keys: &[Expr]) -> bool {
    let Some(read) = grouped_column(expr) else {
        return false;
    };
    whole_column_keys
        .iter()
        .any(|key| grouped_column(key).is_some_and(|key| names_same_column(key, read)))
}

/// Walks an expression outside its aggregates and reports whether `allowed`
/// takes everything it finds there that is not a written value.
///
/// `allowed` is asked about every expression before it is taken apart, so a
/// whole grouping key is taken as one. A column it does not take, and
/// anything this does not know how to walk — a subquery, a window — answers
/// false, which refuses the statement rather than guessing.
fn reads_only(expr: &Expr, allowed: &mut dyn FnMut(&Expr) -> bool) -> bool {
    if allowed(expr) {
        return true;
    }
    match expr {
        Expr::Value(_) => true,
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => false,
        Expr::Function(function) if function.over.is_some() => false,
        Expr::Function(function) if names_an_aggregate_call(function) => true,
        Expr::Function(function) => match &function.args {
            sqlparser::ast::FunctionArguments::None => true,
            sqlparser::ast::FunctionArguments::List(arguments) => {
                arguments.args.iter().all(|argument| match argument {
                    sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(argument),
                    ) => reads_only(argument, allowed),
                    _ => false,
                })
            }
            sqlparser::ast::FunctionArguments::Subquery(_) => false,
        },
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNotFalse(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::Extract { expr: inner, .. }
        | Expr::Floor { expr: inner, .. }
        | Expr::Ceil { expr: inner, .. } => reads_only(inner, allowed),
        Expr::BinaryOp { left, right, .. } => {
            reads_only(left, allowed) && reads_only(right, allowed)
        }
        Expr::Between {
            expr, low, high, ..
        } => reads_only(expr, allowed) && reads_only(low, allowed) && reads_only(high, allowed),
        Expr::InList { expr, list, .. } => {
            reads_only(expr, allowed) && list.iter().all(|member| reads_only(member, allowed))
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            operand
                .as_deref()
                .is_none_or(|operand| reads_only(operand, allowed))
                && conditions.iter().all(|when| {
                    reads_only(&when.condition, allowed) && reads_only(&when.result, allowed)
                })
                && else_result
                    .as_deref()
                    .is_none_or(|result| reads_only(result, allowed))
        }
        Expr::Subquery(query) => names_no_column(query),
        _ => false,
    }
}

/// Reports whether a subquery names no column at all — `(SELECT COUNT(*)
/// FROM users)` — and so answers the same value beside every group.
///
/// A subquery naming a column may name the outer statement's, which MySQL
/// holds to the grouping like any other, and which of the two tables an
/// unqualified name belongs to is the frontend's to know.
fn names_no_column(query: &sqlparser::ast::Query) -> bool {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    query.with.is_none()
        && select.selection.is_none()
        && select.having.is_none()
        && matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(keys, _) if keys.is_empty())
        && select.from.iter().all(|source| source.joins.is_empty())
        && select.projection.iter().all(|item| {
            matches!(item,
                SelectItem::UnnamedExpr(Expr::Function(function))
                | SelectItem::ExprWithAlias { expr: Expr::Function(function), .. }
                    if static_select_metadata::is_count_call(function)
                        && matches!(&function.args, sqlparser::ast::FunctionArguments::List(arguments)
                            if matches!(arguments.args.as_slice(), [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)])))
        })
}

/// Reports whether two expressions are one expression written twice.
///
/// MySQL matches a projected expression against a key as it reads them, so a
/// function name in another case and a column named with or without its table
/// are the same thing to it — measured on 8.4.11, `SELECT
/// DATE(posts.created_at) ... GROUP BY DATE(created_at)` groups. Anything else
/// has to be written alike.
fn names_the_same_expression(left: &Expr, right: &Expr) -> bool {
    match (left, right) {
        (Expr::Nested(left), right) | (right, Expr::Nested(left)) => {
            names_the_same_expression(left, right)
        }
        (
            Expr::Identifier(_) | Expr::CompoundIdentifier(_),
            Expr::Identifier(_) | Expr::CompoundIdentifier(_),
        ) => match (grouped_column(left), grouped_column(right)) {
            (Some(left), Some(right)) => names_same_column(left, right),
            _ => false,
        },
        (Expr::Function(left), Expr::Function(right)) => {
            let (
                sqlparser::ast::FunctionArguments::List(left_arguments),
                sqlparser::ast::FunctionArguments::List(right_arguments),
            ) = (&left.args, &right.args)
            else {
                return left == right;
            };
            left.name
                .to_string()
                .eq_ignore_ascii_case(&right.name.to_string())
                && left.over.is_none()
                && right.over.is_none()
                && left.filter.is_none()
                && right.filter.is_none()
                && left_arguments.duplicate_treatment == right_arguments.duplicate_treatment
                && left_arguments.clauses == right_arguments.clauses
                && left_arguments.args.len() == right_arguments.args.len()
                && left_arguments
                    .args
                    .iter()
                    .zip(&right_arguments.args)
                    .all(|pair| match pair {
                        (
                            sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Expr(left),
                            ),
                            sqlparser::ast::FunctionArg::Unnamed(
                                sqlparser::ast::FunctionArgExpr::Expr(right),
                            ),
                        ) => names_the_same_expression(left, right),
                        (left, right) => left == right,
                    })
        }
        (left, right) => left == right,
    }
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
