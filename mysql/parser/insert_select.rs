use super::{
    read_one_statement, unsupported, MySqlTableName, ParseError, SessionMySqlDialect,
    SessionSqlMode,
};
use crate::statement_reads;
use sqlparser::ast::{
    BinaryOperator, Expr, ObjectNamePart, SelectFlavor, SelectItem, SetExpr, Statement,
    TableFactor, Value,
};
use sqlparser::tokenizer::{Location, Token};

/// One `INSERT INTO t <SELECT>` written with no column list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInsertSelectWithoutColumns {
    table: MySqlTableName,
    replaces: bool,
    ignores: bool,
    select_sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlDirectInsertSelectProjection {
    All,
    Columns(Vec<String>),
    /// Each value a column read, or `None` for a written whole number, NULL
    /// or a `?`, which reads nothing of the source.
    ColumnsAndLiterals(Vec<Option<String>>),
    IntegerArithmetic(Vec<Vec<String>>),
}

/// Identifies a plain one-table copy whose projected values are stored columns.
pub fn direct_insert_select_projection(
    sql: &str,
    mode: SessionSqlMode,
) -> Option<MySqlDirectInsertSelectProjection> {
    insert_select_projection(sql, mode, false, false)
}

pub fn filtered_insert_select_projection(
    sql: &str,
    mode: SessionSqlMode,
) -> Option<MySqlDirectInsertSelectProjection> {
    insert_select_projection(sql, mode, true, true)
}

fn insert_select_projection(
    sql: &str,
    mode: SessionSqlMode,
    allow_filter: bool,
    allow_arithmetic: bool,
) -> Option<MySqlDirectInsertSelectProjection> {
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Insert(insert)) = read_statement.as_ref() else {
        return None;
    };
    if insert.on.is_some() || insert.ignore || insert.replace_into {
        return None;
    }
    let query = insert.source.as_deref()?;
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
        return None;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
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
        || (select.selection.is_some() && !allow_filter)
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
        return None;
    }
    let [from] = select.from.as_slice() else {
        return None;
    };
    if !from.joins.is_empty() || !matches!(&from.relation, TableFactor::Table { alias: None, .. }) {
        return None;
    }
    if matches!(select.projection.as_slice(), [SelectItem::Wildcard(_)]) {
        return Some(MySqlDirectInsertSelectProjection::All);
    }
    if allow_arithmetic {
        let arithmetic = select
            .projection
            .iter()
            .map(|item| {
                let expr = match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                    _ => return None,
                };
                let Expr::BinaryOp { .. } = expr else {
                    return None;
                };
                let columns = integer_arithmetic_columns(expr)?;
                (!columns.is_empty()).then_some(columns)
            })
            .collect::<Option<Vec<_>>>();
        if let Some(columns) = arithmetic {
            return Some(MySqlDirectInsertSelectProjection::IntegerArithmetic(
                columns,
            ));
        }
    }
    let values = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(Expr::Identifier(column))
            | SelectItem::ExprWithAlias {
                expr: Expr::Identifier(column),
                ..
            } => Some(Some(column.value.clone())),
            // A `?` reads nothing of the source either: sqlx copies a user's
            // id beside a bound title with `SELECT id, ? FROM users ...`.
            SelectItem::UnnamedExpr(Expr::Value(value))
                if matches!(&value.value,
                    Value::Number(written, false)
                        if written.bytes().all(|byte| byte.is_ascii_digit())
                            && written.parse::<i64>().is_ok())
                    || matches!(&value.value, Value::Null)
                    || matches!(&value.value, Value::Placeholder(marker) if marker == "?") =>
            {
                Some(None)
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    if values.is_empty() {
        return None;
    }
    if values.iter().all(Option::is_some) {
        return Some(MySqlDirectInsertSelectProjection::Columns(
            values.into_iter().map(Option::unwrap).collect(),
        ));
    }
    Some(MySqlDirectInsertSelectProjection::ColumnsAndLiterals(
        values,
    ))
}

fn integer_arithmetic_columns(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Identifier(column) => Some(vec![column.value.clone()]),
        Expr::Value(value)
            if matches!(&value.value,
            Value::Number(written, false)
                if written.bytes().all(|byte| byte.is_ascii_digit())
                    && written.parse::<i64>().is_ok()) =>
        {
            Some(Vec::new())
        }
        Expr::Nested(inner) => integer_arithmetic_columns(inner),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply,
            right,
        } => {
            let mut columns = integer_arithmetic_columns(left)?;
            columns.extend(integer_arithmetic_columns(right)?);
            Some(columns)
        }
        _ => None,
    }
}

impl MySqlInsertSelectWithoutColumns {
    /// Returns the table the statement writes.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Reports whether the statement was written as a `REPLACE`.
    pub const fn replaces(&self) -> bool {
        self.replaces
    }

    /// Reports whether the statement was written `INSERT IGNORE`.
    pub const fn ignores(&self) -> bool {
        self.ignores
    }

    /// Returns the `SELECT` as MySQL, for the statement written with the
    /// column list the table gives it.
    pub fn select_sql(&self) -> &str {
        &self.select_sql
    }
}

/// Reads an `INSERT INTO t <SELECT>` that names no columns.
///
/// MySQL takes every column of the table, in order — measured on 8.4.11,
/// `INSERT INTO dst SELECT * FROM src` copies all three columns, and a `SELECT`
/// answering a different number of them answers 1136. The caller writes the
/// column list out and runs the ordinary statement, which is what the form
/// means.
///
/// Returns `None` for anything else, so every other `INSERT` keeps its own
/// path. `IGNORE` is kept, and the statement written out skips a colliding
/// row as the one naming its columns does. A statement carrying an upsert
/// clause is refused, as that form is wherever it is written.
pub fn parse_optional_insert_select_without_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInsertSelectWithoutColumns>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Insert(insert)) = read_statement.as_ref() else {
        return Ok(None);
    };
    if !insert.columns.is_empty() {
        return Ok(None);
    }
    let Some(source) = insert.source.as_deref() else {
        return Ok(None);
    };
    if matches!(source.body.as_ref(), SetExpr::Values(_)) {
        return Ok(None);
    }
    if insert.on.is_some() || insert.partitioned.is_some() {
        return unsupported("INSERT SELECT option");
    }
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return unsupported("INSERT target");
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("schema-qualified INSERT target");
    };
    let table = MySqlTableName::parse(&table.value).map_err(|_| ParseError::Unsupported {
        feature: "INSERT target name",
    })?;
    Ok(Some(MySqlInsertSelectWithoutColumns {
        table,
        replaces: insert.replace_into,
        ignores: insert.ignore,
        select_sql: source.to_string(),
    }))
}

/// One `INSERT INTO t (a, b) <SELECT>`, the statement Laravel's `insertUsing`
/// and a data migration write to copy rows into a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInsertSelect {
    table: MySqlTableName,
    columns: Vec<String>,
}

impl MySqlInsertSelect {
    /// Returns the table the statement writes.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the columns the statement names, in the order it names them.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

/// Reads an `INSERT INTO t (a, b) <SELECT>`.
///
/// Returns `None` for anything else, so every other `INSERT` keeps its own
/// path. `IGNORE`, `REPLACE` and an upsert clause are refused: each decides
/// what a colliding row does, which has not been measured beside a `SELECT`.
pub fn parse_optional_insert_select(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInsertSelect>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Insert(insert)) = read_statement.as_ref() else {
        return Ok(None);
    };
    if insert.columns.is_empty() || !insert.assignments.is_empty() {
        return Ok(None);
    }
    let Some(source) = insert.source.as_deref() else {
        return Ok(None);
    };
    if matches!(source.body.as_ref(), SetExpr::Values(_)) {
        return Ok(None);
    }
    if insert.ignore || insert.replace_into || insert.on.is_some() {
        return unsupported("INSERT SELECT option");
    }
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return unsupported("INSERT target");
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("schema-qualified INSERT target");
    };
    let table = MySqlTableName::parse(&table.value).map_err(|_| ParseError::Unsupported {
        feature: "INSERT target name",
    })?;
    let columns = insert
        .columns
        .iter()
        .map(|column| match column.0.as_slice() {
            [ObjectNamePart::Identifier(column)] => Ok(column.value.clone()),
            _ => unsupported("qualified INSERT column"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(MySqlInsertSelect { table, columns }))
}

/// Answers the `SELECT` of an `INSERT INTO t (a, b) <SELECT>` as it was
/// written, for the frontend to render knowing its columns' types.
///
/// The `SELECT` is the rest of the statement after the parenthesis closing the
/// column list, which is the first one the statement opens. Every `?` of the
/// statement stands in it, the column list holding none, so each keeps its
/// number. Answers `None` for any other statement.
pub fn insert_select_source_sql(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Insert(insert)) = read_statement.as_ref() else {
        return Ok(None);
    };
    if insert.columns.is_empty() || !insert.assignments.is_empty() {
        return Ok(None);
    }
    if insert
        .source
        .as_deref()
        .is_none_or(|source| matches!(source.body.as_ref(), SetExpr::Values(_)))
    {
        return Ok(None);
    }
    if insert.partitioned.is_some() || insert.on.is_some() {
        return unsupported("INSERT SELECT option");
    }
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let mut depth = 0_usize;
    let mut column_list_closed = false;
    for token in &tokens {
        if column_list_closed {
            if matches!(token.token, Token::Whitespace(_)) {
                continue;
            }
            let start = byte_offset_of(sql, token.span.start).ok_or(ParseError::Unsupported {
                feature: "INSERT SELECT source position",
            })?;
            return Ok(Some(sql[start..].to_owned()));
        }
        match token.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.checked_sub(1).ok_or(ParseError::Unsupported {
                    feature: "INSERT SELECT column list",
                })?;
                column_list_closed = depth == 0;
            }
            _ => {}
        }
    }
    unsupported("INSERT SELECT source position")
}

/// One `INSERT INTO t VALUES (...)` written with no column list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInsertValuesWithoutColumns {
    table: MySqlTableName,
    column_list_at: usize,
}

impl MySqlInsertValuesWithoutColumns {
    /// Returns the table the statement writes.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Where the column list goes, as a byte offset into the statement.
    ///
    /// The caller writes the list in there rather than rendering the statement
    /// again, because a written value's own spelling is the one thing that must
    /// not change on the way through.
    pub const fn column_list_at(&self) -> usize {
        self.column_list_at
    }
}

/// Reads an `INSERT INTO t VALUES (...)` that names no columns.
///
/// MySQL takes every column of the table, in order — measured on 8.4.11,
/// `INSERT INTO t VALUES (1, 'a')` writes both columns, and a row of a
/// different number of values answers 1136. This is how mysqldump writes every
/// data row, so a dumped table's rows arrive in exactly this shape.
///
/// Returns `None` for anything else, so every other `INSERT` keeps its own
/// path.
pub fn parse_optional_insert_values_without_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInsertValuesWithoutColumns>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Insert(insert)) = read_statement.as_ref() else {
        return Ok(None);
    };
    if !insert.columns.is_empty() || !insert.assignments.is_empty() {
        return Ok(None);
    }
    let Some(source) = insert.source.as_deref() else {
        return Ok(None);
    };
    let SetExpr::Values(_) = source.body.as_ref() else {
        return Ok(None);
    };
    if insert.partitioned.is_some() {
        return unsupported("INSERT VALUES option");
    }
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return unsupported("INSERT target");
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("schema-qualified INSERT target");
    };
    let table = MySqlTableName::parse(&table.value).map_err(|_| ParseError::Unsupported {
        feature: "INSERT target name",
    })?;
    let Some(column_list_at) = where_the_values_begin(sql, mode)? else {
        return Ok(None);
    };
    Ok(Some(MySqlInsertValuesWithoutColumns {
        table,
        column_list_at,
    }))
}

/// Where the `VALUES` keyword stands in the statement, as a byte offset.
///
/// The word is found through the tokenizer rather than by searching the text,
/// so a `VALUES` inside a quoted name, a written value or a comment is not
/// mistaken for the keyword.
///
/// Answers `None` where a `)` stands just before it, which is
/// `INSERT INTO t () VALUES ()` — a column list the statement wrote itself,
/// empty, which sqlparser reads as no list at all and which means the row of
/// defaults rather than every column.
fn where_the_values_begin(sql: &str, mode: SessionSqlMode) -> Result<Option<usize>, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let Some(at) = words.iter().position(|token| {
        matches!(&token.token, Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("VALUES"))
    }) else {
        return Ok(None);
    };
    if at > 0 && matches!(words[at - 1].token, Token::RParen) {
        return Ok(None);
    }
    Ok(byte_offset_of(sql, words[at].span.start))
}

/// The byte offset one line-and-column location stands at.
fn byte_offset_of(sql: &str, location: Location) -> Option<usize> {
    if location.line == 0 || location.column == 0 {
        return None;
    }
    let (mut line, mut column) = (1, 1);
    for (offset, character) in sql.char_indices() {
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
    (line == location.line && column == location.column).then_some(sql.len())
}

/// Writes `INSERT INTO t SET a = 1, b = 2` out as the column-list form.
///
/// The two forms mean the same row — measured on MySQL 8.4.11,
/// `INSERT INTO ai SET v = 1, s = 'a'` stores what
/// `INSERT INTO ai (v, s) VALUES (1, 'a')` stores, `LAST_INSERT_ID()` answers 1
/// either way, and a column the `SET` leaves out takes its default. Writing it
/// out here as MySQL is what lets the `AUTO_INCREMENT` path, which reads only
/// the column-list form, answer the `SET` one too.
///
/// Returns `None` for every other statement. An upsert clause is refused, as it
/// is on the `SET` form wherever it is written.
pub fn parse_optional_insert_set_as_values(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Insert(insert)) = read_statement.as_ref() else {
        return Ok(None);
    };
    if insert.assignments.is_empty() {
        return Ok(None);
    }
    if !insert.columns.is_empty() || insert.source.is_some() {
        return unsupported("INSERT SET with a column list or a source query");
    }
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return unsupported("INSERT target");
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("schema-qualified INSERT target");
    };
    let mut columns = Vec::with_capacity(insert.assignments.len());
    let mut values = Vec::with_capacity(insert.assignments.len());
    for assignment in &insert.assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
            return unsupported("INSERT SET assignment target");
        };
        let [ObjectNamePart::Identifier(column)] = column.0.as_slice() else {
            return unsupported("qualified INSERT SET assignment target");
        };
        columns.push(mysql_quoted(&column.value));
        values.push(written_again(&assignment.value, mode)?);
    }
    // REPLACE and IGNORE both take the SET form, and mean there what they mean
    // on the other one.
    let verb = match (insert.replace_into, insert.ignore) {
        (true, _) => "REPLACE INTO",
        (false, true) => "INSERT IGNORE INTO",
        (false, false) => "INSERT INTO",
    };
    // An upsert clause says what happens to a row that collides, which is the
    // same whichever way the row itself was written, so it comes along as it
    // stands and is held to the rules the other form holds it to.
    let upsert = match &insert.on {
        Some(on) => written_again(on, mode)?,
        None => String::new(),
    };
    Ok(Some(format!(
        "{verb} {} ({}) VALUES ({}){upsert}",
        mysql_quoted(&table.value),
        columns.join(", "),
        values.join(", ")
    )))
}

/// Writes a part of the statement out again. sqlparser writes a word back
/// with its quotes doubled but its backslashes as they stand, so where a
/// backslash escapes, a word holding one would read back as another word —
/// `'a\\\'b'` comes back `'a\''b'` — and is refused.
fn written_again(
    part: &impl std::fmt::Display,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let written = part.to_string();
    if !mode.no_backslash_escapes && written.contains('\\') {
        return unsupported("INSERT SET with a backslash in a word");
    }
    Ok(written)
}

fn mysql_quoted(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_insert_select_needs_plain_stored_columns() {
        let mode = SessionSqlMode::default();
        assert_eq!(
            direct_insert_select_projection("INSERT INTO dst (v) SELECT v FROM src", mode),
            Some(MySqlDirectInsertSelectProjection::Columns(vec!["v".into()]))
        );
        assert_eq!(
            direct_insert_select_projection("INSERT INTO dst (id, v) SELECT * FROM src", mode),
            Some(MySqlDirectInsertSelectProjection::All)
        );
        assert_eq!(
            direct_insert_select_projection("INSERT INTO dst (id) SELECT 1 FROM src", mode),
            Some(MySqlDirectInsertSelectProjection::ColumnsAndLiterals(vec![
                None
            ]))
        );
        assert_eq!(
            filtered_insert_select_projection(
                "INSERT INTO dst (count) SELECT n AS count FROM src WHERE n > 1",
                mode,
            ),
            Some(MySqlDirectInsertSelectProjection::Columns(vec!["n".into()]))
        );
        assert_eq!(
            filtered_insert_select_projection(
                "INSERT INTO dst (s, d) SELECT id + 1 AS s, n * 2 AS d FROM src",
                mode,
            ),
            Some(MySqlDirectInsertSelectProjection::IntegerArithmetic(vec![
                vec!["id".into()],
                vec!["n".into()],
            ]))
        );
        for sql in [
            "INSERT INTO dst (v) SELECT v / 2 FROM src",
            "INSERT INTO dst (v) SELECT 1.2 FROM src",
            "INSERT INTO dst (v) SELECT v FROM src WHERE v = 1.2",
            "INSERT INTO dst (v) SELECT v FROM src JOIN other ON src.id = other.id",
            "INSERT INTO dst (v) SELECT v FROM src ORDER BY v",
        ] {
            assert_eq!(direct_insert_select_projection(sql, mode), None, "{sql}");
        }
    }

    #[test]
    fn an_insert_select_names_its_table_and_columns() {
        let mode = SessionSqlMode::default();
        let copy = parse_optional_insert_select(
            "insert into `users` (`name`, `Email`) select `name`, `email` from `users` where `id` = 1",
            mode,
        )
        .unwrap()
        .unwrap();
        assert_eq!(copy.table().as_str(), "users");
        assert_eq!(copy.columns(), ["name", "Email"]);
        for sql in [
            "INSERT INTO t (a) VALUES (1)",
            "INSERT INTO t SELECT a FROM src",
            "INSERT INTO t SET a = 1",
            "UPDATE t SET a = 1",
        ] {
            assert_eq!(
                parse_optional_insert_select(sql, mode).unwrap(),
                None,
                "{sql}"
            );
        }
        for sql in [
            "INSERT IGNORE INTO t (a) SELECT a FROM src",
            "REPLACE INTO t (a) SELECT a FROM src",
            "INSERT INTO t (a) SELECT a FROM src ON DUPLICATE KEY UPDATE a = 1",
            "INSERT INTO db.t (a) SELECT a FROM src",
        ] {
            assert!(parse_optional_insert_select(sql, mode).is_err(), "{sql}");
        }
    }

    #[test]
    fn insert_set_is_written_out_as_the_column_list_form() {
        for (sql, rewritten) in [
            (
                "INSERT INTO ai SET v = 1, s = 'a'",
                "INSERT INTO `ai` (`v`, `s`) VALUES (1, 'a')",
            ),
            (
                "REPLACE INTO `Ai` SET v = 1",
                "REPLACE INTO `Ai` (`v`) VALUES (1)",
            ),
            (
                "INSERT IGNORE INTO ai SET v = -1",
                "INSERT IGNORE INTO `ai` (`v`) VALUES (-1)",
            ),
        ] {
            assert_eq!(
                parse_optional_insert_set_as_values(sql, SessionSqlMode::default()).unwrap(),
                Some(rewritten.to_owned()),
                "{sql}"
            );
        }
        for sql in [
            "INSERT INTO ai (v) VALUES (1)",
            "INSERT INTO ai SELECT v FROM src",
            "UPDATE ai SET v = 1",
            "SELECT 1",
        ] {
            assert_eq!(
                parse_optional_insert_set_as_values(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
        assert_eq!(
            parse_optional_insert_set_as_values(
                "INSERT INTO k SET id = 1, v = 20 ON DUPLICATE KEY UPDATE n = 999",
                SessionSqlMode::default()
            )
            .unwrap(),
            Some(
                "INSERT INTO `k` (`id`, `v`) VALUES (1, 20) ON DUPLICATE KEY UPDATE n = 999"
                    .to_owned()
            )
        );
        for sql in ["INSERT INTO db.ai SET v = 1", "INSERT INTO ai SET ai.v = 1"] {
            assert!(
                parse_optional_insert_set_as_values(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn insert_select_without_columns_reads_its_table_and_select() {
        let checked = parse_optional_insert_select_without_columns(
            "INSERT INTO `Dst` SELECT * FROM src WHERE id > 1",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(checked.table().as_str(), "dst");
        assert!(!checked.replaces());
        assert_eq!(checked.select_sql(), "SELECT * FROM src WHERE id > 1");

        let replaced = parse_optional_insert_select_without_columns(
            "REPLACE INTO dst SELECT id FROM src",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert!(replaced.replaces());
        assert!(!replaced.ignores());

        let ignored = parse_optional_insert_select_without_columns(
            "INSERT IGNORE INTO dst SELECT id FROM src",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert!(ignored.ignores());
        assert!(!ignored.replaces());
        assert_eq!(ignored.select_sql(), "SELECT id FROM src");
    }

    #[test]
    fn insert_select_without_columns_leaves_every_other_insert_alone() {
        for sql in [
            "INSERT INTO dst (id) SELECT id FROM src",
            "INSERT INTO dst (id) VALUES (1)",
            "INSERT INTO dst VALUES (1)",
            "SELECT 1",
            "UPDATE dst SET id = 1",
        ] {
            assert_eq!(
                parse_optional_insert_select_without_columns(sql, SessionSqlMode::default())
                    .unwrap(),
                None,
                "{sql}"
            );
        }
    }

    #[test]
    fn insert_select_without_columns_refuses_the_forms_it_cannot_write_out() {
        for sql in [
            "INSERT INTO dst SELECT id FROM src ON DUPLICATE KEY UPDATE id = 1",
            "INSERT INTO db.dst SELECT id FROM src",
        ] {
            assert!(
                parse_optional_insert_select_without_columns(sql, SessionSqlMode::default())
                    .is_err(),
                "{sql}"
            );
        }
    }
}
