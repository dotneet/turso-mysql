//! The `SELECT`s a session answers on its own, without reading a table.
//!
//! Each keeps the spelling the client sent, because MySQL names the result
//! column after the expression as written.

use super::{ParseError, SessionSqlMode};

/// A checked `SELECT DATABASE()` that the session answers on its own.
///
/// MySQL answers this without a selected database, which is how a client's
/// `USE` reaches the server at all: `com_use` asks `SELECT DATABASE()` first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlSelectDatabaseQuery {
    column_name: String,
}

impl MySqlSelectDatabaseQuery {
    /// Returns the name MySQL gives the one result column.
    ///
    /// Without an alias this is the call as the client wrote it, spacing and
    /// case included: MySQL 8.4.11 names the column `DATABASE()` for
    /// `SELECT DATABASE()` and `database ()` for `SELECT database ()`.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }
}

/// Parses `SELECT DATABASE()` when the statement is exactly that one query.
///
/// Any other statement returns `None` so that its own parser can handle it.
/// `SCHEMA()` is MySQL's synonym. A trailing alias renames the column, and
/// comments and extra semicolons are taken where MySQL takes them.
pub fn parse_optional_select_database(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlSelectDatabaseQuery>, ParseError> {
    let mut scanner = Scanner::new(sql, mode);
    scanner.skip_gaps();
    if !scanner.take_keyword("SELECT") {
        return Ok(None);
    }
    scanner.skip_gaps();
    let start = scanner.cursor;
    if !scanner.take_keyword("DATABASE") && !scanner.take_keyword("SCHEMA") {
        return Ok(None);
    }
    scanner.skip_gaps();
    if !scanner.take_byte(b'(') {
        return Ok(None);
    }
    scanner.skip_gaps();
    if !scanner.take_byte(b')') {
        return Err(ParseError::ExpectedAdminCommand);
    }
    let call = sql[start..scanner.cursor].to_owned();

    scanner.skip_gaps();
    let alias = scanner.take_alias()?;
    if !scanner.at_end() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlSelectDatabaseQuery {
        column_name: alias.unwrap_or(call),
    }))
}

/// A checked `SELECT` of one system variable, which the session answers from
/// what it knows about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlSystemVariableQuery {
    name: String,
    column_name: String,
}

impl MySqlSystemVariableQuery {
    /// Returns the variable named, without the `@@` or a scope prefix.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the name MySQL gives the one result column.
    ///
    /// Without an alias this is the expression as the client wrote it, which
    /// for `SELECT @@version` is `@@version`, measured on MySQL 8.4.11.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }
}

/// Parses `SELECT @@name` and `SELECT VERSION()` when that is the whole
/// statement.
///
/// The `mysql` client opens with `select @@version_comment limit 1`, so the
/// `LIMIT` MySQL takes here is read and dropped: this answers one row, and a
/// limit can only keep or discard it. A limit of zero is refused rather than
/// answered with a row it asked not to have.
pub fn parse_optional_system_variable_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlSystemVariableQuery>, ParseError> {
    let mut scanner = Scanner::new(sql, mode);
    scanner.skip_gaps();
    if !scanner.take_keyword("SELECT") {
        return Ok(None);
    }
    scanner.skip_gaps();
    let start = scanner.cursor;
    let name = if scanner.take_keyword("VERSION") {
        scanner.skip_gaps();
        if !scanner.take_byte(b'(') {
            return Ok(None);
        }
        scanner.skip_gaps();
        if !scanner.take_byte(b')') {
            return Ok(None);
        }
        "version".to_owned()
    } else {
        if !scanner.take_byte(b'@') || !scanner.take_byte(b'@') {
            return Ok(None);
        }
        for scope in ["SESSION.", "LOCAL.", "GLOBAL."] {
            if scanner.take_keyword(&scope[..scope.len() - 1]) {
                if !scanner.take_byte(b'.') {
                    return Ok(None);
                }
                break;
            }
        }
        let Some(name) = scanner.take_word() else {
            return Ok(None);
        };
        name
    };
    let expression = sql[start..scanner.cursor].to_owned();

    scanner.skip_gaps();
    // `LIMIT` is a keyword here, not the bare alias it would otherwise look
    // like, so it has to be recognized before an alias is read.
    let alias = if scanner.at_keyword("LIMIT") {
        None
    } else {
        scanner.take_alias()?
    };
    scanner.skip_gaps();
    if scanner.take_keyword("LIMIT") {
        scanner.skip_gaps();
        let Some(limit) = scanner.take_word() else {
            return Ok(None);
        };
        // A limit of zero asks for no row, which this cannot answer with one.
        if !matches!(limit.parse::<u64>(), Ok(limit) if limit > 0) {
            return Ok(None);
        }
    }
    // Anything left over means another parser owns the statement — the driver
    // bootstrap query reads two variables at once, for one.
    if !scanner.at_end() {
        return Ok(None);
    }
    Ok(Some(MySqlSystemVariableQuery {
        name,
        column_name: alias.unwrap_or(expression),
    }))
}

/// Reads the statement from its own bytes, so that the column name keeps the
/// spelling the client sent.
struct Scanner<'a> {
    bytes: &'a [u8],
    sql: &'a str,
    cursor: usize,
    mode: SessionSqlMode,
}

impl<'a> Scanner<'a> {
    fn new(sql: &'a str, mode: SessionSqlMode) -> Self {
        Self {
            bytes: sql.as_bytes(),
            sql,
            cursor: 0,
            mode,
        }
    }

    /// Skips whitespace and comments, which MySQL allows between any two parts.
    fn skip_gaps(&mut self) {
        loop {
            match self.bytes.get(self.cursor) {
                Some(byte) if byte.is_ascii_whitespace() => self.cursor += 1,
                Some(b'#') => self.skip_to_line_end(1),
                Some(b'-') if self.bytes.get(self.cursor + 1) == Some(&b'-') => {
                    self.skip_to_line_end(2)
                }
                Some(b'/') if self.bytes.get(self.cursor + 1) == Some(&b'*') => {
                    self.cursor = self.bytes[self.cursor + 2..]
                        .windows(2)
                        .position(|window| window == b"*/")
                        .map_or(self.bytes.len(), |offset| self.cursor + 2 + offset + 2);
                }
                _ => return,
            }
        }
    }

    fn skip_to_line_end(&mut self, opener: usize) {
        self.cursor = self.bytes[self.cursor + opener..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(self.bytes.len(), |offset| self.cursor + opener + offset);
    }

    fn take_keyword(&mut self, keyword: &str) -> bool {
        let end = self.word_end();
        if !self.sql[self.cursor..end].eq_ignore_ascii_case(keyword) {
            return false;
        }
        self.cursor = end;
        true
    }

    fn word_end(&self) -> usize {
        let mut end = self.cursor;
        while self
            .bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$')
        {
            end += 1;
        }
        end
    }

    fn take_byte(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.cursor) != Some(&expected) {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Reads `AS name`, a bare `name`, or nothing.
    fn take_alias(&mut self) -> Result<Option<String>, ParseError> {
        if self.take_keyword("AS") {
            self.skip_gaps();
            return self
                .take_alias_name()
                .map(Some)
                .ok_or(ParseError::ExpectedAdminCommand);
        }
        Ok(self.take_alias_name())
    }

    /// Reports whether the next word is a keyword, without consuming it.
    fn at_keyword(&mut self, keyword: &str) -> bool {
        let cursor = self.cursor;
        let found = self.take_keyword(keyword);
        self.cursor = cursor;
        found
    }

    /// Reads a bare word: a variable name, or the digits of a `LIMIT`.
    fn take_word(&mut self) -> Option<String> {
        let start = self.cursor;
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.cursor += 1;
        }
        (self.cursor > start).then(|| self.sql[start..self.cursor].to_owned())
    }

    fn take_alias_name(&mut self) -> Option<String> {
        if let Some(quote) = self.opening_quote() {
            let mut name = String::new();
            let mut cursor = self.cursor + 1;
            while let Some(byte) = self.bytes.get(cursor) {
                if *byte == quote {
                    if self.bytes.get(cursor + 1) == Some(&quote) {
                        name.push(char::from(quote));
                        cursor += 2;
                        continue;
                    }
                    self.cursor = cursor + 1;
                    return Some(name);
                }
                let end = next_character_end(self.sql, cursor);
                name.push_str(&self.sql[cursor..end]);
                cursor = end;
            }
            return None;
        }
        let end = self.word_end();
        if end == self.cursor {
            return None;
        }
        let name = self.sql[self.cursor..end].to_owned();
        self.cursor = end;
        Some(name)
    }

    fn opening_quote(&self) -> Option<u8> {
        match self.bytes.get(self.cursor) {
            Some(b'`') => Some(b'`'),
            Some(b'"') if self.mode.ansi_quotes => Some(b'"'),
            _ => None,
        }
    }

    /// Reports whether only comments and semicolons are left.
    fn at_end(&mut self) -> bool {
        loop {
            self.skip_gaps();
            match self.bytes.get(self.cursor) {
                None => return true,
                Some(b';') => self.cursor += 1,
                Some(_) => return false,
            }
        }
    }
}

fn next_character_end(sql: &str, cursor: usize) -> usize {
    let mut end = cursor + 1;
    while !sql.is_char_boundary(end) {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_system_variable_a_client_asks_for_at_startup() {
        let read = |sql: &str| {
            parse_optional_system_variable_query(sql, SessionSqlMode::default()).unwrap()
        };
        // The `mysql` client opens with exactly this, LIMIT and all.
        let query = read("select @@version_comment limit 1").unwrap();
        assert_eq!(query.name(), "version_comment");
        assert_eq!(query.column_name(), "@@version_comment");

        for (sql, name, column) in [
            ("SELECT @@version", "version", "@@version"),
            ("SELECT @@SESSION.version", "version", "@@SESSION.version"),
            ("SELECT @@global.version", "version", "@@global.version"),
            ("SELECT VERSION()", "version", "VERSION()"),
            ("SELECT version ()", "version", "version ()"),
            ("SELECT @@version AS v", "version", "v"),
        ] {
            let query = read(sql).unwrap();
            assert_eq!((query.name(), query.column_name()), (name, column), "{sql}");
        }

        // Everything else belongs to its own parser, including the driver
        // bootstrap query, which reads two variables at once, and a LIMIT of
        // zero, which asks for no row.
        for sql in [
            "SELECT 1",
            "SELECT DATABASE()",
            "SELECT id FROM users",
            "SELECT @@max_allowed_packet,@@wait_timeout",
            "SELECT @@version LIMIT 0",
            "",
        ] {
            assert_eq!(read(sql), None, "{sql}");
        }
    }

    use super::*;

    fn column_name(sql: &str) -> Option<String> {
        parse_optional_select_database(sql, SessionSqlMode::default())
            .unwrap()
            .map(|query| query.column_name().to_owned())
    }

    #[test]
    fn names_the_column_the_way_mysql_8_4_names_it() {
        // Measured on MySQL 8.4.11: the column carries the call as written.
        assert_eq!(
            column_name("SELECT DATABASE()").as_deref(),
            Some("DATABASE()")
        );
        assert_eq!(
            column_name("select database()").as_deref(),
            Some("database()")
        );
        assert_eq!(column_name("SELECT SCHEMA()").as_deref(), Some("SCHEMA()"));
        assert_eq!(
            column_name("SELECT database ()").as_deref(),
            Some("database ()")
        );
        assert_eq!(column_name("SELECT DATABASE() AS x").as_deref(), Some("x"));
        assert_eq!(column_name("SELECT DATABASE() x").as_deref(), Some("x"));
        assert_eq!(
            column_name("SELECT DATABASE() AS `my db`").as_deref(),
            Some("my db")
        );
        assert_eq!(
            column_name("/* c */ SELECT DATABASE();;").as_deref(),
            Some("DATABASE()")
        );
        assert_eq!(
            column_name("SELECT DATABASE() -- x").as_deref(),
            Some("DATABASE()")
        );
    }

    #[test]
    fn leaves_every_other_statement_to_its_own_parser() {
        for sql in [
            "SELECT 1",
            "SELECT DATABASES()",
            "SHOW DATABASES",
            "SELECT DATABASE",
            "SELECT SCHEMATA()",
            "",
        ] {
            assert_eq!(column_name(sql), None, "{sql}");
        }
    }

    #[test]
    fn refuses_the_shapes_it_cannot_answer() {
        for sql in [
            // MySQL answers these; this query surface takes one column only.
            "SELECT DATABASE(), 1",
            "SELECT DATABASE() FROM t",
            "SELECT DATABASE() AS",
            "SELECT DATABASE(x)",
        ] {
            assert!(
                parse_optional_select_database(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
