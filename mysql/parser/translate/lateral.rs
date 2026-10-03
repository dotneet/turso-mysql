//! A `LATERAL` derived table that answers one JSON document for each row the
//! statement reads, which is how Drizzle's relational queries put a relation
//! into one result column: `LEFT JOIN LATERAL (SELECT COALESCE(JSON_ARRAYAGG(
//! JSON_ARRAY(p.id, p.title)), JSON_ARRAY()) AS data FROM posts p WHERE
//! p.user_id = users.id) users_posts ON TRUE`.
//!
//! A body that aggregates without a `GROUP BY` answers exactly one row, and
//! one reading a derived table cut by `LIMIT 1` answers at most one. Joined
//! with `LEFT JOIN ... ON TRUE`, every row of the statement meets that row or
//! none, so `users_posts.data` is the scalar subquery written in its place,
//! and that is how the statement is rendered: the join is left out and each
//! result column naming the lateral table reads the subquery.
//!
//! Measured on MySQL 8.4.11: a body that aggregates is written out into a
//! table of its own, and its column is that table's `JSON` column — a length
//! of 4294967295, no decimals, the binary collation and the `BLOB` and
//! `BINARY` flags. A body reading a derived table cut to one row is read
//! through, and its column reports what `JSON_ARRAY` reports on its own. Both
//! name the lateral table and the body's own name for the column, and no
//! database or original table. The documents are built the way MySQL builds
//! them: `[[1, 1, [1, "news"]], [1, 2, [2, "rust"]]]`, an empty relation `[]`
//! and a missing one `null`.

use super::*;

/// The statement's own table and the lateral derived tables joined to it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LateralTables {
    outer_reference: String,
    documents: Vec<LateralDocument>,
}

/// One lateral derived table, which projects one column.
#[derive(Debug, Clone)]
struct LateralDocument {
    alias: Ident,
    column: Ident,
    body: sqlparser::ast::Query,
}

/// A column of a lateral derived table as the statement's result reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LateralColumn {
    pub(crate) table: String,
    pub(crate) column: String,
    pub(crate) shape: LateralShape,
    /// Each column of a table the document is built out of, at every depth,
    /// as the reference the body reads it under and its name.
    pub(crate) columns_written_into_it: Vec<(String, String)>,
}

impl LateralTables {
    fn document_named(&self, alias: &str) -> Option<&LateralDocument> {
        self.documents
            .iter()
            .find(|document| document.alias.value.eq_ignore_ascii_case(alias))
    }

    pub(crate) fn names(&self, alias: &str) -> bool {
        self.document_named(alias).is_some()
    }
}

/// Takes every lateral derived table out of the statement's `FROM`, leaving
/// the statement reading its one table, and answers them.
///
/// Only a statement reading one table and joining nothing but lateral
/// derived tables to it is taken, and the statement may name a lateral table
/// only as a result column of its own: anywhere else the column is not the
/// subquery written in its place.
pub(crate) fn take_lateral_tables_out_of_the_from(
    query: &mut sqlparser::ast::Query,
) -> Result<LateralTables, ParseError> {
    let SetExpr::Select(select) = query.body.as_mut() else {
        return Ok(LateralTables::default());
    };
    let joins_a_lateral_table = select
        .from
        .iter()
        .flat_map(|from| &from.joins)
        .any(|join| matches!(join.relation, TableFactor::Derived { lateral: true, .. }));
    if !joins_a_lateral_table {
        return Ok(LateralTables::default());
    }
    let [from] = select.from.as_mut_slice() else {
        return unsupported("LATERAL derived table beside a second table");
    };
    let outer_reference = match &from.relation {
        TableFactor::Table {
            alias: Some(alias), ..
        } => alias.name.value.clone(),
        TableFactor::Table { name, .. } => match name.0.as_slice() {
            [ObjectNamePart::Identifier(table)] => table.value.clone(),
            _ => return unsupported("LATERAL derived table beside a qualified table"),
        },
        _ => return unsupported("LATERAL derived table beside a table that is not a base table"),
    };
    let documents = std::mem::take(&mut from.joins)
        .into_iter()
        .map(lateral_document)
        .collect::<Result<Vec<_>, _>>()?;
    for (place, document) in documents.iter().enumerate() {
        let alias = &document.alias.value;
        if alias.eq_ignore_ascii_case(&outer_reference)
            || documents[..place]
                .iter()
                .any(|earlier| earlier.alias.value.eq_ignore_ascii_case(alias))
        {
            return unsupported("LATERAL derived table sharing its name with another table");
        }
    }
    let tables = LateralTables {
        outer_reference,
        documents,
    };
    let mut elsewhere = Vec::new();
    if let Some(selection) = &select.selection {
        elsewhere.push(selection.to_string());
    }
    if let Some(having) = &select.having {
        elsewhere.push(having.to_string());
    }
    if let sqlparser::ast::GroupByExpr::Expressions(keys, _) = &select.group_by {
        elsewhere.extend(keys.iter().map(ToString::to_string));
    }
    if let Some(order_by) = &query.order_by {
        elsewhere.push(order_by.to_string());
    }
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                match lateral_column_named(expr, &tables) {
                    Some((document, column)) => {
                        if !column.value.eq_ignore_ascii_case(&document.column.value) {
                            return unsupported("column a LATERAL derived table does not project");
                        }
                    }
                    None => elsewhere.push(expr.to_string()),
                }
            }
            _ => return unsupported("wildcard beside a LATERAL derived table"),
        }
    }
    if elsewhere
        .iter()
        .any(|written| names_a_lateral_table(written, &tables))
    {
        return unsupported("LATERAL derived table named outside the result columns");
    }
    Ok(tables)
}

fn lateral_document(join: sqlparser::ast::Join) -> Result<LateralDocument, ParseError> {
    let (sqlparser::ast::JoinOperator::Left(sqlparser::ast::JoinConstraint::On(on))
    | sqlparser::ast::JoinOperator::LeftOuter(sqlparser::ast::JoinConstraint::On(on))) =
        &join.join_operator
    else {
        return unsupported("LATERAL derived table joined other than by LEFT JOIN");
    };
    if join.global || !is_written_true(on) {
        return unsupported("LATERAL derived table joined on anything but TRUE");
    }
    let TableFactor::Derived {
        lateral: true,
        subquery,
        alias: Some(alias),
        sample: None,
    } = join.relation
    else {
        return unsupported("LATERAL join of something other than a named derived table");
    };
    if !alias.columns.is_empty() || alias.at.is_some() {
        return unsupported("LATERAL derived table naming its own columns");
    }
    let SetExpr::Select(select) = subquery.body.as_ref() else {
        return unsupported("LATERAL derived table whose body is not one SELECT");
    };
    let [SelectItem::ExprWithAlias { alias: column, .. }] = select.projection.as_slice() else {
        return unsupported("LATERAL derived table projecting other than one named column");
    };
    Ok(LateralDocument {
        alias: alias.name,
        column: column.clone(),
        body: *subquery,
    })
}

fn is_written_true(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(value.value, Value::Boolean(true)),
        Expr::Nested(inner) => is_written_true(inner),
        _ => false,
    }
}

fn lateral_column_named<'a>(
    expr: &'a Expr,
    tables: &'a LateralTables,
) -> Option<(&'a LateralDocument, &'a Ident)> {
    let Expr::CompoundIdentifier(parts) = expr else {
        return None;
    };
    let [table, column] = parts.as_slice() else {
        return None;
    };
    tables
        .document_named(&table.value)
        .map(|document| (document, column))
}

/// Whether a lateral table's name is written anywhere in a clause, as a word
/// rather than inside a quoted string.
fn names_a_lateral_table(written: &str, tables: &LateralTables) -> bool {
    let Ok(tokens) = sqlparser::tokenizer::Tokenizer::new(&MySqlDialect {}, written).tokenize()
    else {
        return true;
    };
    tokens.iter().any(|token| {
        matches!(token, sqlparser::tokenizer::Token::Word(word) if tables.names(&word.value))
    })
}

/// Renders a result column naming a lateral table as the subquery the table's
/// body is, under the column's own name, and notes what it reports.
pub(super) fn render_lateral_column(
    item: &SelectItem,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    let (expr, name) = match item {
        SelectItem::UnnamedExpr(expr) => (expr, None),
        SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
        _ => return Ok(None),
    };
    let Some((document, column)) = lateral_column_named(expr, &render_context.lateral_tables)
    else {
        return Ok(None);
    };
    let (document, column) = (document.clone(), column.clone());
    let outer_reference = render_context.lateral_tables.outer_reference.clone();
    let mut written = Written::default();
    let (sql, shape) = render_body(
        &document.body,
        &[outer_reference.as_str()],
        render_context,
        &mut written,
    )?;
    for source in written.sources {
        if !render_context.subquery_tables.contains(&source) {
            render_context.subquery_tables.push(source);
        }
    }
    render_context.lateral_columns.push(LateralColumn {
        table: document.alias.value.clone(),
        column: document.column.value.clone(),
        shape,
        columns_written_into_it: written.columns,
    });
    if !render_context.knows_every_column_kind {
        render_context.renders_a_lateral_table_without_column_kinds = true;
    }
    let name = name.unwrap_or(&column);
    Ok(Some(format!("{sql} AS {}", render_ident(name))))
}

/// What rendering a body has read: the tables, and the columns written into
/// a document.
#[derive(Default)]
struct Written {
    sources: Vec<MySqlSelectSource>,
    columns: Vec<(String, String)>,
}

/// A body rendered as the subquery standing in for its one column, with the
/// shape MySQL reports for that column.
fn render_body(
    query: &sqlparser::ast::Query,
    scope: &[&str],
    render_context: &mut SelectRenderContext<'_>,
    written: &mut Written,
) -> Result<(String, LateralShape), ParseError> {
    if !has_no_clause_of_its_own(query) || query.limit_clause.is_some() {
        return unsupported("LATERAL derived table body clause");
    }
    let select = plain_select(query)?;
    let [SelectItem::ExprWithAlias { expr, .. }] = select.projection.as_slice() else {
        return unsupported("LATERAL derived table projecting other than one named column");
    };
    let [sqlparser::ast::TableWithJoins { relation, joins }] = select.from.as_slice() else {
        return unsupported("LATERAL derived table body reading other than one table");
    };
    let source = render_source(relation, scope, render_context, written)?;
    let mut inner_scope = scope.to_vec();
    inner_scope.push(source.reference.as_str());
    let mut nested = Vec::new();
    for join in joins {
        let document = lateral_document(join.clone())?;
        let alias = &document.alias.value;
        if inner_scope
            .iter()
            .any(|named| named.eq_ignore_ascii_case(alias))
            || nested
                .iter()
                .any(|(named, _, _): &(String, String, String)| named.eq_ignore_ascii_case(alias))
        {
            return unsupported("LATERAL derived table sharing its name with another table");
        }
        let (sql, _) = render_body(&document.body, &inner_scope, render_context, written)?;
        nested.push((document.alias.value, document.column.value, sql));
    }
    let condition = match &select.selection {
        Some(condition) => format!(
            " WHERE {}",
            render_correlation(condition, &source.reference, scope, render_context)?
        ),
        None => String::new(),
    };
    let reads = DocumentReads {
        reference: &source.reference,
        nested: &nested,
    };
    match aggregated_document(expr)? {
        Some((built, falls_back)) => {
            let built = render_document(built, &reads, render_context, written)?;
            let fallback = if falls_back { "'[]'" } else { "NULL" };
            Ok((
                format!(
                    "(SELECT CASE WHEN count(*) = 0 THEN {fallback} ELSE mysql_json_document(json_group_array(json({built}))) END FROM {}{condition})",
                    source.sql
                ),
                LateralShape::Stored,
            ))
        }
        None => {
            let Expr::Function(built) = expr else {
                return unsupported("LATERAL derived table projecting other than a document");
            };
            if !source.at_most_one_row {
                return unsupported("LATERAL derived table answering more than one row");
            }
            let built = render_document(built, &reads, render_context, written)?;
            Ok((
                format!("(SELECT {built} FROM {}{condition})", source.sql),
                LateralShape::Built,
            ))
        }
    }
}

fn has_no_clause_of_its_own(query: &sqlparser::ast::Query) -> bool {
    query.with.is_none()
        && query.order_by.is_none()
        && query.fetch.is_none()
        && query.locks.is_empty()
        && query.for_clause.is_none()
        && query.settings.is_none()
        && query.format_clause.is_none()
        && query.pipe_operators.is_empty()
}

fn plain_select(query: &sqlparser::ast::Query) -> Result<&sqlparser::ast::Select, ParseError> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("LATERAL derived table body that is not one SELECT");
    };
    let groups_nothing = matches!(
        &select.group_by,
        sqlparser::ast::GroupByExpr::Expressions(keys, modifiers)
            if keys.is_empty() && modifiers.is_empty()
    );
    if !groups_nothing
        || !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
    {
        return unsupported("LATERAL derived table body feature");
    }
    Ok(select)
}

/// The table a body reads, rendered, under the name the body reads it by.
struct RenderedSource {
    sql: String,
    reference: String,
    at_most_one_row: bool,
}

/// Renders the one table a body reads: a base table, or a derived table
/// reading every column of one base table — Drizzle numbers the rows of an
/// ordered relation, `(SELECT *, ROW_NUMBER() OVER (ORDER BY p.id) FROM posts
/// p WHERE ...)`, and reads a relation to one row through `(SELECT * FROM
/// users u WHERE ... LIMIT 1)`.
///
/// Measured on MySQL 8.4.11: the rows reach `JSON_ARRAYAGG` in the order the
/// window numbers them, and a `LIMIT` keeps the first rows in that order.
fn render_source(
    relation: &TableFactor,
    scope: &[&str],
    render_context: &mut SelectRenderContext<'_>,
    written: &mut Written,
) -> Result<RenderedSource, ParseError> {
    match relation {
        TableFactor::Table { .. } => {
            let (sql, source) = read_base_table(relation, written)?;
            Ok(RenderedSource {
                sql,
                reference: source,
                at_most_one_row: false,
            })
        }
        TableFactor::Derived {
            lateral: false,
            subquery,
            alias: Some(alias),
            sample: None,
        } if alias.columns.is_empty() && alias.at.is_none() => {
            if !has_no_clause_of_its_own(subquery) {
                return unsupported("LATERAL derived table body reading a derived table clause");
            }
            let select = plain_select(subquery)?;
            let [sqlparser::ast::TableWithJoins {
                relation: table,
                joins,
            }] = select.from.as_slice()
            else {
                return unsupported("derived table in a LATERAL body reading other than one table");
            };
            if !joins.is_empty() || !matches!(table, TableFactor::Table { .. }) {
                return unsupported("derived table in a LATERAL body reading other than one table");
            }
            let (table_sql, inner_reference) = read_base_table(table, written)?;
            written
                .sources
                .last_mut()
                .expect("a base table read was just noted")
                .reference
                .clone_from(&alias.name.value);
            let window = match select.projection.as_slice() {
                [SelectItem::Wildcard(options)] if wildcard_options_are_empty(options) => None,
                [SelectItem::Wildcard(options), SelectItem::UnnamedExpr(Expr::Function(numbered))]
                    if wildcard_options_are_empty(options) =>
                {
                    Some(render_row_number(
                        numbered,
                        &inner_reference,
                        render_context,
                    )?)
                }
                _ => return unsupported("derived table in a LATERAL body projecting other than *"),
            };
            let condition = match &select.selection {
                Some(condition) => format!(
                    " WHERE {}",
                    render_correlation(condition, &inner_reference, scope, render_context)?
                ),
                None => String::new(),
            };
            let (limit, at_most_one_row) = match &subquery.limit_clause {
                None => (String::new(), false),
                Some(clause) => {
                    let (count, offset) = written_row_count(clause)?;
                    if count != 1 && window.is_none() {
                        return unsupported(
                            "derived table in a LATERAL body cut to rows in no order",
                        );
                    }
                    (format!(" LIMIT {count} OFFSET {offset}"), count <= 1)
                }
            };
            Ok(RenderedSource {
                sql: format!(
                    "(SELECT *{} FROM {table_sql}{condition}{limit}) AS {}",
                    window
                        .map(|window| format!(", {window}"))
                        .unwrap_or_default(),
                    render_ident(&alias.name)
                ),
                reference: alias.name.value.clone(),
                at_most_one_row,
            })
        }
        _ => unsupported("LATERAL derived table body reading something other than a table"),
    }
}

fn read_base_table(
    relation: &TableFactor,
    written: &mut Written,
) -> Result<(String, String), ParseError> {
    let (sql, mut source) = render_select_table(relation, None)?;
    if source.catalog.is_some() || !source.hinted_indexes.is_empty() {
        return unsupported("LATERAL derived table body reading a catalog table or with a hint");
    }
    source.subquery = true;
    let reference = source.reference.clone();
    written.sources.push(source);
    Ok((sql, reference))
}

/// `ROW_NUMBER() OVER (ORDER BY ...)` over whole-number columns of the
/// derived table's own table, which says the order the body's rows are read
/// in and nothing else.
fn render_row_number(
    function: &sqlparser::ast::Function,
    reference: &str,
    render_context: &SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let takes_nothing = matches!(
        &function.args,
        sqlparser::ast::FunctionArguments::List(list)
            if list.args.is_empty() && list.duplicate_treatment.is_none() && list.clauses.is_empty()
    );
    let Some(sqlparser::ast::WindowType::WindowSpec(window)) = &function.over else {
        return unsupported("derived table in a LATERAL body projecting other than *");
    };
    if !is_named(function, "ROW_NUMBER")
        || !takes_nothing
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || window.window_name.is_some()
        || !window.partition_by.is_empty()
        || window.window_frame.is_some()
        || window.order_by.is_empty()
    {
        return unsupported("derived table in a LATERAL body numbering rows other than in order");
    }
    let order = window
        .order_by
        .iter()
        .map(|item| {
            let column = own_column(&item.expr, reference).ok_or(ParseError::Unsupported {
                feature: "LATERAL body ordered by other than its own column",
            })?;
            if item.with_fill.is_some() || item.options.nulls_first.is_some() {
                return unsupported("LATERAL body ordered with an option");
            }
            hold_to_a_whole_number(column, render_context)?;
            Ok(format!(
                "{}.{}{}",
                render_ident_str(reference),
                render_ident(column),
                match item.options.asc {
                    Some(false) => " DESC",
                    _ => " ASC",
                }
            ))
        })
        .collect::<Result<Vec<_>, ParseError>>()?;
    Ok(format!("row_number() OVER (ORDER BY {})", order.join(", ")))
}

fn written_row_count(clause: &sqlparser::ast::LimitClause) -> Result<(u64, u64), ParseError> {
    let (count, offset) = match clause {
        sqlparser::ast::LimitClause::LimitOffset {
            limit: Some(count),
            offset,
            limit_by,
        } if limit_by.is_empty() => (count, offset.as_ref().map(|offset| &offset.value)),
        sqlparser::ast::LimitClause::OffsetCommaLimit { offset, limit } => (limit, Some(offset)),
        _ => return unsupported("LATERAL body row count"),
    };
    let written_number = |expr: &Expr| match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(digits, false) => digits.parse::<u64>().ok(),
            _ => None,
        },
        _ => None,
    };
    let count = written_number(count).ok_or(ParseError::Unsupported {
        feature: "LATERAL body row count other than a written number",
    })?;
    let offset = match offset {
        Some(offset) => written_number(offset).ok_or(ParseError::Unsupported {
            feature: "LATERAL body row count other than a written number",
        })?,
        None => 0,
    };
    Ok((count, offset))
}

/// Renders how a body's rows are matched to the row reading them: columns of
/// whole numbers of its own table equal to columns of a table read outside
/// it, joined by `AND`.
fn render_correlation(
    condition: &Expr,
    reference: &str,
    scope: &[&str],
    render_context: &SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match condition {
        Expr::Nested(inner) => render_correlation(inner, reference, scope, render_context),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => Ok(format!(
            "({} AND {})",
            render_correlation(left, reference, scope, render_context)?,
            render_correlation(right, reference, scope, render_context)?
        )),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let (Expr::CompoundIdentifier(first), Expr::CompoundIdentifier(second)) =
                (left.as_ref(), right.as_ref())
            else {
                return unsupported("LATERAL body matching other than two columns");
            };
            let ([first_table, first_column], [second_table, second_column]) =
                (first.as_slice(), second.as_slice())
            else {
                return unsupported("LATERAL body matching other than two columns");
            };
            let in_scope = |table: &Ident| {
                scope
                    .iter()
                    .any(|named| named.eq_ignore_ascii_case(&table.value))
            };
            let own = |table: &Ident| table.value.eq_ignore_ascii_case(reference);
            if !(own(first_table) && in_scope(second_table)
                || in_scope(first_table) && own(second_table))
            {
                return unsupported(
                    "LATERAL body matching other than its own column with an outer one",
                );
            }
            if render_context.knows_every_column_kind
                && hold_to_a_whole_number(first_column, render_context)?
                    != hold_to_a_whole_number(second_column, render_context)?
            {
                return unsupported("LATERAL body matching whole numbers of two kinds");
            }
            Ok(format!(
                "{}.{} = {}.{}",
                render_ident(first_table),
                render_ident(first_column),
                render_ident(second_table),
                render_ident(second_column)
            ))
        }
        _ => unsupported("LATERAL body condition other than matching columns"),
    }
}

/// Holds a column to whole numbers once the kinds are known, and answers
/// whether it is one the engine keeps in a stored form of its own — a
/// `BIGINT UNSIGNED`.
fn hold_to_a_whole_number(
    column: &Ident,
    render_context: &SelectRenderContext<'_>,
) -> Result<bool, ParseError> {
    if !render_context.knows_every_column_kind {
        return Ok(false);
    }
    match render_context.decimal_scale(&column.value) {
        Some(0) => Ok(true),
        Some(_) => unsupported("LATERAL body reading a DECIMAL with places as a whole number"),
        None if render_context.is_integer_column(&column.value) => Ok(false),
        None => unsupported("LATERAL body reading other than whole numbers"),
    }
}

fn own_column<'a>(expr: &'a Expr, reference: &str) -> Option<&'a Ident> {
    let Expr::CompoundIdentifier(parts) = expr else {
        return None;
    };
    match parts.as_slice() {
        [table, column] if table.value.eq_ignore_ascii_case(reference) => Some(column),
        _ => None,
    }
}

/// `COALESCE(JSON_ARRAYAGG(<document>), JSON_ARRAY())` or
/// `JSON_ARRAYAGG(<document>)`, with whether it falls back on the empty
/// array; `None` when the projection does not aggregate.
fn aggregated_document(
    expr: &Expr,
) -> Result<Option<(&sqlparser::ast::Function, bool)>, ParseError> {
    let Expr::Function(function) = expr else {
        return Ok(None);
    };
    if is_named(function, "COALESCE") {
        let arguments = plain_arguments(function)?;
        let [aggregate, fallback] = arguments.as_slice() else {
            return unsupported("LATERAL body falling back other than on an empty array");
        };
        let Expr::Function(fallback) = fallback else {
            return unsupported("LATERAL body falling back other than on an empty array");
        };
        if !is_named(fallback, "JSON_ARRAY") || !plain_arguments(fallback)?.is_empty() {
            return unsupported("LATERAL body falling back other than on an empty array");
        }
        let Expr::Function(aggregate) = aggregate else {
            return unsupported("LATERAL body falling back from other than JSON_ARRAYAGG");
        };
        return collected_document(aggregate)
            .map(|built| Some((built, true)))
            .ok_or(ParseError::Unsupported {
                feature: "LATERAL body falling back from other than JSON_ARRAYAGG",
            });
    }
    Ok(collected_document(function).map(|built| (built, false)))
}

fn collected_document(function: &sqlparser::ast::Function) -> Option<&sqlparser::ast::Function> {
    if !is_named(function, "JSON_ARRAYAGG") {
        return None;
    }
    match plain_arguments(function).ok()?.as_slice() {
        [Expr::Function(built)] => Some(built),
        _ => None,
    }
}

fn is_named(function: &sqlparser::ast::Function, name: &str) -> bool {
    matches!(
        function.name.0.as_slice(),
        [ObjectNamePart::Identifier(named)] if named.quote_style.is_none() && named.value.eq_ignore_ascii_case(name)
    )
}

/// The arguments of a call written with nothing but its arguments.
fn plain_arguments(function: &sqlparser::ast::Function) -> Result<Vec<&Expr>, ParseError> {
    let FunctionArguments::List(list) = &function.args else {
        return unsupported("LATERAL body call without an argument list");
    };
    if function.uses_odbc_syntax
        || !matches!(function.parameters, FunctionArguments::None)
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || function.over.is_some()
        || !function.within_group.is_empty()
        || list.duplicate_treatment.is_some()
        || !list.clauses.is_empty()
    {
        return unsupported("LATERAL body call option");
    }
    list.args
        .iter()
        .map(|argument| match argument {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
                Ok(expr)
            }
            _ => unsupported("LATERAL body call argument"),
        })
        .collect()
}

/// What a document in a body may be built out of: its own table's columns
/// and the documents of the lateral tables it joins.
struct DocumentReads<'a> {
    reference: &'a str,
    nested: &'a [(String, String, String)],
}

/// `JSON_ARRAY` over columns of the body's own table and the columns of the
/// lateral tables it joins, each written into the document the way MySQL
/// writes it.
fn render_document(
    function: &sqlparser::ast::Function,
    reads: &DocumentReads<'_>,
    render_context: &SelectRenderContext<'_>,
    written: &mut Written,
) -> Result<String, ParseError> {
    if !is_named(function, "JSON_ARRAY") {
        return unsupported("LATERAL body building other than JSON_ARRAY");
    }
    let arguments = plain_arguments(function)?
        .into_iter()
        .map(|argument| {
            let Expr::CompoundIdentifier(parts) = argument else {
                return unsupported("LATERAL body document built from other than columns");
            };
            let [table, column] = parts.as_slice() else {
                return unsupported("LATERAL body document built from other than columns");
            };
            if table.value.eq_ignore_ascii_case(reads.reference) {
                written
                    .columns
                    .push((reads.reference.to_owned(), column.value.clone()));
                let rendered = format!("{}.{}", render_ident(table), render_ident(column));
                return render_column_into_a_document(&rendered, &column.value, render_context);
            }
            match reads
                .nested
                .iter()
                .find(|(alias, _, _)| alias.eq_ignore_ascii_case(&table.value))
            {
                Some((_, projected, sql)) if projected.eq_ignore_ascii_case(&column.value) => {
                    Ok(format!("json({sql})"))
                }
                _ => unsupported("LATERAL body document built from a column of another table"),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "mysql_json_document(json_array({}))",
        arguments.join(", ")
    ))
}
