//! `CREATE TABLE <new> LIKE <old>`, which makes an empty table shaped like
//! another one in the same database.

use crate::statement_reads;
use sqlparser::tokenizer::Token;

use super::{MySqlTableName, ParseError, SessionMySqlDialect, SessionSqlMode};

/// One `CREATE TABLE [IF NOT EXISTS] <new> LIKE <old>`, also written with the
/// `LIKE` in parentheses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MySqlCreateTableLike {
    table: MySqlTableName,
    source: MySqlTableName,
    only_if_missing: bool,
}

impl MySqlCreateTableLike {
    /// The table the statement makes.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// The table whose shape the new one takes.
    pub fn source(&self) -> &MySqlTableName {
        &self.source
    }

    /// Whether the statement says `IF NOT EXISTS`.
    pub fn only_if_missing(&self) -> bool {
        self.only_if_missing
    }
}

/// Reads a `CREATE TABLE ... LIKE ...`, answering `None` for any other
/// statement.
///
/// A temporary table, and either name written with its database, are refused:
/// neither has been measured.
pub fn parse_optional_create_table_like(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlCreateTableLike>, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let Ok(tokens) = statement_reads::tokens(&dialect, sql) else {
        return Ok(None);
    };
    let mut words = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_) | Token::SemiColon | Token::EOF))
        .cloned()
        .peekable();
    let keyword = |token: Option<&Token>, expected: &str| {
        matches!(token, Some(Token::Word(word))
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
    };
    if !keyword(words.next().as_ref(), "CREATE") {
        return Ok(None);
    }
    let temporary = keyword(words.peek(), "TEMPORARY");
    if temporary {
        words.next();
    }
    if !keyword(words.next().as_ref(), "TABLE") {
        return Ok(None);
    }
    let only_if_missing = keyword(words.peek(), "IF");
    if only_if_missing
        && (!keyword(words.next().as_ref(), "IF")
            || !keyword(words.next().as_ref(), "NOT")
            || !keyword(words.next().as_ref(), "EXISTS"))
    {
        return Ok(None);
    }
    let Some(Token::Word(table)) = words.next() else {
        return Ok(None);
    };
    let in_parentheses = matches!(words.peek(), Some(Token::LParen));
    if in_parentheses {
        words.next();
    }
    if !keyword(words.peek(), "LIKE") {
        return Ok(None);
    }
    words.next();
    let refused = |feature| Err(ParseError::Unsupported { feature });
    if temporary {
        return refused("CREATE TEMPORARY TABLE LIKE");
    }
    let Some(Token::Word(source)) = words.next() else {
        return refused("CREATE TABLE LIKE source");
    };
    if in_parentheses && !matches!(words.next(), Some(Token::RParen)) {
        return refused("CREATE TABLE LIKE source");
    }
    if words.next().is_some() {
        return refused("CREATE TABLE LIKE source");
    }
    let name = |word: &str| {
        MySqlTableName::parse(word).map_err(|_| ParseError::Unsupported {
            feature: "CREATE TABLE LIKE table name",
        })
    };
    Ok(Some(MySqlCreateTableLike {
        table: name(&table.value)?,
        source: name(&source.value)?,
        only_if_missing,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(sql: &str) -> Result<Option<MySqlCreateTableLike>, ParseError> {
        parse_optional_create_table_like(sql, SessionSqlMode::default())
    }

    #[test]
    fn both_spellings_name_the_new_table_and_its_source() {
        for sql in [
            "CREATE TABLE copy LIKE users",
            "create table `copy` (like `users`);",
        ] {
            let like = read(sql).unwrap().unwrap();
            assert_eq!(like.table().as_str(), "copy", "{sql}");
            assert_eq!(like.source().as_str(), "users", "{sql}");
            assert!(!like.only_if_missing(), "{sql}");
        }
        assert!(read("CREATE TABLE IF NOT EXISTS copy LIKE users")
            .unwrap()
            .unwrap()
            .only_if_missing());
    }

    #[test]
    fn other_create_statements_are_not_read() {
        for sql in [
            "CREATE TABLE copy (id INT)",
            "CREATE TABLE copy AS SELECT * FROM users",
            "CREATE VIEW copy AS SELECT id FROM users",
            "SELECT * FROM users WHERE email LIKE 'a%'",
        ] {
            assert_eq!(read(sql).unwrap(), None, "{sql}");
        }
    }

    #[test]
    fn unmeasured_forms_are_refused() {
        for sql in [
            "CREATE TEMPORARY TABLE copy LIKE users",
            "CREATE TABLE copy LIKE reports.users",
            "CREATE TABLE copy (LIKE users",
            "CREATE TABLE copy LIKE users extra",
        ] {
            assert!(read(sql).is_err(), "{sql}");
        }
    }
}
