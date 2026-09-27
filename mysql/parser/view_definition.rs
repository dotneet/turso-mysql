//! A view's `SELECT` written the way MySQL prints it back.
//!
//! MySQL keeps a view as the text it prints in `SHOW CREATE VIEW`: every
//! column qualified by its table and named with an `AS`, keywords in lower
//! case, and each comparison and each run of `AND` or `OR` in parentheses of
//! its own. A view whose `SELECT` carries a condition, groups its rows,
//! aggregates them, joins tables or names a table under an alias is kept here
//! in that same text, so what `SHOW CREATE VIEW` prints is what was stored.

use super::*;
use sqlparser::ast::{JoinConstraint, JoinOperator, Query, TableWithJoins};

/// Names the columns a base table declares, or nothing for any other name.
pub type DeclaredColumns<'a> = dyn Fn(&MySqlTableName) -> Option<Vec<String>> + 'a;

/// Writes a `CREATE VIEW` kept in the text MySQL prints the way MySQL prints
/// it back, or answers nothing for any other `CREATE VIEW`.
///
/// `declared_columns` names the columns a table declares, which is how MySQL
/// writes a column however the statement spelled it: measured on 8.4.11,
/// `SELECT ID FROM posts` is kept as `` `posts`.`id` AS `ID` ``, and a column
/// a join names without its table is kept under the table that has it.
pub fn view_written_as_mysql_prints_it(
    sql: &str,
    mode: SessionSqlMode,
    declared_columns: &DeclaredColumns<'_>,
) -> Result<Option<String>, ParseError> {
    let dump_ddl = parse_optional_mysqldump_ddl(sql)?;
    let sql = dump_ddl.as_ref().map_or(sql, MySqlDumpDdl::normalized_sql);
    let Statement::CreateView(view) = parse_one_statement(sql, mode)? else {
        return Ok(None);
    };
    if !kept_as_mysql_prints_it(&view.query) {
        return Ok(None);
    }
    refuse_view_options(&view)?;
    let [ObjectNamePart::Identifier(name)] = view.name.0.as_slice() else {
        return unsupported("qualified view name");
    };
    let name = MySqlTableName::parse(&name.value)?;
    Ok(Some(format!(
        "CREATE VIEW {} AS {}",
        quoted_name(name.as_str()),
        view_body(
            &view.query,
            mode,
            Some(declared_columns),
            Written::AsMySqlPrintsIt
        )?
    )))
}

/// Reads the name a `CREATE VIEW` makes, before anything else in it is read.
///
/// A view joining tables can name a column without its table, which only
/// the tables' own columns resolve, so whether the rest is taken is found out
/// later, once they are known.
pub fn created_view_name(sql: &str, mode: SessionSqlMode) -> Option<MySqlTableName> {
    let dump_ddl = parse_optional_mysqldump_ddl(sql).ok()?;
    let sql = dump_ddl.as_ref().map_or(sql, MySqlDumpDdl::normalized_sql);
    let Ok(Statement::CreateView(view)) = parse_one_statement(sql, mode) else {
        return None;
    };
    let [ObjectNamePart::Identifier(name)] = view.name.0.as_slice() else {
        return None;
    };
    MySqlTableName::parse(&name.value).ok()
}

/// The MySQL DDL a view is kept under.
///
/// A view kept in the text MySQL prints cannot be rendered back into it from
/// the engine's statement — a `count(0)` or a `<=>` has become something else
/// by then — so the text it was made from is kept, once it is proved to
/// translate to that same statement. Any other view is rendered from its
/// statement.
pub fn mysql_create_view_ddl(
    statement: &Stmt,
    written: &str,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    if !translated_view_is_kept_as_mysql_prints_it(statement) {
        return render_create_view_mysql_with_mode(statement, mode);
    }
    if parse_create_view_ast(written, mode)? != *statement {
        return unsupported("a view kept as MySQL prints it written other than as it was made");
    }
    Ok(written.to_owned())
}

/// Reads the `SELECT` of a view kept in the text MySQL prints, written the
/// way the `SELECT` translator reads it, which is what the view runs.
pub fn translated_view_select(sql: &str, mode: SessionSqlMode) -> Result<String, ParseError> {
    let Statement::CreateView(view) = parse_one_statement(sql, mode)? else {
        return Err(ParseError::ExpectedCreateView);
    };
    view_body(&view.query, mode, None, Written::ForTheTranslator)
}

/// Prints `SHOW CREATE VIEW` for a view kept in the text MySQL prints.
pub fn render_show_create_written_view_mysql(
    sql: &str,
    mode: SessionSqlMode,
    username: &str,
) -> Result<String, ParseError> {
    let Statement::CreateView(view) = parse_one_statement(sql, mode)? else {
        return Err(ParseError::ExpectedCreateView);
    };
    let [ObjectNamePart::Identifier(name)] = view.name.0.as_slice() else {
        return unsupported("qualified view name");
    };
    Ok(format!(
        "CREATE ALGORITHM=UNDEFINED DEFINER=`{}`@`%` SQL SECURITY DEFINER VIEW {} AS {}",
        username.replace('`', "``"),
        quoted_name(&name.value),
        view_body(&view.query, mode, None, Written::AsMySqlPrintsIt)?
    ))
}

/// The definition `information_schema.VIEWS` reads a view kept in the text
/// MySQL prints back as, and whether MySQL can update through it.
///
/// Measured on MySQL 8.4.11: every table is named in full,
/// `` `probe`.`posts` ``, and so is every column of a table read under its own
/// name, `` `probe`.`posts`.`id` ``, where a column of a table read under an
/// alias keeps the alias, `` `p`.`id` ``. A view grouping or aggregating its
/// rows, or reading one of its tables through a `LEFT JOIN`, cannot be
/// updated through; one reading its tables' columns through a condition or an
/// inner join can.
pub fn written_view_definition(
    sql: &str,
    mode: SessionSqlMode,
    database: &str,
) -> Result<(String, bool), ParseError> {
    let Statement::CreateView(view) = parse_one_statement(sql, mode)? else {
        return Err(ParseError::ExpectedCreateView);
    };
    let definition = view_body(
        &view.query,
        mode,
        None,
        Written::AsInformationSchemaPrintsIt { database },
    )?;
    let readings = written_view_columns(sql, mode)?;
    let updatable = !readings.grouped()
        && readings.sources().iter().all(|source| !source.outer)
        && readings
            .columns()
            .iter()
            .all(|(_, reading)| matches!(reading, MySqlViewColumnReading::Column { .. }));
    Ok((definition, updatable))
}

/// Reports whether a `SELECT` reads its one table's rows as they come: its
/// columns as they are, under a condition reading no other table, perhaps
/// cut short by a `LIMIT`.
///
/// Measured on MySQL 8.4.11, that is the `SELECT` a view joining tables is
/// reported the same way for however MySQL plans it. Sorting, grouping,
/// `DISTINCT` or an aggregate can have MySQL gather the rows in a temporary
/// table first, which changes the columns it reports, and whether it does
/// depends on the plan.
pub fn select_reads_rows_as_they_come(sql: &str, mode: SessionSqlMode) -> bool {
    let Ok(Statement::Query(query)) = parse_one_statement(sql, mode) else {
        return false;
    };
    if query.with.is_some()
        || query.order_by.is_some()
        || query.fetch.is_some()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return false;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    let [from] = select.from.as_slice() else {
        return false;
    };
    matches!(select.flavor, SelectFlavor::Standard)
        && select.distinct.is_none()
        && select.top.is_none()
        && select.into.is_none()
        && select.having.is_none()
        && select.named_window.is_empty()
        && select.qualify.is_none()
        && matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, modifiers) if exprs.is_empty() && modifiers.is_empty())
        && from.joins.is_empty()
        && matches!(from.relation, TableFactor::Table { .. })
        && select.projection.iter().all(|item| match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => true,
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
            }
            _ => false,
        })
        && select
            .selection
            .as_ref()
            .is_none_or(condition_reads_no_other_table)
}

/// Reports whether a condition reads only the columns of the table it
/// stands over and written values: no subquery, and nothing this does not
/// look into.
fn condition_reads_no_other_table(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => true,
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner) => condition_reads_no_other_table(inner),
        Expr::BinaryOp { left, right, .. } => {
            condition_reads_no_other_table(left) && condition_reads_no_other_table(right)
        }
        Expr::InList { expr, list, .. } => {
            condition_reads_no_other_table(expr) && list.iter().all(condition_reads_no_other_table)
        }
        Expr::Between {
            expr, low, high, ..
        } => [expr, low, high]
            .into_iter()
            .all(|part| condition_reads_no_other_table(part)),
        Expr::Like {
            expr,
            pattern,
            escape_char: None,
            ..
        } => condition_reads_no_other_table(expr) && condition_reads_no_other_table(pattern),
        _ => false,
    }
}

/// What each column of a view kept in the text MySQL prints reads, the
/// tables it reads them from, and whether it groups its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlWrittenView {
    sources: Vec<MySqlViewSource>,
    renames_a_table: bool,
    grouped: bool,
    columns: Vec<(String, MySqlViewColumnReading)>,
}

impl MySqlWrittenView {
    /// Returns the first table the view reads, which is the only one a view
    /// grouping its rows reads.
    pub fn table(&self) -> &MySqlTableName {
        &self.sources[0].table
    }

    /// Returns every table the view reads, in the order its `FROM` names them.
    pub fn sources(&self) -> &[MySqlViewSource] {
        &self.sources
    }

    /// Reports whether the view reads one table under its own name, which
    /// is the view whose columns the engine's own statement already names.
    pub fn reads_one_table_by_its_name(&self) -> bool {
        self.sources.len() == 1 && !self.renames_a_table
    }

    /// Reports whether the view groups its rows, by a `GROUP BY` or by an
    /// aggregate over them all. MySQL reads such a view out of a table of its
    /// own, which is what changes the shape it reports for each column.
    pub const fn grouped(&self) -> bool {
        self.grouped
    }

    /// Returns each column's name and what it reads, in order.
    pub fn columns(&self) -> &[(String, MySqlViewColumnReading)] {
        &self.columns
    }
}

/// One table a view reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlViewSource {
    table: MySqlTableName,
    outer: bool,
}

impl MySqlViewSource {
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Reports whether a `LEFT JOIN` can leave this table's row missing, which
    /// is what takes `NOT NULL` off its columns.
    pub const fn outer(&self) -> bool {
        self.outer
    }
}

/// What one column of a view reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlViewColumnReading {
    /// A column of one of the view's tables, counted among
    /// [`MySqlWrittenView::sources`] and named as that table declares it.
    Column {
        source: usize,
        column: String,
    },
    /// `COUNT(*)`, `COUNT(col)` or `COUNT(DISTINCT col)`.
    Count,
    Sum(String),
    Average(String),
    Least(String),
    Greatest(String),
}

/// Reads what each column of a view kept in the text MySQL prints reads.
pub fn written_view_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlWrittenView, ParseError> {
    let Statement::CreateView(view) = parse_one_statement(sql, mode)? else {
        return Err(ParseError::ExpectedCreateView);
    };
    let (select, from) = view_select(&view.query)?;
    let body = ViewBody {
        from: &from,
        declared: None,
        mode,
        written: Written::AsMySqlPrintsIt,
    };
    let mut grouped = !group_by_columns(select)?.is_empty();
    let mut columns = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        let (expr, alias) = projected(item)?;
        let reading = match body.written_column(expr) {
            Some((source, column)) => MySqlViewColumnReading::Column {
                source,
                column: column.value.clone(),
            },
            None => {
                grouped = true;
                let (function, argument) = aggregate(expr)?;
                let argument = match argument {
                    Some(argument) => Some(
                        body.written_column(argument)
                            .ok_or(ParseError::Unsupported {
                                feature: "CREATE VIEW aggregate over something other than a column",
                            })?
                            .1
                            .value
                            .clone(),
                    ),
                    None => None,
                };
                match (function, argument) {
                    (Aggregate::Count { .. }, _) => MySqlViewColumnReading::Count,
                    (Aggregate::Sum, Some(column)) => MySqlViewColumnReading::Sum(column),
                    (Aggregate::Average, Some(column)) => MySqlViewColumnReading::Average(column),
                    (Aggregate::Least, Some(column)) => MySqlViewColumnReading::Least(column),
                    (Aggregate::Greatest, Some(column)) => MySqlViewColumnReading::Greatest(column),
                    _ => return unsupported("CREATE VIEW aggregate"),
                }
            }
        };
        let name = match (alias, &reading) {
            (Some(alias), _) => alias.value.clone(),
            (None, MySqlViewColumnReading::Column { column, .. }) => column.clone(),
            (None, _) => return unsupported("CREATE VIEW aggregate without a name"),
        };
        columns.push((name, reading));
    }
    if grouped && !from.is_one_table_by_its_name() {
        return unsupported("CREATE VIEW grouping the rows of a join or of a renamed table");
    }
    Ok(MySqlWrittenView {
        sources: from
            .sources
            .iter()
            .map(|source| MySqlViewSource {
                table: source.table.clone(),
                outer: source.outer,
            })
            .collect(),
        renames_a_table: from.sources.iter().any(|source| source.alias.is_some()),
        grouped,
        columns,
    })
}

/// Reports whether a view is kept in the text MySQL prints: one whose
/// `SELECT` carries a condition, groups its rows, aggregates them, joins
/// tables or reads a table under an alias.
pub(crate) fn kept_as_mysql_prints_it(query: &Query) -> bool {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    select.selection.is_some()
        || !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
        || select.projection.iter().any(|item| {
            matches!(
                item,
                SelectItem::UnnamedExpr(Expr::Function(_))
                    | SelectItem::ExprWithAlias {
                        expr: Expr::Function(_),
                        ..
                    }
            )
        })
        || select.from.len() > 1
        || select.from.iter().any(|from| {
            !from.joins.is_empty()
                || !matches!(from.relation, TableFactor::Table { alias: None, .. })
        })
}

/// Reports whether a translated view is kept in the text MySQL prints.
pub fn translated_view_is_kept_as_mysql_prints_it(statement: &Stmt) -> bool {
    let Stmt::CreateView { select, .. } = statement else {
        return false;
    };
    let OneSelect::Select {
        columns,
        where_clause,
        group_by,
        from,
        ..
    } = &select.body.select
    else {
        return false;
    };
    where_clause.is_some()
        || group_by.is_some()
        || columns.iter().any(|column| {
            matches!(
                column,
                ResultColumn::Expr(expr, _)
                    if matches!(
                        expr.as_ref(),
                        TursoExpr::FunctionCall { .. } | TursoExpr::FunctionCallStar { .. }
                    )
            )
        })
        || from.as_ref().is_some_and(|from| {
            !from.joins.is_empty()
                || matches!(from.select.as_ref(), SelectTable::Table(_, Some(_), _))
        })
}

/// Which of the texts a view's `SELECT` is written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written<'a> {
    /// As MySQL prints it back: `` `t`.`c` ``, `count(0)`, `group by`.
    AsMySqlPrintsIt,
    /// As `information_schema.VIEWS` prints it back, every table named with
    /// its database: `` `db`.`t`.`c` ``.
    AsInformationSchemaPrintsIt { database: &'a str },
    /// As the `SELECT` translator reads it: `COUNT(*)`, and each column bare
    /// when the view reads one table under its own name.
    ForTheTranslator,
}

/// Writes a view's `SELECT`, refusing every shape whose printing has not been
/// measured.
///
/// Without `declared_columns` each column keeps the spelling it was written
/// with, which is what reading text already written this way needs.
pub(crate) fn view_body(
    query: &Query,
    mode: SessionSqlMode,
    declared_columns: Option<&DeclaredColumns<'_>>,
    written: Written<'_>,
) -> Result<String, ParseError> {
    let (select, from) = view_select(query)?;
    let declared = match declared_columns {
        Some(declared_columns) => Some(
            from.sources
                .iter()
                .map(|source| {
                    declared_columns(&source.table).ok_or(ParseError::Unsupported {
                        feature: "CREATE VIEW over something other than a base table",
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        None => None,
    };
    let body = ViewBody {
        from: &from,
        declared: declared.as_deref(),
        mode,
        written,
    };
    let columns = select
        .projection
        .iter()
        .map(|item| body.projected_column(item))
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE VIEW without projections");
    }
    let names = select
        .projection
        .iter()
        .map(|item| body.column_name(item))
        .collect::<Result<Vec<_>, _>>()?;
    // MySQL answers 1060 for a view naming two columns alike.
    for (at, name) in names.iter().enumerate() {
        if names[..at]
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(name))
        {
            return unsupported("CREATE VIEW naming two columns alike");
        }
    }
    let mut text = format!("select {} from {}", columns.join(","), body.written_from()?);
    if let Some(condition) = &select.selection {
        text.push_str(" where ");
        text.push_str(&body.condition(condition)?);
    }
    let grouping = group_by_columns(select)?;
    if !grouping.is_empty() {
        if !from.is_one_table_by_its_name() {
            return unsupported("CREATE VIEW grouping the rows of a join or of a renamed table");
        }
        let grouping = grouping
            .iter()
            .map(|expr| match body.written_column(expr) {
                Some((source, column)) => body.column(source, column),
                None => unsupported("CREATE VIEW grouping by something other than a column"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        text.push_str(" group by ");
        text.push_str(&grouping.join(","));
    }
    Ok(text)
}

/// Reads a view's `SELECT` and the tables its `FROM` reads, refusing every
/// clause whose printing has not been measured.
fn view_select(query: &Query) -> Result<(&sqlparser::ast::Select, ViewFrom<'_>), ParseError> {
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
        || !select.connect_by.is_empty()
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
    // Measured on MySQL 8.4.11, a comma between two tables is printed
    // `` (`a` join `b`) `` by `SHOW CREATE VIEW` and `` `a` join `b` `` by
    // `information_schema.VIEWS`, which has not been followed further.
    let [from] = select.from.as_slice() else {
        return unsupported("CREATE VIEW FROM clause");
    };
    let mut view_from = ViewFrom {
        sources: Vec::new(),
        joins: Vec::new(),
    };
    view_from.gather(from)?;
    for (at, source) in view_from.sources.iter().enumerate() {
        // MySQL answers 1066 for two tables read under one name.
        if view_from.sources[..at]
            .iter()
            .any(|earlier| earlier.reference().eq_ignore_ascii_case(source.reference()))
        {
            return unsupported("CREATE VIEW reading two tables under one name");
        }
    }
    Ok((select, view_from))
}

/// The tables a view reads, in the order its `FROM` names them, and what
/// each table after the first is joined on.
struct ViewFrom<'q> {
    sources: Vec<ViewSource>,
    /// The join bringing in `sources[at + 1]` is `joins[at]`.
    joins: Vec<ViewJoin<'q>>,
}

struct ViewSource {
    table: MySqlTableName,
    alias: Option<String>,
    outer: bool,
}

impl ViewSource {
    /// The name the view's columns are qualified by.
    fn reference(&self) -> &str {
        self.alias.as_deref().unwrap_or(self.table.as_str())
    }
}

struct ViewJoin<'q> {
    left: bool,
    on: &'q Expr,
}

impl<'q> ViewFrom<'q> {
    /// Reads the tables of one `FROM` entry, left to right. MySQL prints a
    /// join of three tables as the join of the first two joined to the
    /// third, `` ((`a` join `b` on(...)) join `c` on(...)) ``, which reads
    /// back as a join nested on the left.
    fn gather(&mut self, from: &'q TableWithJoins) -> Result<(), ParseError> {
        match &from.relation {
            TableFactor::NestedJoin {
                table_with_joins,
                alias: None,
            } if !table_with_joins.joins.is_empty() => self.gather(table_with_joins)?,
            relation => self.sources.push(view_source(relation, false)?),
        }
        for join in &from.joins {
            if join.global {
                return unsupported("CREATE VIEW JOIN form");
            }
            // A `RIGHT JOIN` is printed turned around into a `LEFT JOIN`, and
            // `USING` as the `ON` it stands for, both measured on 8.4.11 and
            // neither followed here.
            let (left, on) = match &join.join_operator {
                JoinOperator::Join(JoinConstraint::On(on))
                | JoinOperator::Inner(JoinConstraint::On(on)) => (false, on),
                JoinOperator::Left(JoinConstraint::On(on))
                | JoinOperator::LeftOuter(JoinConstraint::On(on)) => (true, on),
                _ => return unsupported("CREATE VIEW JOIN form"),
            };
            self.sources.push(view_source(&join.relation, left)?);
            self.joins.push(ViewJoin { left, on });
        }
        Ok(())
    }

    fn is_one_table_by_its_name(&self) -> bool {
        matches!(self.sources.as_slice(), [source] if source.alias.is_none())
    }
}

fn view_source(relation: &TableFactor, outer: bool) -> Result<ViewSource, ParseError> {
    let TableFactor::Table {
        name,
        alias,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_hints,
    } = relation
    else {
        return unsupported("CREATE VIEW table source");
    };
    if !with_hints.is_empty() || !partitions.is_empty() || !index_hints.is_empty() {
        return unsupported("CREATE VIEW table option");
    }
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("CREATE VIEW over a qualified table");
    };
    let alias = match alias {
        None => None,
        Some(alias) if alias.columns.is_empty() && alias.at.is_none() => {
            Some(alias.name.value.clone())
        }
        Some(_) => return unsupported("CREATE VIEW table alias form"),
    };
    Ok(ViewSource {
        table: MySqlTableName::parse(&table.value)?,
        alias,
        outer,
    })
}

fn group_by_columns(select: &sqlparser::ast::Select) -> Result<&[Expr], ParseError> {
    match &select.group_by {
        sqlparser::ast::GroupByExpr::Expressions(exprs, modifiers) if modifiers.is_empty() => {
            Ok(exprs)
        }
        _ => unsupported("CREATE VIEW GROUP BY form"),
    }
}

fn projected(item: &SelectItem) -> Result<(&Expr, Option<&Ident>), ParseError> {
    match item {
        SelectItem::UnnamedExpr(expr) => Ok((expr, None)),
        SelectItem::ExprWithAlias { expr, alias } => Ok((expr, Some(alias))),
        _ => unsupported("CREATE VIEW projection"),
    }
}

/// The aggregates a view takes, each measured on MySQL 8.4.11 for how it is
/// printed back and for the column it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aggregate {
    Count { distinct: bool },
    Sum,
    Average,
    Least,
    Greatest,
}

/// Reads one aggregate and the column it reads, which a `COUNT(*)` has none
/// of.
fn aggregate(expr: &Expr) -> Result<(Aggregate, Option<&Expr>), ParseError> {
    let Expr::Function(function) = expr else {
        return unsupported("CREATE VIEW projection");
    };
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return unsupported("CREATE VIEW call");
    };
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return unsupported("CREATE VIEW call");
    };
    if name.quote_style.is_some()
        || function.over.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || !matches!(function.parameters, sqlparser::ast::FunctionArguments::None)
        || !arguments.clauses.is_empty()
    {
        return unsupported("CREATE VIEW call");
    }
    let distinct = match arguments.duplicate_treatment {
        None => false,
        Some(sqlparser::ast::DuplicateTreatment::Distinct) => true,
        Some(sqlparser::ast::DuplicateTreatment::All) => {
            return unsupported("CREATE VIEW aggregate over ALL")
        }
    };
    let named = |candidate: &str| name.value.eq_ignore_ascii_case(candidate);
    let function = if named("COUNT") {
        Aggregate::Count { distinct }
    } else if distinct {
        return unsupported("CREATE VIEW aggregate over DISTINCT");
    } else if named("SUM") {
        Aggregate::Sum
    } else if named("AVG") {
        Aggregate::Average
    } else if named("MIN") {
        Aggregate::Least
    } else if named("MAX") {
        Aggregate::Greatest
    } else {
        return unsupported("CREATE VIEW call");
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)]
            if function == (Aggregate::Count { distinct: false }) =>
        {
            Ok((function, None))
        }
        // `count(0)` is how MySQL prints `COUNT(*)` back, and counts every row
        // as it does.
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(value),
        ))] if function == (Aggregate::Count { distinct: false })
            && matches!(&value.value, Value::Number(number, false) if number == "0") =>
        {
            Ok((function, None))
        }
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(argument))] => {
            Ok((function, Some(argument)))
        }
        _ => unsupported("CREATE VIEW aggregate argument"),
    }
}

/// What writing one view's `SELECT` needs to know.
struct ViewBody<'a> {
    from: &'a ViewFrom<'a>,
    /// The columns each table declares, in the order of `from.sources`.
    declared: Option<&'a [Vec<String>]>,
    mode: SessionSqlMode,
    written: Written<'a>,
}

impl ViewBody<'_> {
    /// Writes one result column, `` `t`.`c` AS `name` ``, named after its
    /// alias or, without one, after the column as it was written. An
    /// aggregate has to be named: MySQL names one after the text it was
    /// written as, spacing and all.
    fn projected_column(&self, item: &SelectItem) -> Result<String, ParseError> {
        let (expr, alias) = projected(item)?;
        if let Some((source, column)) = self.written_column(expr) {
            let name = alias.unwrap_or(column);
            return Ok(format!(
                "{} AS {}",
                self.column(source, column)?,
                quoted_name(&name.value)
            ));
        }
        if !self.from.is_one_table_by_its_name() {
            return unsupported("CREATE VIEW aggregating the rows of a join or of a renamed table");
        }
        let Some(alias) = alias else {
            return unsupported("CREATE VIEW aggregate without a name");
        };
        Ok(format!(
            "{} AS {}",
            self.aggregate(expr)?,
            quoted_name(&alias.value)
        ))
    }

    /// The name one result column answers under.
    fn column_name(&self, item: &SelectItem) -> Result<String, ParseError> {
        let (expr, alias) = projected(item)?;
        match (alias, self.written_column(expr)) {
            (Some(alias), _) => Ok(alias.value.clone()),
            (None, Some((_, column))) => Ok(column.value.clone()),
            (None, None) => unsupported("CREATE VIEW aggregate without a name"),
        }
    }

    /// Writes an aggregate: measured on MySQL 8.4.11, `COUNT(*)` is printed
    /// `count(0)` and the rest by their names in lower case over their
    /// qualified column, `count(distinct ...)` among them.
    fn aggregate(&self, expr: &Expr) -> Result<String, ParseError> {
        let (function, argument) = aggregate(expr)?;
        let argument = match argument {
            Some(argument) => match self.written_column(argument) {
                Some((source, column)) => Some(self.column(source, column)?),
                None => {
                    return unsupported("CREATE VIEW aggregate over something other than a column")
                }
            },
            None => None,
        };
        let prints = self.written != Written::ForTheTranslator;
        Ok(match (function, argument) {
            (Aggregate::Count { .. }, None) if prints => "count(0)".to_owned(),
            (Aggregate::Count { .. }, None) => "COUNT(*)".to_owned(),
            (Aggregate::Count { distinct: true }, Some(column)) if prints => {
                format!("count(distinct {column})")
            }
            (Aggregate::Count { distinct: true }, Some(column)) => {
                format!("COUNT(DISTINCT {column})")
            }
            (function, Some(column)) => {
                let name = match function {
                    Aggregate::Count { .. } => "count",
                    Aggregate::Sum => "sum",
                    Aggregate::Average => "avg",
                    Aggregate::Least => "min",
                    Aggregate::Greatest => "max",
                };
                if prints {
                    format!("{name}({column})")
                } else {
                    format!("{}({column})", name.to_ascii_uppercase())
                }
            }
            (_, None) => unreachable!("only a count reads no column"),
        })
    }

    /// Reads the column an expression names and which of the view's tables
    /// has it.
    ///
    /// A qualifier has to be the name the table is read under — its alias
    /// when it has one, as in MySQL, which answers 1054 for the table's own
    /// name then. A bare column in a view of one table is that table's; in a
    /// join it is the one table's that declares it, and without the declared
    /// columns to tell, as when reading text MySQL printed, where every column
    /// is qualified, it names none.
    fn written_column<'e>(&self, expr: &'e Expr) -> Option<(usize, &'e Ident)> {
        match expr {
            Expr::Identifier(column) if self.from.sources.len() == 1 => Some((0, column)),
            Expr::Identifier(column) => {
                let declared = self.declared?;
                let mut having = declared.iter().enumerate().filter(|(_, columns)| {
                    columns
                        .iter()
                        .any(|declared| declared.eq_ignore_ascii_case(&column.value))
                });
                let (source, _) = having.next()?;
                // MySQL answers 1052 for a column two of the tables have.
                if having.next().is_some() {
                    return None;
                }
                Some((source, column))
            }
            Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [qualifier, column] => self
                    .from
                    .sources
                    .iter()
                    .position(|source| source.reference().eq_ignore_ascii_case(&qualifier.value))
                    .map(|source| (source, column)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Writes a column: qualified by the name its table is read under, as
    /// MySQL prints it, or bare, as the translator reads the one table a view
    /// reads under its own name.
    fn column(&self, source: usize, column: &Ident) -> Result<String, ParseError> {
        let declared = match self.declared {
            Some(declared) => declared[source]
                .iter()
                .find(|declared| declared.eq_ignore_ascii_case(&column.value))
                .ok_or(ParseError::Unsupported {
                    feature: "CREATE VIEW naming a column its table does not have",
                })?
                .as_str(),
            None => column.value.as_str(),
        };
        let read = &self.from.sources[source];
        Ok(match self.written {
            Written::AsMySqlPrintsIt => {
                format!(
                    "{}.{}",
                    quoted_name(read.reference()),
                    quoted_name(declared)
                )
            }
            Written::AsInformationSchemaPrintsIt { database } if read.alias.is_none() => format!(
                "{}.{}.{}",
                quoted_name(database),
                quoted_name(read.table.as_str()),
                quoted_name(declared)
            ),
            Written::AsInformationSchemaPrintsIt { .. } => {
                format!(
                    "{}.{}",
                    quoted_name(read.reference()),
                    quoted_name(declared)
                )
            }
            Written::ForTheTranslator if self.from.is_one_table_by_its_name() => {
                quoted_name(declared)
            }
            Written::ForTheTranslator => {
                format!(
                    "{}.{}",
                    quoted_name(read.reference()),
                    quoted_name(declared)
                )
            }
        })
    }

    /// Writes the `FROM`: measured on MySQL 8.4.11, a table read under an
    /// alias is `` `posts` `p` ``, and each join stands in parentheses of its
    /// own with its condition in `on(...)` —
    /// `` ((`a` join `b` on((...))) left join `c` on((...))) ``. The
    /// translator reads the same joins left to right without them.
    fn written_from(&self) -> Result<String, ParseError> {
        let mut text = self.table(0);
        for (at, join) in self.from.joins.iter().enumerate() {
            let table = self.table(at + 1);
            let on = self.condition(join.on)?;
            text = match (self.written, join.left) {
                (Written::ForTheTranslator, false) => format!("{text} JOIN {table} ON {on}"),
                (Written::ForTheTranslator, true) => format!("{text} LEFT JOIN {table} ON {on}"),
                (_, false) => format!("({text} join {table} on({on}))"),
                (_, true) => format!("({text} left join {table} on({on}))"),
            };
        }
        Ok(text)
    }

    fn table(&self, source: usize) -> String {
        let read = &self.from.sources[source];
        let table = match self.written {
            Written::AsInformationSchemaPrintsIt { database } => {
                format!(
                    "{}.{}",
                    quoted_name(database),
                    quoted_name(read.table.as_str())
                )
            }
            _ => quoted_name(read.table.as_str()),
        };
        match (&read.alias, self.written) {
            (None, _) => table,
            (Some(alias), Written::ForTheTranslator) => {
                format!("{table} AS {}", quoted_name(alias))
            }
            (Some(alias), _) => format!("{table} {}", quoted_name(alias)),
        }
    }

    /// Writes a condition: measured on MySQL 8.4.11, each comparison stands
    /// in parentheses of its own, a run of `AND` or of `OR` is gathered into
    /// one list — `a AND (b AND c)` is written `((a) and (b) and (c))` — and
    /// the list stands in parentheses too.
    fn condition(&self, expr: &Expr) -> Result<String, ParseError> {
        match expr {
            Expr::Nested(inner) => self.condition(inner),
            Expr::BinaryOp {
                op: op @ (BinaryOperator::And | BinaryOperator::Or),
                ..
            } => {
                let mut operands = Vec::new();
                gather(expr, op, &mut operands);
                let word = if matches!(op, BinaryOperator::And) {
                    " and "
                } else {
                    " or "
                };
                let written = operands
                    .into_iter()
                    .map(|operand| self.condition(operand))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("({})", written.join(word)))
            }
            Expr::BinaryOp { left, op, right } => {
                let operator = match op {
                    BinaryOperator::Eq => "=",
                    BinaryOperator::NotEq => "<>",
                    BinaryOperator::Lt => "<",
                    BinaryOperator::LtEq => "<=",
                    BinaryOperator::Gt => ">",
                    BinaryOperator::GtEq => ">=",
                    BinaryOperator::Spaceship => "<=>",
                    _ => return unsupported("CREATE VIEW condition operator"),
                };
                Ok(format!(
                    "({} {operator} {})",
                    self.operand(left)?,
                    self.operand(right)?
                ))
            }
            Expr::IsNull(inner) => Ok(format!("({} is null)", self.operand(inner)?)),
            Expr::IsNotNull(inner) => Ok(format!("({} is not null)", self.operand(inner)?)),
            _ => unsupported("CREATE VIEW condition"),
        }
    }

    fn operand(&self, expr: &Expr) -> Result<String, ParseError> {
        if let Some((source, column)) = self.written_column(expr) {
            return self.column(source, column);
        }
        let Expr::Value(value) = expr else {
            return unsupported("CREATE VIEW condition operand");
        };
        match &value.value {
            // A number is written back as the digits it is, so only a whole
            // one written without leading zeros is taken.
            Value::Number(number, false)
                if number
                    .parse::<u64>()
                    .is_ok_and(|parsed| parsed.to_string() == *number) =>
            {
                Ok(number.clone())
            }
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                self.written_word(text)
            }
            Value::Boolean(true) => Ok("true".to_owned()),
            Value::Boolean(false) => Ok("false".to_owned()),
            Value::Null => Ok("NULL".to_owned()),
            _ => unsupported("CREATE VIEW condition value"),
        }
    }

    /// Writes a word in single quotes: measured on MySQL 8.4.11, a quote is
    /// written `\'` and a backslash `\\`. A control character is refused,
    /// MySQL writing some of them escaped and some as they are, and so is a
    /// quote or a backslash under `NO_BACKSLASH_ESCAPES`, where the escaped
    /// form would not read back.
    fn written_word(&self, text: &str) -> Result<String, ParseError> {
        if text.chars().any(char::is_control) {
            return unsupported("CREATE VIEW word holding a control character");
        }
        if self.mode.no_backslash_escapes && text.contains(['\'', '\\']) {
            return unsupported("CREATE VIEW word holding a quote under NO_BACKSLASH_ESCAPES");
        }
        Ok(format!(
            "'{}'",
            text.replace('\\', "\\\\").replace('\'', "\\'")
        ))
    }
}

/// Collects the operands of a run of one of `AND` and `OR`, through the
/// parentheses written around its parts.
fn gather<'e>(expr: &'e Expr, op: &BinaryOperator, operands: &mut Vec<&'e Expr>) {
    match expr {
        Expr::Nested(inner) if is_the_operator(inner, op) => gather(inner, op, operands),
        Expr::BinaryOp {
            left,
            op: this,
            right,
        } if this == op => {
            gather(left, op, operands);
            gather(right, op, operands);
        }
        _ => operands.push(expr),
    }
}

fn is_the_operator(expr: &Expr, op: &BinaryOperator) -> bool {
    match expr {
        Expr::Nested(inner) => is_the_operator(inner, op),
        Expr::BinaryOp { op: this, .. } => this == op,
        _ => false,
    }
}

fn quoted_name(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(table: &MySqlTableName) -> Option<Vec<String>> {
        match table.as_str() {
            "posts" => Some(["id", "user_id", "n", "title"].map(str::to_owned).to_vec()),
            "keyed" => Some(["id", "code", "grp", "amount"].map(str::to_owned).to_vec()),
            "users" => Some(["id", "name", "email", "age"].map(str::to_owned).to_vec()),
            "comments" => Some(["id", "post_id", "note"].map(str::to_owned).to_vec()),
            _ => None,
        }
    }

    /// Each expectation is what MySQL 8.4.11 printed for the same statement.
    #[test]
    fn a_view_grouping_its_rows_is_written_the_way_mysql_prints_it() {
        for (sql, expected, translated) in [
            (
                "CREATE VIEW v2 AS SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id",
                "CREATE VIEW `v2` AS select `posts`.`user_id` AS `user_id`,count(0) AS `c` from `posts` group by `posts`.`user_id`",
                "select `user_id` AS `user_id`,COUNT(*) AS `c` from `posts` group by `user_id`",
            ),
            (
                "CREATE VIEW g1 AS SELECT id, code, grp, COUNT(*) AS c FROM keyed GROUP BY id, code, grp",
                "CREATE VIEW `g1` AS select `keyed`.`id` AS `id`,`keyed`.`code` AS `code`,`keyed`.`grp` AS `grp`,count(0) AS `c` from `keyed` group by `keyed`.`id`,`keyed`.`code`,`keyed`.`grp`",
                "select `id` AS `id`,`code` AS `code`,`grp` AS `grp`,COUNT(*) AS `c` from `keyed` group by `id`,`code`,`grp`",
            ),
            (
                "CREATE VIEW g2 AS SELECT grp, SUM(amount) AS s, AVG(amount) AS a, MIN(code) AS lo, MAX(id) AS hi, COUNT(amount) AS n FROM keyed WHERE grp > 0 GROUP BY grp",
                "CREATE VIEW `g2` AS select `keyed`.`grp` AS `grp`,sum(`keyed`.`amount`) AS `s`,avg(`keyed`.`amount`) AS `a`,min(`keyed`.`code`) AS `lo`,max(`keyed`.`id`) AS `hi`,count(`keyed`.`amount`) AS `n` from `keyed` where (`keyed`.`grp` > 0) group by `keyed`.`grp`",
                "select `grp` AS `grp`,SUM(`amount`) AS `s`,AVG(`amount`) AS `a`,MIN(`code`) AS `lo`,MAX(`id`) AS `hi`,COUNT(`amount`) AS `n` from `keyed` where (`grp` > 0) group by `grp`",
            ),
            (
                "CREATE VIEW g3 AS SELECT COUNT(*) AS c, MAX(amount) AS m FROM keyed",
                "CREATE VIEW `g3` AS select count(0) AS `c`,max(`keyed`.`amount`) AS `m` from `keyed`",
                "select COUNT(*) AS `c`,MAX(`amount`) AS `m` from `keyed`",
            ),
            (
                "CREATE VIEW w2 AS SELECT user_id, COUNT(DISTINCT n) AS cd FROM posts GROUP BY user_id",
                "CREATE VIEW `w2` AS select `posts`.`user_id` AS `user_id`,count(distinct `posts`.`n`) AS `cd` from `posts` group by `posts`.`user_id`",
                "select `user_id` AS `user_id`,COUNT(DISTINCT `n`) AS `cd` from `posts` group by `user_id`",
            ),
        ] {
            let written = written(sql).unwrap_or_else(|| panic!("{sql}"));
            assert_eq!(written, expected, "{sql}");
            assert_eq!(self::written(&written).as_deref(), Some(expected));
            assert_eq!(
                translated_view_select(&written, SessionSqlMode::default()).unwrap(),
                translated
            );
        }
        let columns = written_view_columns(
            "CREATE VIEW `g2` AS select `keyed`.`grp` AS `grp`,sum(`keyed`.`amount`) AS `s`,count(0) AS `c` from `keyed` group by `keyed`.`grp`",
            SessionSqlMode::default(),
        )
        .unwrap();
        assert!(columns.grouped());
        assert_eq!(columns.table().as_str(), "keyed");
        assert_eq!(
            columns.columns(),
            [
                (
                    "grp".to_owned(),
                    MySqlViewColumnReading::Column {
                        source: 0,
                        column: "grp".to_owned()
                    }
                ),
                (
                    "s".to_owned(),
                    MySqlViewColumnReading::Sum("amount".to_owned())
                ),
                ("c".to_owned(), MySqlViewColumnReading::Count),
            ]
        );
        for sql in [
            // MySQL names an unnamed aggregate after the text it was written
            // as, spacing and all.
            "CREATE VIEW v AS SELECT user_id, COUNT(*) FROM posts GROUP BY user_id",
            "CREATE VIEW v AS SELECT user_id, SUM(DISTINCT n) AS s FROM posts GROUP BY user_id",
            "CREATE VIEW v AS SELECT user_id, SUM(n + 1) AS s FROM posts GROUP BY user_id",
            "CREATE VIEW v AS SELECT user_id, GROUP_CONCAT(n) AS s FROM posts GROUP BY user_id",
            "CREATE VIEW v AS SELECT user_id FROM posts GROUP BY user_id HAVING COUNT(*) > 1",
            "CREATE VIEW v AS SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id WITH ROLLUP",
        ] {
            assert!(
                view_written_as_mysql_prints_it(sql, SessionSqlMode::default(), &declared).is_err(),
                "{sql}"
            );
        }
    }

    fn written(sql: &str) -> Option<String> {
        view_written_as_mysql_prints_it(sql, SessionSqlMode::default(), &declared).unwrap()
    }

    /// Each expectation is what MySQL 8.4.11 printed for the same statement,
    /// through `SHOW CREATE VIEW` and through `information_schema.VIEWS`.
    #[test]
    fn a_view_joining_tables_is_written_the_way_mysql_prints_it() {
        for (sql, expected, translated, definition, updatable) in [
            (
                "CREATE VIEW v1 AS SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id",
                "CREATE VIEW `v1` AS select `u`.`name` AS `name`,`p`.`title` AS `title` from (`users` `u` join `posts` `p` on((`p`.`user_id` = `u`.`id`)))",
                "select `u`.`name` AS `name`,`p`.`title` AS `title` from `users` AS `u` JOIN `posts` AS `p` ON (`p`.`user_id` = `u`.`id`)",
                "select `u`.`name` AS `name`,`p`.`title` AS `title` from (`probe`.`users` `u` join `probe`.`posts` `p` on((`p`.`user_id` = `u`.`id`)))",
                true,
            ),
            (
                "CREATE VIEW v2 AS SELECT u.id, u.name AS author, p.title FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE u.id > 1",
                "CREATE VIEW `v2` AS select `u`.`id` AS `id`,`u`.`name` AS `author`,`p`.`title` AS `title` from (`users` `u` left join `posts` `p` on((`p`.`user_id` = `u`.`id`))) where (`u`.`id` > 1)",
                "select `u`.`id` AS `id`,`u`.`name` AS `author`,`p`.`title` AS `title` from `users` AS `u` LEFT JOIN `posts` AS `p` ON (`p`.`user_id` = `u`.`id`) where (`u`.`id` > 1)",
                "select `u`.`id` AS `id`,`u`.`name` AS `author`,`p`.`title` AS `title` from (`probe`.`users` `u` left join `probe`.`posts` `p` on((`p`.`user_id` = `u`.`id`))) where (`u`.`id` > 1)",
                false,
            ),
            (
                "CREATE VIEW v3 AS SELECT u.name, p.title, c.note FROM users u JOIN posts p ON p.user_id = u.id LEFT JOIN comments c ON c.post_id = p.id",
                "CREATE VIEW `v3` AS select `u`.`name` AS `name`,`p`.`title` AS `title`,`c`.`note` AS `note` from ((`users` `u` join `posts` `p` on((`p`.`user_id` = `u`.`id`))) left join `comments` `c` on((`c`.`post_id` = `p`.`id`)))",
                "select `u`.`name` AS `name`,`p`.`title` AS `title`,`c`.`note` AS `note` from `users` AS `u` JOIN `posts` AS `p` ON (`p`.`user_id` = `u`.`id`) LEFT JOIN `comments` AS `c` ON (`c`.`post_id` = `p`.`id`)",
                "select `u`.`name` AS `name`,`p`.`title` AS `title`,`c`.`note` AS `note` from ((`probe`.`users` `u` join `probe`.`posts` `p` on((`p`.`user_id` = `u`.`id`))) left join `probe`.`comments` `c` on((`c`.`post_id` = `p`.`id`)))",
                false,
            ),
            (
                "CREATE VIEW v4 AS SELECT users.name, posts.title FROM users INNER JOIN posts ON posts.user_id = users.id",
                "CREATE VIEW `v4` AS select `users`.`name` AS `name`,`posts`.`title` AS `title` from (`users` join `posts` on((`posts`.`user_id` = `users`.`id`)))",
                "select `users`.`name` AS `name`,`posts`.`title` AS `title` from `users` JOIN `posts` ON (`posts`.`user_id` = `users`.`id`)",
                "select `probe`.`users`.`name` AS `name`,`probe`.`posts`.`title` AS `title` from (`probe`.`users` join `probe`.`posts` on((`probe`.`posts`.`user_id` = `probe`.`users`.`id`)))",
                true,
            ),
            (
                "CREATE VIEW v5 AS SELECT name, title FROM users JOIN posts ON user_id = users.id",
                "CREATE VIEW `v5` AS select `users`.`name` AS `name`,`posts`.`title` AS `title` from (`users` join `posts` on((`posts`.`user_id` = `users`.`id`)))",
                "select `users`.`name` AS `name`,`posts`.`title` AS `title` from `users` JOIN `posts` ON (`posts`.`user_id` = `users`.`id`)",
                "select `probe`.`users`.`name` AS `name`,`probe`.`posts`.`title` AS `title` from (`probe`.`users` join `probe`.`posts` on((`probe`.`posts`.`user_id` = `probe`.`users`.`id`)))",
                true,
            ),
            (
                "CREATE VIEW v6 AS SELECT u.id AS uid, p.id AS pid, p.n FROM users AS u LEFT OUTER JOIN posts AS p ON u.id = p.user_id AND p.n > 1",
                "CREATE VIEW `v6` AS select `u`.`id` AS `uid`,`p`.`id` AS `pid`,`p`.`n` AS `n` from (`users` `u` left join `posts` `p` on(((`u`.`id` = `p`.`user_id`) and (`p`.`n` > 1))))",
                "select `u`.`id` AS `uid`,`p`.`id` AS `pid`,`p`.`n` AS `n` from `users` AS `u` LEFT JOIN `posts` AS `p` ON ((`u`.`id` = `p`.`user_id`) and (`p`.`n` > 1))",
                "select `u`.`id` AS `uid`,`p`.`id` AS `pid`,`p`.`n` AS `n` from (`probe`.`users` `u` left join `probe`.`posts` `p` on(((`u`.`id` = `p`.`user_id`) and (`p`.`n` > 1))))",
                false,
            ),
            (
                "CREATE VIEW v8 AS SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id JOIN comments c ON c.post_id = p.id WHERE c.note <> 'x' AND u.age IS NOT NULL",
                "CREATE VIEW `v8` AS select `u`.`name` AS `name`,`p`.`title` AS `title` from ((`users` `u` join `posts` `p` on((`p`.`user_id` = `u`.`id`))) join `comments` `c` on((`c`.`post_id` = `p`.`id`))) where ((`c`.`note` <> 'x') and (`u`.`age` is not null))",
                "select `u`.`name` AS `name`,`p`.`title` AS `title` from `users` AS `u` JOIN `posts` AS `p` ON (`p`.`user_id` = `u`.`id`) JOIN `comments` AS `c` ON (`c`.`post_id` = `p`.`id`) where ((`c`.`note` <> 'x') and (`u`.`age` is not null))",
                "select `u`.`name` AS `name`,`p`.`title` AS `title` from ((`probe`.`users` `u` join `probe`.`posts` `p` on((`p`.`user_id` = `u`.`id`))) join `probe`.`comments` `c` on((`c`.`post_id` = `p`.`id`))) where ((`c`.`note` <> 'x') and (`u`.`age` is not null))",
                true,
            ),
            (
                "CREATE VIEW w4 AS SELECT id FROM posts p WHERE p.user_id = 1",
                "CREATE VIEW `w4` AS select `p`.`id` AS `id` from `posts` `p` where (`p`.`user_id` = 1)",
                "select `p`.`id` AS `id` from `posts` AS `p` where (`p`.`user_id` = 1)",
                "select `p`.`id` AS `id` from `probe`.`posts` `p` where (`p`.`user_id` = 1)",
                true,
            ),
            (
                "CREATE VIEW w3 AS SELECT p.id FROM posts p",
                "CREATE VIEW `w3` AS select `p`.`id` AS `id` from `posts` `p`",
                "select `p`.`id` AS `id` from `posts` AS `p`",
                "select `p`.`id` AS `id` from `probe`.`posts` `p`",
                true,
            ),
            (
                "CREATE VIEW w1 AS SELECT id, title FROM posts WHERE user_id > 1",
                "CREATE VIEW `w1` AS select `posts`.`id` AS `id`,`posts`.`title` AS `title` from `posts` where (`posts`.`user_id` > 1)",
                "select `id` AS `id`,`title` AS `title` from `posts` where (`user_id` > 1)",
                "select `probe`.`posts`.`id` AS `id`,`probe`.`posts`.`title` AS `title` from `probe`.`posts` where (`probe`.`posts`.`user_id` > 1)",
                true,
            ),
            (
                "CREATE VIEW w2 AS SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id",
                "CREATE VIEW `w2` AS select `posts`.`user_id` AS `user_id`,count(0) AS `c` from `posts` group by `posts`.`user_id`",
                "select `user_id` AS `user_id`,COUNT(*) AS `c` from `posts` group by `user_id`",
                "select `probe`.`posts`.`user_id` AS `user_id`,count(0) AS `c` from `probe`.`posts` group by `probe`.`posts`.`user_id`",
                false,
            ),
        ] {
            let written = written(sql).unwrap_or_else(|| panic!("{sql}"));
            assert_eq!(written, expected, "{sql}");
            assert_eq!(self::written(&written).as_deref(), Some(expected));
            let mode = SessionSqlMode::default();
            assert_eq!(translated_view_select(&written, mode).unwrap(), translated);
            assert_eq!(
                written_view_definition(&written, mode, "probe").unwrap(),
                (definition.to_owned(), updatable),
                "{sql}"
            );
        }

        let dumped = "/*!50001 CREATE ALGORITHM=UNDEFINED */\n/*!50013 DEFINER=`root`@`%` SQL SECURITY DEFINER */\n/*!50001 VIEW `user_posts` AS select `u`.`name` AS `name`,`p`.`title` AS `title` from (`users` `u` join `posts` `p` on((`p`.`user_id` = `u`.`id`))) */";
        assert_eq!(
            written(dumped).as_deref(),
            Some("CREATE VIEW `user_posts` AS select `u`.`name` AS `name`,`p`.`title` AS `title` from (`users` `u` join `posts` `p` on((`p`.`user_id` = `u`.`id`)))")
        );

        let readings = written_view_columns(
            "CREATE VIEW `v2` AS select `u`.`id` AS `id`,`u`.`name` AS `author`,`p`.`title` AS `title` from (`users` `u` left join `posts` `p` on((`p`.`user_id` = `u`.`id`))) where (`u`.`id` > 1)",
            SessionSqlMode::default(),
        )
        .unwrap();
        assert!(!readings.grouped());
        assert!(!readings.reads_one_table_by_its_name());
        assert_eq!(
            readings
                .sources()
                .iter()
                .map(|source| (source.table().as_str(), source.outer()))
                .collect::<Vec<_>>(),
            [("users", false), ("posts", true)]
        );
        assert_eq!(
            readings.columns()[1],
            (
                "author".to_owned(),
                MySqlViewColumnReading::Column {
                    source: 0,
                    column: "name".to_owned()
                }
            )
        );
    }

    #[test]
    fn a_join_mysql_prints_by_rules_not_measured_is_refused() {
        for sql in [
            // MySQL prints a comma as a join, `USING` as the `ON` it stands
            // for, and a `RIGHT JOIN` turned around.
            "CREATE VIEW v AS SELECT u.name, p.title FROM users u, posts p WHERE p.user_id = u.id",
            "CREATE VIEW v AS SELECT u.name, p.title FROM users u JOIN posts p USING (id)",
            "CREATE VIEW v AS SELECT u.name, p.title FROM users u RIGHT JOIN posts p ON p.user_id = u.id",
            "CREATE VIEW v AS SELECT u.name, p.title FROM users u CROSS JOIN posts p",
            "CREATE VIEW v AS SELECT u.name, p.title FROM users u NATURAL JOIN posts p",
            "CREATE VIEW v AS SELECT u.name FROM users u JOIN (posts p JOIN comments c ON c.post_id = p.id) ON p.user_id = u.id",
            // 1052, 1060, 1066 and 1054 in MySQL.
            "CREATE VIEW v AS SELECT id FROM users u JOIN posts p ON p.user_id = u.id",
            "CREATE VIEW v AS SELECT u.id, p.id FROM users u JOIN posts p ON p.user_id = u.id",
            "CREATE VIEW v AS SELECT users.id FROM users JOIN users ON users.id = users.id",
            "CREATE VIEW v AS SELECT posts.id FROM posts p",
            "CREATE VIEW v AS SELECT u.missing FROM users u JOIN posts p ON p.user_id = u.id",
            // Not measured.
            "CREATE VIEW v AS SELECT * FROM users u JOIN posts p ON p.user_id = u.id",
            "CREATE VIEW v AS SELECT u.name, COUNT(*) AS c FROM users u JOIN posts p ON p.user_id = u.id GROUP BY u.name",
            "CREATE VIEW v AS SELECT COUNT(*) AS c FROM posts p",
            "CREATE VIEW v AS SELECT u.name FROM users u JOIN plain x ON x.id = u.id",
            "CREATE VIEW v AS SELECT u.name FROM users u JOIN posts p ON p.user_id = u.id ORDER BY u.name",
        ] {
            assert!(
                view_written_as_mysql_prints_it(sql, SessionSqlMode::default(), &declared).is_err(),
                "{sql}"
            );
        }
    }

    /// Each expectation is what MySQL 8.4.11 printed for the same statement.
    #[test]
    fn a_view_with_a_condition_is_written_the_way_mysql_prints_it() {
        for (sql, expected) in [
            (
                "CREATE VIEW v3 AS SELECT id, title FROM posts WHERE n > 1",
                "CREATE VIEW `v3` AS select `posts`.`id` AS `id`,`posts`.`title` AS `title` from `posts` where (`posts`.`n` > 1)",
            ),
            (
                "CREATE VIEW x1 AS SELECT id FROM posts WHERE (n > 1 AND (n < 5 AND title = 'a')) OR (user_id = 2 OR user_id = 3)",
                "CREATE VIEW `x1` AS select `posts`.`id` AS `id` from `posts` where (((`posts`.`n` > 1) and (`posts`.`n` < 5) and (`posts`.`title` = 'a')) or (`posts`.`user_id` = 2) or (`posts`.`user_id` = 3))",
            ),
            (
                "CREATE VIEW x2 AS SELECT id FROM posts WHERE n > 1 AND n < 5 OR n = 7 AND title <> 'x'",
                "CREATE VIEW `x2` AS select `posts`.`id` AS `id` from `posts` where (((`posts`.`n` > 1) and (`posts`.`n` < 5)) or ((`posts`.`n` = 7) and (`posts`.`title` <> 'x')))",
            ),
            (
                "CREATE VIEW x3 AS SELECT id FROM posts WHERE title = 'it''s' OR title = 'back\\\\slash' OR title = 'é'",
                "CREATE VIEW `x3` AS select `posts`.`id` AS `id` from `posts` where ((`posts`.`title` = 'it\\'s') or (`posts`.`title` = 'back\\\\slash') or (`posts`.`title` = 'é'))",
            ),
            (
                "CREATE VIEW x4 AS SELECT id FROM posts WHERE user_id IS NOT NULL AND title IS NULL",
                "CREATE VIEW `x4` AS select `posts`.`id` AS `id` from `posts` where ((`posts`.`user_id` is not null) and (`posts`.`title` is null))",
            ),
            (
                "CREATE VIEW x8 AS SELECT id FROM posts WHERE 1 < n",
                "CREATE VIEW `x8` AS select `posts`.`id` AS `id` from `posts` where (1 < `posts`.`n`)",
            ),
            (
                "CREATE VIEW x9 AS SELECT id FROM posts WHERE n = TRUE AND title = \"dq\" AND n = FALSE AND n = NULL AND n <=> 1 AND n != 1 AND 'a' = title",
                "CREATE VIEW `x9` AS select `posts`.`id` AS `id` from `posts` where ((`posts`.`n` = true) and (`posts`.`title` = 'dq') and (`posts`.`n` = false) and (`posts`.`n` = NULL) and (`posts`.`n` <=> 1) and (`posts`.`n` <> 1) and ('a' = `posts`.`title`))",
            ),
            (
                "CREATE VIEW x2 AS SELECT id AS pid, title AS title FROM posts WHERE n = user_id",
                "CREATE VIEW `x2` AS select `posts`.`id` AS `pid`,`posts`.`title` AS `title` from `posts` where (`posts`.`n` = `posts`.`user_id`)",
            ),
            (
                "CREATE VIEW y1 AS SELECT ID, Title AS T, posts.N FROM posts WHERE N > 1 AND TITLE = 'a'",
                "CREATE VIEW `y1` AS select `posts`.`id` AS `ID`,`posts`.`title` AS `T`,`posts`.`n` AS `N` from `posts` where ((`posts`.`n` > 1) and (`posts`.`title` = 'a'))",
            ),
        ] {
            let written = written(sql).unwrap_or_else(|| panic!("{sql}"));
            assert_eq!(written, expected, "{sql}");
            // What was written reads back as itself.
            assert_eq!(self::written(&written).as_deref(), Some(expected));
        }
        assert_eq!(written("CREATE VIEW v1 AS SELECT id FROM posts"), None);
    }

    #[test]
    fn a_view_condition_mysql_writes_by_rules_not_measured_is_refused() {
        for sql in [
            "CREATE VIEW v AS SELECT id FROM posts WHERE n IN (1, 2)",
            "CREATE VIEW v AS SELECT id FROM posts WHERE title LIKE 'a%'",
            "CREATE VIEW v AS SELECT id FROM posts WHERE n BETWEEN 1 AND 2",
            "CREATE VIEW v AS SELECT id FROM posts WHERE NOT (n < 0)",
            "CREATE VIEW v AS SELECT id FROM posts WHERE n = -1",
            "CREATE VIEW v AS SELECT id FROM posts WHERE n = 1.5",
            "CREATE VIEW v AS SELECT id FROM posts WHERE n = 007",
            "CREATE VIEW v AS SELECT id FROM posts WHERE title = 'a\tb'",
            "CREATE VIEW v AS SELECT id FROM posts WHERE n = ?",
            "CREATE VIEW v AS SELECT id FROM posts WHERE missing = 1",
            "CREATE VIEW v AS SELECT other.id FROM posts WHERE n = 1",
            "CREATE VIEW v AS SELECT id FROM plain WHERE id = 1",
            "CREATE VIEW v AS SELECT id + 1 AS i FROM posts WHERE n = 1",
            "CREATE VIEW v AS SELECT id FROM posts WHERE n = 1 ORDER BY id",
        ] {
            assert!(
                view_written_as_mysql_prints_it(sql, SessionSqlMode::default(), &declared).is_err(),
                "{sql}"
            );
        }
        let no_backslash_escapes = SessionSqlMode {
            ansi_quotes: false,
            no_backslash_escapes: true,
        };
        assert!(view_written_as_mysql_prints_it(
            "CREATE VIEW v AS SELECT id FROM posts WHERE title = 'it''s'",
            no_backslash_escapes,
            &declared
        )
        .is_err());
    }
}
