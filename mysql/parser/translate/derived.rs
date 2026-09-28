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
//! A body joining tables is read straight through too, unless the statement
//! drops repeated rows, which MySQL does by writing them into a table of its
//! own.

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
    /// The table each projected column is read from, in order, when the body
    /// joins tables. Empty for a body reading one table.
    joined: Vec<MySqlJoinedDerivedColumn>,
    /// Whether MySQL writes the rows of the statement reading a body that
    /// joins tables into a table of its own first: to drop repeated rows for
    /// its `DISTINCT`, or to sort them by columns of more than one table.
    written_through_a_table: bool,
    /// Whether the body joins a table with an inner join, whose tables MySQL
    /// reads in an order of its own choosing.
    has_an_inner_join: bool,
    /// The one column a `DISTINCT` over a body reading one table reads, which
    /// the frontend holds to being that table's whole primary key.
    distinct_over_one_column: Option<usize>,
}

/// One column of a derived table whose body joins tables, and the table it
/// is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlJoinedDerivedColumn {
    table: MySqlTableName,
    /// The name the body reads the table under — its alias when it gives one.
    reference: String,
    /// The column's own name in its table.
    column: String,
    /// Whether a `LEFT JOIN` can leave the column's row missing.
    outer: bool,
}

impl MySqlJoinedDerivedColumn {
    /// Returns the table the column belongs to.
    pub const fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the name the body reads the column's table under.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Returns the column's own name in its table.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// Reports whether a `LEFT JOIN` can leave the column's row missing.
    pub const fn outer(&self) -> bool {
        self.outer
    }
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

    /// Returns the table each column is read from, when the body joins
    /// tables, or nothing for a body reading one table.
    pub fn joined(&self) -> &[MySqlJoinedDerivedColumn] {
        &self.joined
    }

    /// Reports whether MySQL writes the statement's rows into a table of its
    /// own before it answers them — to drop repeated rows, or to sort them.
    pub const fn written_through_a_table(&self) -> bool {
        self.written_through_a_table
    }

    /// Returns the one column a `DISTINCT` over a body reading one table
    /// reads, which has to be that table's whole primary key.
    pub const fn distinct_over_one_column(&self) -> Option<usize> {
        self.distinct_over_one_column
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
    let select = match super::unwrap_query_wrappers(query.body.as_ref())? {
        SetExpr::Select(select) => select.as_ref(),
        // A `UNION` body was held to branches reading the same columns of one
        // `information_schema` table, so the first branch says what they all
        // project.
        union @ SetExpr::SetOperation { .. } if inner.catalog.is_some() => {
            super::union_branches(union)?.1[0]
        }
        _ => return unsupported("derived table body"),
    };
    if select.distinct.is_some() {
        return unsupported("derived table body with DISTINCT");
    }
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return unsupported("derived table body grouping");
    };
    let aggregates =
        projects_an_aggregate(select) || !group_by.is_empty() || select.having.is_some();
    // Measured on MySQL 8.4.11: a body cut with a `LIMIT` is written out into
    // a table of its own too, whatever that table holds.
    let materialized = aggregates || query.limit_clause.is_some();
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
                    joined: Vec::new(),
                    written_through_a_table: false,
                    has_an_inner_join: false,
                    distinct_over_one_column: None,
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
        if !aggregates {
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
    refuse_two_columns_of_one_name(&names)?;
    Ok((
        projected_columns,
        MySqlDerivedColumns {
            inner_reference,
            materialized,
            names,
            answers,
            joined: Vec::new(),
            written_through_a_table: false,
            has_an_inner_join: false,
            distinct_over_one_column: None,
        },
    ))
}

/// What a derived table the statement only counts the rows of projects: as
/// far as anything reads it, its table's own columns, as a body projecting
/// `*` does.
pub(super) fn only_counted(inner: &MySqlSelectSource) -> MySqlDerivedColumns {
    MySqlDerivedColumns {
        inner_reference: inner.reference.clone(),
        materialized: false,
        names: Vec::new(),
        answers: Vec::new(),
        joined: Vec::new(),
        written_through_a_table: false,
        has_an_inner_join: false,
        distinct_over_one_column: None,
    }
}

/// Returns a derived table's body when it is one `SELECT` joining tables.
pub(super) fn body_joining_tables(
    query: &sqlparser::ast::Query,
) -> Option<&sqlparser::ast::Select> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    select
        .from
        .iter()
        .any(|from| !from.joins.is_empty())
        .then_some(select.as_ref())
}

/// Renders a derived table whose body joins tables, which is how TypeORM
/// pages an entity loaded with its relations: `(SELECT Post.id AS Post_id,
/// ..., Post__Post_tags.name AS Post__Post_tags_name FROM posts Post LEFT
/// JOIN post_tag ... LEFT JOIN tags Post__Post_tags ON ...) distinctAlias`.
///
/// Each column is a column of one of the joined tables, named with its table,
/// so each is traced to that table. A joined table may itself be a derived
/// table joining tables, whose columns are traced through it: Entity Framework
/// Core loads a relation of a relation as `LEFT JOIN (SELECT p.Id, ...,
/// s.Name FROM Posts AS p LEFT JOIN (SELECT p0.PostsId, ..., t.Name FROM
/// PostTags AS p0 INNER JOIN Tags AS t ON p0.TagsId = t.Id) AS s ON p.Id =
/// s.PostsId) AS s0`. An inner join is recorded: MySQL picks its own order for
/// one, which only the statement reading the body can say does not matter. A
/// condition in the body is refused: measured on 8.4.11, `WHERE Post.id = 1`
/// makes MySQL read `posts` as one constant row, and every column then reports
/// another shape — one depending on whether that row exists.
pub(super) fn render_derived_table_joining_tables(
    subquery: &sqlparser::ast::Query,
    select: &sqlparser::ast::Select,
    alias: &sqlparser::ast::TableAlias,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, MySqlSelectSource), ParseError> {
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
        return unsupported("derived table joining tables with a clause of its own");
    }
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return unsupported("derived table joining tables and grouping them");
    };
    if select.distinct.is_some()
        || !group_by.is_empty()
        || select.having.is_some()
        || projects_an_aggregate(select)
    {
        return unsupported("derived table joining tables and grouping them");
    }
    if select.selection.is_some() {
        return unsupported("derived table joining tables under a condition");
    }
    let [from] = select.from.as_slice() else {
        return unsupported("derived table joining tables with a comma");
    };
    // Only an `ON` matching columns against each other, which is what TypeORM
    // and Entity Framework Core write, has been measured.
    let mut has_an_inner_join = false;
    for join in &from.joins {
        match &join.join_operator {
            sqlparser::ast::JoinOperator::Left(sqlparser::ast::JoinConstraint::On(on))
            | sqlparser::ast::JoinOperator::LeftOuter(sqlparser::ast::JoinConstraint::On(on))
                if matches_columns_alone(on) => {}
            sqlparser::ast::JoinOperator::Inner(sqlparser::ast::JoinConstraint::On(on))
            | sqlparser::ast::JoinOperator::Join(sqlparser::ast::JoinConstraint::On(on))
                if matches_columns_alone(on) =>
            {
                has_an_inner_join = true;
            }
            _ => {
                return unsupported(
                    "derived table joining tables other than by [LEFT|INNER] JOIN ... ON columns",
                )
            }
        }
    }
    let comparisons_before = render_context.checked_comparisons.len();
    let reads_a_joined_body =
        std::mem::replace(&mut render_context.renders_a_joined_derived_body, true);
    let body = super::render_select_body(select, render_context);
    render_context.renders_a_joined_derived_body = reads_a_joined_body;
    let (body, sources) = body?;
    let is_a_derived_table_joining_tables = |source: &MySqlSelectSource| {
        source
            .derived
            .as_ref()
            .is_some_and(|derived| !derived.joined.is_empty() && !derived.materialized)
    };
    if sources.iter().any(|source| {
        (source.derived.is_some() && !is_a_derived_table_joining_tables(source))
            || source.catalog.is_some()
            || (source.derived.is_none() && !source.projected_columns.is_empty())
    }) {
        return unsupported("derived table joining anything but tables");
    }
    let references = sources
        .iter()
        .map(|source| source.reference.as_str())
        .collect::<Vec<_>>();
    for comparison in &mut render_context.checked_comparisons[comparisons_before..] {
        comparison.name_the_inner_sources(&references);
    }
    let mut names = Vec::with_capacity(select.projection.len());
    let mut joined = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
            _ => return unsupported("derived table joining tables projecting a wildcard"),
        };
        // A bare name belongs to whichever joined table has the column, which
        // only the frontend can see.
        let Expr::CompoundIdentifier(parts) = expr else {
            return unsupported(
                "derived table joining tables projecting anything but a column named with its table",
            );
        };
        let [table, column] = parts.as_slice() else {
            return unsupported(
                "derived table joining tables projecting a column of another database",
            );
        };
        let Some(source) = sources
            .iter()
            .find(|source| source.reference.eq_ignore_ascii_case(&table.value))
        else {
            return unsupported("derived table projecting a column of a table it does not join");
        };
        names.push(alias.unwrap_or(column).value.clone());
        // A column of a derived table joined in is the column that table read,
        // missing wherever either join can leave its row missing.
        let traced = match &source.derived {
            Some(inner) => {
                let Some(ordinal) = inner
                    .names
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(&column.value))
                else {
                    return unsupported("derived table projecting no column of a derived table");
                };
                has_an_inner_join |= inner.has_an_inner_join;
                let read = &inner.joined[ordinal];
                MySqlJoinedDerivedColumn {
                    table: read.table.clone(),
                    reference: read.reference.clone(),
                    column: read.column.clone(),
                    outer: read.outer || source.outer,
                }
            }
            None => MySqlJoinedDerivedColumn {
                table: source.table.clone(),
                reference: source.reference.clone(),
                column: column.value.clone(),
                outer: source.outer,
            },
        };
        joined.push(traced);
    }
    refuse_two_columns_of_one_name(&names)?;
    let first = sources
        .first()
        .expect("a body joining tables reads a first table")
        .clone();
    for mut source in sources {
        source.subquery = true;
        if !render_context.subquery_tables.iter().any(|held| {
            held.reference == source.reference
                && held.table == source.table
                && held.projected_columns.is_empty()
        }) {
            render_context.subquery_tables.push(source);
        }
    }
    Ok((
        format!("({body}) AS {}", render_ident(&alias.name)),
        MySqlSelectSource {
            reference: alias.name.value.clone(),
            table: first.table,
            outer: false,
            branch: 0,
            subquery: false,
            read_by_a_result_subquery: false,
            projected_columns: joined.iter().map(|column| column.column.clone()).collect(),
            derived: Some(MySqlDerivedColumns {
                inner_reference: first.reference,
                materialized: false,
                answers: vec![None; names.len()],
                names,
                joined,
                written_through_a_table: false,
                has_an_inner_join,
                distinct_over_one_column: None,
            }),
            catalog: None,
            hinted_indexes: Vec::new(),
            sorted_through_a_table: false,
        },
    ))
}

/// Reports whether a join's `ON` matches columns of the tables against each
/// other and nothing else — `a.x = b.y`, alone or joined by `AND`.
fn matches_columns_alone(on: &Expr) -> bool {
    match on {
        Expr::Nested(inner) => matches_columns_alone(inner),
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } => matches_columns_alone(left) && matches_columns_alone(right),
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::Eq,
            right,
        } => {
            matches!(left.as_ref(), Expr::CompoundIdentifier(parts) if parts.len() == 2)
                && matches!(right.as_ref(), Expr::CompoundIdentifier(parts) if parts.len() == 2)
        }
        _ => false,
    }
}

/// Refuses two columns going by one name, which MySQL answers 1060 for.
fn refuse_two_columns_of_one_name(names: &[String]) -> Result<(), ParseError> {
    for (index, name) in names.iter().enumerate() {
        if names[..index]
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(name))
        {
            return unsupported("derived table with two columns of one name");
        }
    }
    Ok(())
}

/// Holds a statement reading a derived table whose body joins tables to the
/// shapes measured over one, and notes whether it drops repeated rows.
///
/// Measured on MySQL 8.4.11: the statement's own `DISTINCT` makes MySQL write
/// the rows into a table of its own, and every column then reports that
/// table's shape, whatever the tables hold — none of them, one row each or
/// thousands. An `ORDER BY` without it decides nothing so steady: MySQL sorts
/// through such a table when it matches a joined table by hash rather than by
/// its key, a choice it makes by how many rows each table holds, so it is
/// refused, and a `LIMIT` with no order is refused with it, since which rows it
/// keeps is each engine's own. Anything but columns of the derived table,
/// `DISTINCT`, an order among the columns it projects and a `LIMIT` has not
/// been measured and is refused.
pub(super) fn hold_the_statement_reading_joined_tables_through_a_derived_table(
    query: &sqlparser::ast::Query,
    sources: &mut [MySqlSelectSource],
) -> Result<(), ParseError> {
    let Some(position) = sources.iter().position(|source| {
        !source.subquery
            && source
                .derived
                .as_ref()
                .is_some_and(|derived| !derived.joined.is_empty())
    }) else {
        return Ok(());
    };
    let has_an_inner_join = sources[position]
        .derived
        .as_ref()
        .is_some_and(|derived| derived.has_an_inner_join);
    if sources.iter().filter(|source| !source.subquery).count() != 1 || has_an_inner_join {
        return hold_a_statement_sorted_through_a_table(query, sources);
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("derived table joining tables in a set operation");
    };
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return unsupported("statement grouping a derived table joining tables");
    };
    if query.with.is_some()
        || !query.locks.is_empty()
        || select.selection.is_some()
        || !group_by.is_empty()
        || select.having.is_some()
    {
        return unsupported("statement narrowing or grouping a derived table joining tables");
    }
    let reference = sources[position].reference.clone();
    let derived = sources[position]
        .derived
        .as_mut()
        .expect("the derived table was found above");
    let Some(projection) = columns_the_statement_reads(select, &reference, &derived.names) else {
        return unsupported(
            "statement reading anything but the columns of a derived table joining tables",
        );
    };
    let drops_repeats = select.distinct.is_some();
    match &query.order_by {
        Some(_) if !drops_repeats => {
            return unsupported("ORDER BY over a derived table joining tables without DISTINCT");
        }
        Some(order_by) => {
            let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
                return unsupported("SELECT ORDER BY option");
            };
            for expression in expressions {
                if projected_column_ordered_by(
                    &expression.expr,
                    &reference,
                    &derived.names,
                    &projection,
                )
                .is_none()
                {
                    return unsupported(
                        "ORDER BY naming what a DISTINCT over a derived table joining tables does not project",
                    );
                }
            }
        }
        None if query.limit_clause.is_some() => {
            return unsupported("LIMIT without ORDER BY over a derived table joining tables");
        }
        None => {}
    }
    derived.written_through_a_table = drops_repeats;
    Ok(())
}

/// Holds a statement joining a derived table that joins tables beside other
/// tables, or one whose body joins a table with an inner join, to the one
/// shape measured of it: sorted through a table of MySQL's own.
///
/// Entity Framework Core loads relations that way — `SELECT u.Id, ...,
/// s0.Name FROM Users AS u LEFT JOIN (SELECT p.Id, ... FROM Posts AS p LEFT
/// JOIN (SELECT p0.PostsId, ..., t.Name FROM PostTags AS p0 INNER JOIN Tags AS
/// t ON p0.TagsId = t.Id) AS s ON p.Id = s.PostsId) AS s0 ON u.Id = s0.UserId
/// ORDER BY u.Id, s0.Id, s0.PostsId, s0.TagsId`. Measured on MySQL 8.4.11 over
/// no rows, three and thousands: ordered by columns of more than one of the
/// tables it reads, MySQL writes the rows into a table of its own to sort
/// them, only the first table it reads being able to hand rows over in an
/// order, and every column then reports that table's column: without keys, a
/// derived table's naming no database and its own table by name. A condition
/// could make MySQL read a table as one constant row, which reports another
/// shape; without an order spanning two tables which table MySQL reads first
/// decides the shape, and it picks that by how many rows each holds. Both are
/// refused, and so is anything but columns in the projection and the order.
fn hold_a_statement_sorted_through_a_table(
    query: &sqlparser::ast::Query,
    sources: &mut [MySqlSelectSource],
) -> Result<(), ParseError> {
    if let Err(feature) = sorted_across_its_tables(query, sources) {
        return unsupported(feature);
    }
    note_the_sort_through_a_table(sources);
    Ok(())
}

/// Notes a statement joining tables alone that MySQL sorts through a table
/// of its own — Entity Framework Core's split query reads `... FROM Users AS
/// u INNER JOIN Posts AS p ON u.Id = p.UserId ORDER BY u.Id, p.Id` — for the
/// reasons [`hold_a_statement_sorted_through_a_table`] gives. Measured on
/// MySQL 8.4.11 over no rows, three and thousands, every column then loses
/// its keys. Any other statement is left as it is.
pub(super) fn note_a_join_sorted_across_its_tables(
    query: &sqlparser::ast::Query,
    sources: &mut [MySqlSelectSource],
) {
    let read = sources
        .iter()
        .filter(|source| is_read_by_the_statement(source))
        .collect::<Vec<_>>();
    if read.len() < 2 || read.iter().any(|source| source.derived.is_some()) {
        return;
    }
    if sorted_across_its_tables(query, sources).is_ok() {
        note_the_sort_through_a_table(sources);
    }
}

/// Answers why MySQL might not sort a statement's rows through a table of its
/// own, or nothing when it does: see [`hold_a_statement_sorted_through_a_table`].
fn sorted_across_its_tables(
    query: &sqlparser::ast::Query,
    sources: &[MySqlSelectSource],
) -> Result<(), &'static str> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err("statement joining tables in a set operation");
    };
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return Err("statement grouping the tables it joins");
    };
    if query.with.is_some()
        || !query.locks.is_empty()
        || select.selection.is_some()
        || !group_by.is_empty()
        || select.having.is_some()
        || select.distinct.is_some()
    {
        return Err("statement narrowing, grouping or dropping repeats of the tables it joins");
    }
    // A column is traced through the name of the table it is read from, so
    // two tables read under one name would leave it unsaid which.
    for (at, source) in sources.iter().enumerate() {
        if sources[..at]
            .iter()
            .any(|earlier| earlier.reference.eq_ignore_ascii_case(&source.reference))
        {
            return Err("statement reading two tables under one name");
        }
    }
    for source in sources
        .iter()
        .filter(|source| is_read_by_the_statement(source))
    {
        let a_table = source.derived.is_none()
            && source.catalog.is_none()
            && source.projected_columns.is_empty();
        let joining_tables = source
            .derived
            .as_ref()
            .is_some_and(|derived| !derived.joined.is_empty() && !derived.materialized);
        if !a_table && !joining_tables {
            return Err("statement joining a derived table beside anything but tables");
        }
    }
    for item in &select.projection {
        let (SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) = item else {
            return Err("statement joining tables projecting a wildcard");
        };
        if table_read_for(expr, sources).is_none() {
            return Err("statement joining tables projecting anything but their columns");
        }
    }
    let Some(sqlparser::ast::OrderBy {
        kind: sqlparser::ast::OrderByKind::Expressions(expressions),
        ..
    }) = &query.order_by
    else {
        return Err("statement joining a derived table beside another table unordered");
    };
    let mut ordering_tables = Vec::new();
    for expression in expressions {
        let Some(table) = table_read_for(&expression.expr, sources) else {
            return Err("statement joining tables ordered by anything but their columns");
        };
        if !ordering_tables.contains(&table) {
            ordering_tables.push(table);
        }
    }
    if ordering_tables.len() < 2 {
        return Err("statement joining a derived table ordered by one table's columns");
    }
    Ok(())
}

fn note_the_sort_through_a_table(sources: &mut [MySqlSelectSource]) {
    for source in sources
        .iter_mut()
        .filter(|source| is_read_by_the_statement(source))
    {
        match &mut source.derived {
            Some(derived) => derived.written_through_a_table = true,
            None => source.sorted_through_a_table = true,
        }
    }
}

fn is_read_by_the_statement(source: &MySqlSelectSource) -> bool {
    !source.subquery && source.branch == 0
}

/// The name of the table a column the statement names is read from — `u.Id`
/// from `u`, and `s0.Id0` from the `t` its derived table read it from — or
/// nothing for anything but a column of a table the statement reads.
fn table_read_for(expr: &Expr, sources: &[MySqlSelectSource]) -> Option<String> {
    let Expr::CompoundIdentifier(parts) = expr else {
        return None;
    };
    let [table, column] = parts.as_slice() else {
        return None;
    };
    let source = sources.iter().find(|source| {
        is_read_by_the_statement(source) && source.reference.eq_ignore_ascii_case(&table.value)
    })?;
    let reference = match &source.derived {
        None => &source.reference,
        Some(derived) => {
            let ordinal = derived
                .names
                .iter()
                .position(|name| name.eq_ignore_ascii_case(&column.value))?;
            &derived.joined[ordinal].reference
        }
    };
    Some(reference.to_ascii_lowercase())
}

/// Holds a `DISTINCT` over a derived table or a CTE reading one table to the
/// one shape measured of it.
///
/// Measured on MySQL 8.4.11, how MySQL drops the repeated rows decides every
/// column's shape, and how it drops them turns on the statement's order and on
/// the table's keys. Ordered by the table's primary key, it reads the key in
/// order and the columns are read straight through; otherwise it writes the
/// rows into a table of its own — `SELECT DISTINCT x.i FROM (SELECT p.id AS
/// i, p.title AS t FROM posts p) x` names `posts`, `i` and no database —
/// unless an index on the column serves, and a nullable column in that table
/// carries a flag of its own. So a `DISTINCT` reading anything but one
/// column, ordered by that column first, is refused, and the frontend holds
/// the column to being its table's whole primary key: TypeORM's pagination of
/// an entity, `SELECT DISTINCT distinctAlias.User_id AS ids_User_id FROM
/// (...) distinctAlias ORDER BY User_id ASC LIMIT 10`, is that shape.
pub(super) fn hold_a_distinct_reading_one_table_through_a_derived_table(
    query: &sqlparser::ast::Query,
    sources: &mut [MySqlSelectSource],
) -> Result<(), ParseError> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(());
    };
    if select.distinct.is_none() {
        return Ok(());
    }
    let Some(position) = sources.iter().position(|source| {
        !source.subquery
            && source
                .derived
                .as_ref()
                .is_some_and(|derived| derived.joined.is_empty() && !derived.materialized)
    }) else {
        return Ok(());
    };
    if sources.iter().filter(|source| !source.subquery).count() != 1 {
        return unsupported("DISTINCT over a derived table beside another table");
    }
    let reference = sources[position].reference.clone();
    let derived = sources[position]
        .derived
        .as_mut()
        .expect("the derived table was found above");
    let Some(projection) = columns_the_statement_reads(select, &reference, &derived.names) else {
        return unsupported("DISTINCT over a derived table reading anything but its columns");
    };
    let Some((&column, others)) = projection.ordinals.split_first() else {
        return unsupported("SELECT without projections");
    };
    if others.iter().any(|other| *other != column) {
        return unsupported("DISTINCT over a derived table reading more than one of its columns");
    }
    let first_order = query
        .order_by
        .as_ref()
        .and_then(|order_by| match &order_by.kind {
            sqlparser::ast::OrderByKind::Expressions(expressions) => expressions.first(),
            sqlparser::ast::OrderByKind::All(_) => None,
        });
    if first_order.and_then(|order| {
        projected_column_ordered_by(&order.expr, &reference, &derived.names, &projection)
    }) != Some(column)
    {
        return unsupported("DISTINCT over a derived table not ordered by the column it reads");
    }
    derived.distinct_over_one_column = Some(column);
    Ok(())
}

/// What a statement reading only a derived table's columns projects: each
/// result column's name and the place among the derived table's columns of
/// the one it reads.
struct ReadColumns {
    result_names: Vec<String>,
    ordinals: Vec<usize>,
}

/// Reads a statement's projection as columns of one derived table, or
/// nothing when it projects anything else.
fn columns_the_statement_reads(
    select: &sqlparser::ast::Select,
    reference: &str,
    names: &[String],
) -> Option<ReadColumns> {
    let mut read = ReadColumns {
        result_names: Vec::with_capacity(select.projection.len()),
        ordinals: Vec::with_capacity(select.projection.len()),
    };
    for item in &select.projection {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
            _ => return None,
        };
        let ordinal = derived_column_named(expr, reference, names)?;
        read.result_names
            .push(alias.map_or_else(|| names[ordinal].clone(), |alias| alias.value.clone()));
        read.ordinals.push(ordinal);
    }
    Some(read)
}

/// Returns the place among a derived table's columns of the projected column
/// an `ORDER BY` term names: by its place, by the name it answers under, or
/// as the derived table's column it reads. MySQL answers 3065 for any other
/// beside a `DISTINCT`.
fn projected_column_ordered_by(
    expr: &Expr,
    reference: &str,
    names: &[String],
    projection: &ReadColumns,
) -> Option<usize> {
    if let Expr::Value(value) = expr {
        let sqlparser::ast::Value::Number(place, _) = &value.value else {
            return None;
        };
        let place = place.parse::<usize>().ok()?;
        return place
            .checked_sub(1)
            .and_then(|index| projection.ordinals.get(index))
            .copied();
    }
    if let Expr::Identifier(name) = expr {
        if let Some(index) = projection
            .result_names
            .iter()
            .position(|result| result.eq_ignore_ascii_case(&name.value))
        {
            return Some(projection.ordinals[index]);
        }
    }
    derived_column_named(expr, reference, names)
        .filter(|ordinal| projection.ordinals.contains(ordinal))
}

/// Returns the place among a derived table's columns of the one an expression
/// names, bare or with the derived table's name.
fn derived_column_named(expr: &Expr, reference: &str, names: &[String]) -> Option<usize> {
    let name = match expr {
        Expr::Identifier(name) => name,
        Expr::CompoundIdentifier(parts)
            if parts.len() == 2 && parts[0].value.eq_ignore_ascii_case(reference) =>
        {
            &parts[1]
        }
        _ => return None,
    };
    names
        .iter()
        .position(|candidate| candidate.eq_ignore_ascii_case(&name.value))
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
            | StaticSelectMetadata::AggregateAsWholeNumber(_)
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
        // A column of a derived table joining tables is the column of the
        // table its body read it from, which is what it is held to.
        if let Some(qualifier) = &mut comparison.qualifier {
            trace_through_a_derived_table_joining_tables(
                qualifier,
                &mut comparison.column_name,
                sources,
            )?;
        }
        if let CheckedSelectComparisonRhs::Column {
            qualifier: Some(qualifier),
            name,
        } = &mut comparison.rhs
        {
            trace_through_a_derived_table_joining_tables(qualifier, name, sources)?;
        }
        if !comparison.inner_sources.is_empty() || comparison.answers.is_some() {
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

/// Names a column of a derived table joining tables — `s0.UserId` — by the
/// table its body read it from and its own name there — `p.UserId` — and
/// leaves any other column as it is.
fn trace_through_a_derived_table_joining_tables(
    qualifier: &mut String,
    column: &mut String,
    sources: &[MySqlSelectSource],
) -> Result<(), ParseError> {
    let Some(derived) = sources
        .iter()
        .find(|source| source.reference.eq_ignore_ascii_case(qualifier))
        .and_then(|source| source.derived.as_ref())
        .filter(|derived| !derived.joined.is_empty())
    else {
        return Ok(());
    };
    let Some(ordinal) = derived
        .names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(column))
    else {
        return unsupported("comparison naming no column of a derived table joining tables");
    };
    let read = &derived.joined[ordinal];
    qualifier.clone_from(&read.reference);
    column.clone_from(&read.column);
    Ok(())
}
