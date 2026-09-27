//! A view's `SELECT` written the way MySQL prints it back.
//!
//! MySQL keeps a view as the text it prints in `SHOW CREATE VIEW`: every
//! column qualified by its table and named with an `AS`, keywords in lower
//! case, and each comparison and each run of `AND` or `OR` in parentheses of
//! its own. A view whose `SELECT` carries a condition is kept here in that
//! same text, so what `SHOW CREATE VIEW` prints is what was stored.

use super::*;
use sqlparser::ast::Query;

/// Names the columns a base table declares, or nothing for any other name.
pub type DeclaredColumns<'a> = dyn Fn(&MySqlTableName) -> Option<Vec<String>> + 'a;

/// Writes a `CREATE VIEW` whose `SELECT` carries a `WHERE` the way MySQL
/// prints it back, or answers nothing for any other `CREATE VIEW`.
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
    if !has_a_condition(&view.query) {
        return Ok(None);
    }
    refuse_view_options(&view)?;
    let [ObjectNamePart::Identifier(name)] = view.name.0.as_slice() else {
        return unsupported("qualified view name");
    };
    let name = MySqlTableName::parse(&name.value)?;
    Ok(Some(format!(
        "CREATE VIEW `{}` AS {}",
        name.as_str().replace('`', "``"),
        view_body(&view.query, mode, Some(declared_columns))?
    )))
}

/// The MySQL DDL a view is kept under.
///
/// A view with a condition is kept in the text MySQL prints, which the engine
/// statement cannot be rendered back into — a `LIKE` or a `<=>` has become
/// something else by then — so the text it was made from is kept, once it is
/// proved to translate to that same statement. Any other view is rendered
/// from its statement.
pub fn mysql_create_view_ddl(
    statement: &Stmt,
    written: &str,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    if !translated_view_has_a_condition(statement) {
        return render_create_view_mysql_with_mode(statement, mode);
    }
    if parse_create_view_ast(written, mode)? != *statement {
        return unsupported("a view with a condition written other than as it was made");
    }
    Ok(written.to_owned())
}

/// Reads the `SELECT` of a `CREATE VIEW` written the way MySQL prints it, as
/// that same text.
pub fn written_view_select(sql: &str, mode: SessionSqlMode) -> Result<String, ParseError> {
    let Statement::CreateView(view) = parse_one_statement(sql, mode)? else {
        return Err(ParseError::ExpectedCreateView);
    };
    view_body(&view.query, mode, None)
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
        view_body(&view.query, mode, None)?
    ))
}

/// Reports whether a view's `SELECT` carries a `WHERE`, which is the shape
/// kept in the text MySQL prints.
pub(crate) fn has_a_condition(query: &Query) -> bool {
    matches!(query.body.as_ref(), SetExpr::Select(select) if select.selection.is_some())
}

/// Reports whether a translated view's `SELECT` carries a `WHERE`.
pub fn translated_view_has_a_condition(statement: &Stmt) -> bool {
    matches!(
        statement,
        Stmt::CreateView { select, .. }
            if matches!(&select.body.select, OneSelect::Select { where_clause: Some(_), .. })
    )
}

/// Writes a view's `SELECT` the way MySQL prints it, refusing every shape
/// whose printing has not been measured.
///
/// Without `declared_columns` each column keeps the spelling it was written
/// with, which is what the translation of text already written this way
/// reads.
pub(crate) fn view_body(
    query: &Query,
    mode: SessionSqlMode,
    declared_columns: Option<&DeclaredColumns<'_>>,
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
    let table = MySqlTableName::parse(&table.value)?;
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
    };
    let columns = select
        .projection
        .iter()
        .map(|item| body.projected_column(item))
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE VIEW without projections");
    }
    let mut written = format!(
        "select {} from {}",
        columns.join(","),
        quoted_name(table.as_str())
    );
    if let Some(condition) = &select.selection {
        written.push_str(" where ");
        written.push_str(&body.condition(condition)?);
    }
    Ok(written)
}

/// What writing one view's `SELECT` needs to know.
struct ViewBody<'a> {
    table: &'a MySqlTableName,
    declared: Option<&'a [String]>,
    mode: SessionSqlMode,
}

impl ViewBody<'_> {
    /// Writes one result column, `` `t`.`c` AS `name` ``, named after its
    /// alias or, without one, after the column as it was written.
    fn projected_column(&self, item: &SelectItem) -> Result<String, ParseError> {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
            _ => return unsupported("CREATE VIEW projection"),
        };
        let Some(column) = self.written_column(expr) else {
            return unsupported("CREATE VIEW projection");
        };
        let name = alias.unwrap_or(column);
        Ok(format!(
            "{} AS {}",
            self.column(column)?,
            quoted_name(&name.value)
        ))
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
        Ok(format!(
            "{}.{}",
            quoted_name(self.table.as_str()),
            quoted_name(declared)
        ))
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
        (table.as_str() == "posts")
            .then(|| ["id", "user_id", "n", "title"].map(str::to_owned).to_vec())
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
