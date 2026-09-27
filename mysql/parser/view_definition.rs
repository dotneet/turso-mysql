//! A view's `SELECT` written the way MySQL prints it back.
//!
//! MySQL keeps a view as the text it prints in `SHOW CREATE VIEW`: every
//! column qualified by its table and named with an `AS`, keywords in lower
//! case, and each comparison and each run of `AND` or `OR` in parentheses of
//! its own. A view whose `SELECT` carries a condition, groups its rows or
//! aggregates them is kept here in that same text, so what `SHOW CREATE VIEW`
//! prints is what was stored.

use super::*;
use sqlparser::ast::Query;

/// Names the columns a base table declares, or nothing for any other name.
pub type DeclaredColumns<'a> = dyn Fn(&MySqlTableName) -> Option<Vec<String>> + 'a;

/// Writes a `CREATE VIEW` kept in the text MySQL prints the way MySQL prints
/// it back, or answers nothing for any other `CREATE VIEW`.
///
/// `declared_columns` names the columns a table declares, which is how MySQL
/// writes a column however the statement spelled it: measured on 8.4.11,
/// `SELECT ID FROM posts` is kept as `` `posts`.`id` AS `ID` ``.
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

/// What each column of a view kept in the text MySQL prints reads, and
/// whether the view groups its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlWrittenView {
    table: MySqlTableName,
    grouped: bool,
    columns: Vec<(String, MySqlViewColumnReading)>,
}

impl MySqlWrittenView {
    /// Returns the one table the view reads.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
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

/// What one column of a view reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlViewColumnReading {
    /// A column of the table, named as the table declares it.
    Column(String),
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
    let (select, table) = one_table_select(&view.query)?;
    let body = ViewBody {
        table: &table,
        declared: None,
        mode,
        written: Written::AsMySqlPrintsIt,
    };
    let mut grouped = !group_by_columns(select)?.is_empty();
    let mut columns = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        let (expr, alias) = projected(item)?;
        let reading = match body.written_column(expr) {
            Some(column) => MySqlViewColumnReading::Column(column.value.clone()),
            None => {
                grouped = true;
                let (function, argument) = aggregate(expr)?;
                let argument = match argument {
                    Some(argument) => Some(
                        body.written_column(argument)
                            .ok_or(ParseError::Unsupported {
                                feature: "CREATE VIEW aggregate over something other than a column",
                            })?
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
            (None, MySqlViewColumnReading::Column(column)) => column.clone(),
            (None, _) => return unsupported("CREATE VIEW aggregate without a name"),
        };
        columns.push((name, reading));
    }
    Ok(MySqlWrittenView {
        table,
        grouped,
        columns,
    })
}

/// Reports whether a view is kept in the text MySQL prints: one whose
/// `SELECT` carries a condition, groups its rows or aggregates them.
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
}

/// Which of the two texts a view's `SELECT` is written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    /// As MySQL prints it back: `` `t`.`c` ``, `count(0)`, `group by`.
    AsMySqlPrintsIt,
    /// As the `SELECT` translator reads it: each column bare, `COUNT(*)`.
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
    written: Written,
) -> Result<String, ParseError> {
    let (select, table) = one_table_select(query)?;
    let declared = match declared_columns {
        Some(declared_columns) => {
            Some(declared_columns(&table).ok_or(ParseError::Unsupported {
                feature: "CREATE VIEW over something other than a base table",
            })?)
        }
        None => None,
    };
    let body = ViewBody {
        table: &table,
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
    let mut text = format!(
        "select {} from {}",
        columns.join(","),
        quoted_name(table.as_str())
    );
    if let Some(condition) = &select.selection {
        text.push_str(" where ");
        text.push_str(&body.condition(condition)?);
    }
    let grouping = group_by_columns(select)?;
    if !grouping.is_empty() {
        let grouping = grouping
            .iter()
            .map(|expr| match body.written_column(expr) {
                Some(column) => body.column(column),
                None => unsupported("CREATE VIEW grouping by something other than a column"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        text.push_str(" group by ");
        text.push_str(&grouping.join(","));
    }
    Ok(text)
}

/// Reads a view's `SELECT` over one table, refusing every clause whose
/// printing has not been measured.
fn one_table_select(
    query: &Query,
) -> Result<(&sqlparser::ast::Select, MySqlTableName), ParseError> {
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
    let [from] = select.from.as_slice() else {
        return unsupported("CREATE VIEW FROM clause");
    };
    if !from.joins.is_empty() {
        return unsupported("CREATE VIEW JOIN");
    }
    let TableFactor::Table {
        name,
        alias: None,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_hints,
    } = &from.relation
    else {
        return unsupported("CREATE VIEW table source");
    };
    if !with_hints.is_empty() || !partitions.is_empty() || !index_hints.is_empty() {
        return unsupported("CREATE VIEW table option");
    }
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("CREATE VIEW over a qualified table");
    };
    Ok((select, MySqlTableName::parse(&table.value)?))
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
    table: &'a MySqlTableName,
    declared: Option<&'a [String]>,
    mode: SessionSqlMode,
    written: Written,
}

impl ViewBody<'_> {
    /// Writes one result column, `` `t`.`c` AS `name` ``, named after its
    /// alias or, without one, after the column as it was written. An
    /// aggregate has to be named: MySQL names one after the text it was
    /// written as, spacing and all.
    fn projected_column(&self, item: &SelectItem) -> Result<String, ParseError> {
        let (expr, alias) = projected(item)?;
        if let Some(column) = self.written_column(expr) {
            let name = alias.unwrap_or(column);
            return Ok(format!(
                "{} AS {}",
                self.column(column)?,
                quoted_name(&name.value)
            ));
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

    /// Writes an aggregate: measured on MySQL 8.4.11, `COUNT(*)` is printed
    /// `count(0)` and the rest by their names in lower case over their
    /// qualified column, `count(distinct ...)` among them.
    fn aggregate(&self, expr: &Expr) -> Result<String, ParseError> {
        let (function, argument) = aggregate(expr)?;
        let argument = match argument {
            Some(argument) => match self.written_column(argument) {
                Some(column) => Some(self.column(column)?),
                None => {
                    return unsupported("CREATE VIEW aggregate over something other than a column")
                }
            },
            None => None,
        };
        let prints = self.written == Written::AsMySqlPrintsIt;
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

    /// Reads the column an expression names, refusing a qualifier naming any
    /// table but the view's own.
    fn written_column<'e>(&self, expr: &'e Expr) -> Option<&'e Ident> {
        match expr {
            Expr::Identifier(column) => Some(column),
            Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [qualifier, column]
                    if qualifier.value.eq_ignore_ascii_case(self.table.as_str()) =>
                {
                    Some(column)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Writes a column: qualified by its table, as MySQL prints it, or bare,
    /// as the translator reads the one table a view reads.
    fn column(&self, column: &Ident) -> Result<String, ParseError> {
        let declared = match self.declared {
            Some(declared) => declared
                .iter()
                .find(|declared| declared.eq_ignore_ascii_case(&column.value))
                .ok_or(ParseError::Unsupported {
                    feature: "CREATE VIEW naming a column its table does not have",
                })?
                .as_str(),
            None => column.value.as_str(),
        };
        Ok(match self.written {
            Written::AsMySqlPrintsIt => format!(
                "{}.{}",
                quoted_name(self.table.as_str()),
                quoted_name(declared)
            ),
            Written::ForTheTranslator => quoted_name(declared),
        })
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
        if let Some(column) = self.written_column(expr) {
            return self.column(column);
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
                    MySqlViewColumnReading::Column("grp".to_owned())
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
            "CREATE VIEW v AS SELECT id FROM posts p WHERE p.n = 1",
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
