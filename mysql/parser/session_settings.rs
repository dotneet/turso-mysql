use super::{ParseError, SessionSqlMode};

/// One session setting this server can accept without changing how it behaves.
///
/// Every real client opens with a handful of these. Accepting one is only
/// honest when the state it asks for is the state this server is already in,
/// so each variant carries what was asked for and the caller checks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlSessionSetting {
    /// `SET sql_mode = '...'`, with the modes it named, in order and as
    /// written. Which of them this server can honestly accept is the server's
    /// question, not the parser's.
    SqlMode(Vec<String>),
    /// `SET time_zone = '...'`, with the zone as written.
    TimeZone(String),
    /// `SET information_schema_stats_expiry = <n>`.
    InformationSchemaStatsExpiry(u64),
    /// `SET innodb_lock_wait_timeout = <n>`, in seconds.
    ///
    /// This one changes how the server behaves rather than restating what it
    /// already does: it is how long a session waits for a lock another session
    /// holds before giving up.
    LockWaitTimeout(u64),
    /// `SET NAMES <charset> [COLLATE <collation>]`, with what it named.
    Names {
        character_set: String,
        collation: Option<String>,
    },
    /// `SET [SESSION] TRANSACTION ISOLATION LEVEL <level>`, with the level as
    /// written and its words joined by one space.
    ///
    /// The `GLOBAL` scope is not one of these: it changes what other sessions
    /// get, which is not a thing this server can honestly accept.
    TransactionIsolationLevel(String),
}

/// Parses one supported `SET` of a session variable.
///
/// Returns `None` for anything that is not one of these, so the statement's own
/// parser keeps it. A versioned comment around the statement, which is how
/// `mysqldump` writes every one of them, is read the way MySQL reads it.
pub fn parse_optional_session_setting(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlSessionSetting>, ParseError> {
    let Some(body) = statement_body(sql) else {
        return Ok(None);
    };
    let mut scanner = Scanner::new(body);
    if !scanner.take_keyword("SET") {
        return Ok(None);
    }
    // `SESSION` and `LOCAL` both name the session, which is also the default.
    let _ = scanner.take_keyword("SESSION") || scanner.take_keyword("LOCAL");
    // `SET TRANSACTION ISOLATION LEVEL <level>` is its own statement too. With
    // no scope word it names the next transaction rather than the session,
    // which makes no difference to a server that answers only the level it is
    // already in.
    if scanner.take_keyword("TRANSACTION") {
        if !scanner.take_keyword("ISOLATION") || !scanner.take_keyword("LEVEL") {
            return Ok(None);
        }
        let Some(level) = scanner.take_isolation_level() else {
            return Ok(None);
        };
        if !scanner.at_end() {
            return Err(ParseError::TrailingAdminCommandTokens);
        }
        return Ok(Some(MySqlSessionSetting::TransactionIsolationLevel(level)));
    }
    // `SET NAMES` is its own statement, not an assignment.
    if scanner.take_keyword("NAMES") {
        let Some(character_set) = scanner.take_charset_name(mode) else {
            return Ok(None);
        };
        let collation = if scanner.take_keyword("COLLATE") {
            let Some(collation) = scanner.take_charset_name(mode) else {
                return Ok(None);
            };
            Some(collation)
        } else {
            None
        };
        if !scanner.at_end() {
            return Err(ParseError::TrailingAdminCommandTokens);
        }
        return Ok(Some(MySqlSessionSetting::Names {
            character_set,
            collation,
        }));
    }
    let Some(name) = scanner.take_variable_name() else {
        return Ok(None);
    };
    if !scanner.take_byte(b'=') {
        return Ok(None);
    }
    let setting = if name.eq_ignore_ascii_case("sql_mode") {
        let Some(value) = scanner.take_string(mode) else {
            return Ok(None);
        };
        MySqlSessionSetting::SqlMode(named_sql_modes(&value))
    } else if name.eq_ignore_ascii_case("time_zone") {
        let Some(value) = scanner.take_string(mode) else {
            return Ok(None);
        };
        MySqlSessionSetting::TimeZone(value)
    } else if name.eq_ignore_ascii_case("information_schema_stats_expiry") {
        let Some(value) = scanner.take_unsigned() else {
            return Ok(None);
        };
        MySqlSessionSetting::InformationSchemaStatsExpiry(value)
    } else if name.eq_ignore_ascii_case("innodb_lock_wait_timeout") {
        let Some(value) = scanner.take_unsigned() else {
            return Ok(None);
        };
        MySqlSessionSetting::LockWaitTimeout(value)
    } else {
        return Ok(None);
    };
    if !scanner.at_end() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(setting))
}

/// One `SET @name = value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlUserVariableAssignment {
    name: String,
    value: MySqlUserVariableValue,
}

impl MySqlUserVariableAssignment {
    /// Returns the variable named, lowercased and without the `@`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns what the variable is set to.
    pub fn value(&self) -> &MySqlUserVariableValue {
        &self.value
    }
}

/// What a user variable holds.
///
/// The kind decides the result column `SELECT @name` answers, and the three
/// kinds answer three different columns, so what was stored has to be kept
/// apart rather than flattened into text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlUserVariableValue {
    Null,
    Integer(i64),
    /// A decimal literal, kept as it was written.
    Decimal(String),
    Text(String),
}

/// Parses `SET @name = value[, @name = value...]`.
///
/// Returns `None` for anything that is not one, so the other `SET` readers keep
/// their own statements. Only a literal is taken: an expression such as
/// `SET @y := @x + 1` is refused rather than half-answered.
pub fn parse_optional_user_variable_assignment(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<Vec<MySqlUserVariableAssignment>>, ParseError> {
    let Some(body) = statement_body(sql) else {
        return Ok(None);
    };
    let mut scanner = Scanner::new(body);
    if !scanner.take_keyword("SET") {
        return Ok(None);
    }
    let mut assignments = Vec::new();
    loop {
        scanner.skip_spaces();
        if !scanner.take_byte(b'@') || scanner.at_byte(b'@') {
            return Ok(None);
        }
        let Some(name) = scanner.take_user_variable_name() else {
            return Ok(None);
        };
        // MySQL takes both spellings here; `:=` is the only one that also works
        // inside an expression.
        scanner.skip_spaces();
        let _ = scanner.take_byte(b':');
        if !scanner.take_byte(b'=') {
            return Ok(None);
        }
        let Some(value) = scanner.take_user_variable_value(mode) else {
            return Err(ParseError::Unsupported {
                feature: "SET of a user variable to something other than a literal",
            });
        };
        assignments.push(MySqlUserVariableAssignment {
            name: name.to_ascii_lowercase(),
            value,
        });
        scanner.skip_spaces();
        if !scanner.take_byte(b',') {
            break;
        }
    }
    if !scanner.at_end() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(assignments))
}

/// Splits a `sql_mode` value into the modes it names.
///
/// MySQL takes an empty value as naming none, and ignores the spaces around a
/// comma.
fn named_sql_modes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Unwraps the versioned comment `mysqldump` writes each of these in.
///
/// `/*!40100 SET @@SQL_MODE='' */` runs on any server past the named version,
/// which every version this speaks for is. Text outside such a comment is
/// returned as it stands.
fn statement_body(sql: &str) -> Option<&str> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let Some(rest) = trimmed.strip_prefix("/*!") else {
        return Some(trimmed);
    };
    let inner = rest.strip_suffix("*/")?;
    let digits = inner.bytes().take_while(u8::is_ascii_digit).count();
    Some(inner[digits..].trim())
}

/// Reads one `SET` from its own bytes, which keeps the value's spelling.
struct Scanner<'a> {
    bytes: &'a [u8],
    sql: &'a str,
    cursor: usize,
}

impl<'a> Scanner<'a> {
    fn new(sql: &'a str) -> Self {
        Self {
            bytes: sql.as_bytes(),
            sql,
            cursor: 0,
        }
    }

    fn skip_spaces(&mut self) {
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.cursor += 1;
        }
    }

    fn take_keyword(&mut self, keyword: &str) -> bool {
        self.skip_spaces();
        let end = self.word_end(self.cursor);
        if !self.sql[self.cursor..end].eq_ignore_ascii_case(keyword) {
            return false;
        }
        self.cursor = end;
        true
    }

    /// Reads one of MySQL's four isolation level names.
    ///
    /// Each is one or two words, so the words are read and joined rather than
    /// matched against the text, which lets any spacing through.
    fn take_isolation_level(&mut self) -> Option<String> {
        for (first, second) in [
            ("REPEATABLE", Some("READ")),
            ("SERIALIZABLE", None),
            ("READ", Some("COMMITTED")),
            ("READ", Some("UNCOMMITTED")),
        ] {
            let restore = self.cursor;
            if !self.take_keyword(first) {
                continue;
            }
            let Some(second) = second else {
                return Some(first.to_owned());
            };
            if self.take_keyword(second) {
                return Some(format!("{first} {second}"));
            }
            self.cursor = restore;
        }
        None
    }

    /// Reads a variable name, with or without the `@@` and a scope prefix.
    fn take_variable_name(&mut self) -> Option<String> {
        self.skip_spaces();
        let mut cursor = self.cursor;
        if self.sql[cursor..].starts_with("@@") {
            cursor += 2;
            for scope in ["SESSION.", "LOCAL.", "GLOBAL."] {
                if self.sql[cursor..].len() >= scope.len()
                    && self.sql[cursor..cursor + scope.len()].eq_ignore_ascii_case(scope)
                {
                    cursor += scope.len();
                    break;
                }
            }
        }
        let end = self.word_end(cursor);
        if end == cursor {
            return None;
        }
        let name = self.sql[cursor..end].to_owned();
        self.cursor = end;
        Some(name)
    }

    fn word_end(&self, from: usize) -> usize {
        let mut end = from;
        while self
            .bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            end += 1;
        }
        end
    }

    fn take_byte(&mut self, expected: u8) -> bool {
        self.skip_spaces();
        if self.bytes.get(self.cursor) != Some(&expected) {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn at_byte(&self, expected: u8) -> bool {
        self.bytes.get(self.cursor) == Some(&expected)
    }

    /// Reads the name after a `@`, which takes the same characters a table
    /// name does.
    fn take_user_variable_name(&mut self) -> Option<String> {
        let start = self.cursor;
        while self
            .bytes
            .get(self.cursor)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$')
        {
            self.cursor += 1;
        }
        (self.cursor > start).then(|| self.sql[start..self.cursor].to_owned())
    }

    /// Reads the literal a user variable is set to.
    ///
    /// A number without a point or an exponent is an integer, and one with
    /// either is a decimal — measured on MySQL 8.4.11, `SET @f = 1.5` answers a
    /// NEWDECIMAL where `SET @x = 1` answers a LONGLONG. Anything wider than an
    /// `i64` is left out: the engine holds an integer as one.
    fn take_user_variable_value(&mut self, mode: SessionSqlMode) -> Option<MySqlUserVariableValue> {
        self.skip_spaces();
        if self.take_keyword("NULL") {
            return Some(MySqlUserVariableValue::Null);
        }
        if self.take_keyword("TRUE") {
            return Some(MySqlUserVariableValue::Integer(1));
        }
        if self.take_keyword("FALSE") {
            return Some(MySqlUserVariableValue::Integer(0));
        }
        if matches!(self.bytes.get(self.cursor), Some(b'\'') | Some(b'"')) {
            return self.take_string(mode).map(MySqlUserVariableValue::Text);
        }
        let start = self.cursor;
        let _ = self.take_byte(b'-');
        let mut digits = false;
        let mut decimal = false;
        while let Some(byte) = self.bytes.get(self.cursor) {
            match byte {
                b'0'..=b'9' => digits = true,
                b'.' => decimal = true,
                _ => break,
            }
            self.cursor += 1;
        }
        if !digits {
            self.cursor = start;
            return None;
        }
        let written = &self.sql[start..self.cursor];
        if decimal {
            return Some(MySqlUserVariableValue::Decimal(written.to_owned()));
        }
        written.parse().ok().map(MySqlUserVariableValue::Integer)
    }

    /// Reads a quoted value. `sql_mode` and `time_zone` are always quoted here.
    fn take_string(&mut self, mode: SessionSqlMode) -> Option<String> {
        self.skip_spaces();
        let quote = match self.bytes.get(self.cursor) {
            Some(b'\'') => b'\'',
            Some(b'"') if !mode.ansi_quotes => b'"',
            _ => return None,
        };
        let mut value = String::new();
        let mut cursor = self.cursor + 1;
        while let Some(byte) = self.bytes.get(cursor) {
            if *byte == quote {
                if self.bytes.get(cursor + 1) == Some(&quote) {
                    value.push(char::from(quote));
                    cursor += 2;
                    continue;
                }
                self.cursor = cursor + 1;
                return Some(value);
            }
            let end = next_character_end(self.sql, cursor);
            value.push_str(&self.sql[cursor..end]);
            cursor = end;
        }
        None
    }

    /// Reads a character set or collation, which MySQL takes quoted or bare.
    fn take_charset_name(&mut self, mode: SessionSqlMode) -> Option<String> {
        if let Some(quoted) = self.take_string(mode) {
            return Some(quoted);
        }
        self.skip_spaces();
        let end = self.word_end(self.cursor);
        if end == self.cursor {
            return None;
        }
        let name = self.sql[self.cursor..end].to_owned();
        self.cursor = end;
        Some(name)
    }

    fn take_unsigned(&mut self) -> Option<u64> {
        self.skip_spaces();
        let end = self.word_end(self.cursor);
        let digits = self.sql.get(self.cursor..end)?;
        let value = digits.parse().ok()?;
        self.cursor = end;
        Some(value)
    }

    fn at_end(&mut self) -> bool {
        self.skip_spaces();
        self.cursor >= self.bytes.len()
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
    use super::*;

    fn parse(sql: &str) -> Option<MySqlSessionSetting> {
        parse_optional_session_setting(sql, SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn reads_the_settings_a_real_client_opens_with() {
        // The four statements a real `mysqldump --no-data` sends first, and the
        // versioned comments it wraps three of them in.
        assert_eq!(
            parse("/*!40100 SET @@SQL_MODE='' */"),
            Some(MySqlSessionSetting::SqlMode(Vec::new()))
        );
        assert_eq!(
            parse("/*!40103 SET TIME_ZONE='+00:00' */"),
            Some(MySqlSessionSetting::TimeZone("+00:00".to_owned()))
        );
        assert_eq!(
            parse("/*!80000 SET SESSION information_schema_stats_expiry=0 */"),
            Some(MySqlSessionSetting::InformationSchemaStatsExpiry(0))
        );
    }

    #[test]
    fn reads_the_spellings_mysql_takes_for_the_same_setting() {
        for sql in [
            "SET sql_mode = ''",
            "SET SESSION sql_mode=''",
            "SET LOCAL sql_mode = ''",
            "SET @@sql_mode = ''",
            "SET @@session.sql_mode=''",
            "set @@SESSION.SQL_MODE = '' ;",
        ] {
            assert_eq!(
                parse(sql),
                Some(MySqlSessionSetting::SqlMode(Vec::new())),
                "{sql}"
            );
        }
    }

    #[test]
    fn reads_the_modes_a_sql_mode_value_names() {
        let named = |value: &str| match parse(&format!("SET sql_mode = '{value}'")) {
            Some(MySqlSessionSetting::SqlMode(named)) => named,
            other => panic!("{value}: {other:?}"),
        };
        assert!(named("").is_empty());
        // MySQL 8.4's own default, which is what a client that reads the
        // variable and writes it back sends.
        assert_eq!(
            named("ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE"),
            [
                "ONLY_FULL_GROUP_BY",
                "STRICT_TRANS_TABLES",
                "NO_ZERO_IN_DATE"
            ]
        );
        assert_eq!(named("ANSI_QUOTES"), ["ANSI_QUOTES"]);
        assert_eq!(
            named("STRICT_TRANS_TABLES, NO_BACKSLASH_ESCAPES"),
            ["STRICT_TRANS_TABLES", "NO_BACKSLASH_ESCAPES"]
        );
    }

    #[test]
    fn reads_set_names() {
        assert_eq!(
            parse("SET NAMES utf8mb4"),
            Some(MySqlSessionSetting::Names {
                character_set: "utf8mb4".to_owned(),
                collation: None
            })
        );
        assert_eq!(
            parse("SET NAMES 'utf8mb4' COLLATE 'utf8mb4_general_ci'"),
            Some(MySqlSessionSetting::Names {
                character_set: "utf8mb4".to_owned(),
                collation: Some("utf8mb4_general_ci".to_owned())
            })
        );
        assert_eq!(
            parse("set names latin1"),
            Some(MySqlSessionSetting::Names {
                character_set: "latin1".to_owned(),
                collation: None
            })
        );
    }

    /// Every one of MySQL's four levels is read, with or without a scope word.
    /// Which of them this server can honestly accept is the server's question,
    /// not the parser's, so all four come back here.
    #[test]
    fn reads_set_transaction_isolation_level() {
        for (sql, level) in [
            (
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                "REPEATABLE READ",
            ),
            (
                "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                "REPEATABLE READ",
            ),
            (
                "set local transaction isolation level read committed",
                "READ COMMITTED",
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
                "READ UNCOMMITTED",
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;",
                "SERIALIZABLE",
            ),
        ] {
            assert_eq!(
                parse(sql),
                Some(MySqlSessionSetting::TransactionIsolationLevel(
                    level.to_owned()
                )),
                "{sql}"
            );
        }

        for sql in [
            // GLOBAL changes what other sessions get, which is not one of these.
            "SET GLOBAL TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            // Not a level MySQL has.
            "SET TRANSACTION ISOLATION LEVEL SNAPSHOT",
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE",
            "SET TRANSACTION ISOLATION LEVEL READ",
            // The other SET TRANSACTION forms are not read here.
            "SET TRANSACTION READ ONLY",
            "SET TRANSACTION LEVEL REPEATABLE READ",
        ] {
            assert_eq!(parse(sql), None, "{sql}");
        }
    }

    #[test]
    fn leaves_every_other_statement_to_its_own_parser() {
        for sql in [
            "SELECT 1",
            "SET autocommit = 0",
            "SET sql_notes = 1",
            "SET SESSION NET_READ_TIMEOUT= 86400, SESSION NET_WRITE_TIMEOUT= 86400",
            "SET sql_mode",
            "",
        ] {
            assert_eq!(parse(sql), None, "{sql}");
        }
    }

    /// A user variable belongs to the connection, and it is a literal that is
    /// taken here. Measured on MySQL 8.4.11: names are matched whatever their
    /// case, `:=` is the other spelling of `=`, and one statement can set
    /// several.
    #[test]
    fn reads_a_user_variable_assignment() {
        let read = |sql: &str| {
            parse_optional_user_variable_assignment(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|assignment| (assignment.name().to_owned(), assignment.value().clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            read("SET @X = 1"),
            [("x".to_owned(), MySqlUserVariableValue::Integer(1))]
        );
        assert_eq!(
            read("SET @s := 'abc';"),
            [(
                "s".to_owned(),
                MySqlUserVariableValue::Text("abc".to_owned())
            )]
        );
        assert_eq!(
            read("SET @n = NULL"),
            [("n".to_owned(), MySqlUserVariableValue::Null)]
        );
        assert_eq!(
            read("SET @f = 1.5"),
            [(
                "f".to_owned(),
                MySqlUserVariableValue::Decimal("1.5".to_owned())
            )]
        );
        assert_eq!(
            read("SET @b = TRUE, @neg = -7"),
            [
                ("b".to_owned(), MySqlUserVariableValue::Integer(1)),
                ("neg".to_owned(), MySqlUserVariableValue::Integer(-7)),
            ]
        );

        // The system-variable settings keep their own reader.
        for sql in ["SET @@sql_mode = ''", "SET sql_mode = ''", "SELECT @x"] {
            assert_eq!(
                parse_optional_user_variable_assignment(sql, SessionSqlMode::default()),
                Ok(None),
                "{sql}"
            );
        }

        // An expression is refused rather than half-answered.
        for sql in [
            "SET @y := @x + 1",
            "SET @x = (SELECT 1)",
            "SET @x = 1 extra",
        ] {
            assert!(
                parse_optional_user_variable_assignment(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn refuses_a_setting_with_more_after_it() {
        for sql in [
            "SET sql_mode = '' extra",
            "SET time_zone = '+00:00' , x = 1",
        ] {
            assert!(
                parse_optional_session_setting(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
