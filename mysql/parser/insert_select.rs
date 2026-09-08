use super::{
    parse_one_statement, unsupported, MySqlTableName, ParseError, SessionMySqlDialect,
    SessionSqlMode,
};
use sqlparser::ast::{ObjectNamePart, SetExpr, Statement};
use sqlparser::tokenizer::{Location, Token, Tokenizer};

/// One `INSERT INTO t <SELECT>` written with no column list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInsertSelectWithoutColumns {
    table: MySqlTableName,
    replaces: bool,
    select_sql: String,
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
/// path. A statement carrying `IGNORE` or an upsert clause is refused, as those
/// forms are wherever they are written.
pub fn parse_optional_insert_select_without_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInsertSelectWithoutColumns>, ParseError> {
    let Ok(Statement::Insert(insert)) = parse_one_statement(sql, mode) else {
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
    if insert.ignore || insert.on.is_some() || insert.partitioned.is_some() {
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
        select_sql: source.to_string(),
    }))
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
    let Ok(Statement::Insert(insert)) = parse_one_statement(sql, mode) else {
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
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize_with_location()
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
    let Ok(Statement::Insert(insert)) = parse_one_statement(sql, mode) else {
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
        values.push(assignment.value.to_string());
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
    let upsert = insert
        .on
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    Ok(Some(format!(
        "{verb} {} ({}) VALUES ({}){upsert}",
        mysql_quoted(&table.value),
        columns.join(", "),
        values.join(", ")
    )))
}

fn mysql_quoted(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "INSERT IGNORE INTO dst SELECT id FROM src",
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
