use super::{unsupported, ParseError, SessionSqlMode};

/// One session setting the server can validate and apply.
///
/// Every real client opens with a handful of these. Each variant carries
/// what was asked for so the caller can check and apply it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlSessionSetting {
    /// `SET sql_mode = '...'`, with the modes it named, in order and as
    /// written. Which of them this server can honestly accept is the server's
    /// question, not the parser's.
    SqlMode(Vec<String>),
    SqlModeFromUserVariable(String),
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
    /// `SET foreign_key_checks = 0` or `= 1`.
    ///
    /// This one changes how the server behaves too: with it off, a row may
    /// name a parent that is not there. It is the first thing a fixture loader
    /// and a dumped schema each say, both of them writing rows in an order no
    /// foreign key would allow.
    ForeignKeyChecks(bool),
    /// `SET unique_checks = 0` or `= 1`.
    UniqueChecks(bool),
    /// `SET sql_notes = 0` or `= 1`.
    SqlNotes(bool),
    /// `SET @name = value`, written beside the system variables of one `SET`.
    /// `mysqldump` saves every setting it changes that way:
    /// `SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0`.
    UserVariable(MySqlUserVariableAssignment),
    /// `SET <variable> = @name`, which is how a dump puts back what it saved
    /// — `SET TIME_ZONE=@OLD_TIME_ZONE`. `variable` is `time_zone`,
    /// `foreign_key_checks`, `unique_checks` or `sql_notes`. What it becomes
    /// depends on what the user variable holds, which is the server's to read.
    FromUserVariable {
        variable: String,
        user_variable: String,
    },
    /// `SET character_set_results = NULL` disables result conversion.
    CharacterSetResultsNull,
    CharacterSetClient(String),
    CharacterSetResults(String),
    CollationConnection(String),
    /// `SET sql_quote_show_create = 1` keeps the default quoted identifiers.
    SqlQuoteShowCreate(bool),
    /// `SET NAMES <charset> [COLLATE <collation>]`, with what it named.
    Names {
        character_set: String,
        collation: Option<String>,
    },
    /// `SET [SESSION] TRANSACTION ISOLATION LEVEL <level>`, with the level as
    /// written and its words joined by one space, or the same level set
    /// through `transaction_isolation`, where a hyphen joins the words.
    ///
    /// The `GLOBAL` scope is not one of these: it changes what other sessions
    /// get, which is not a thing this server can honestly accept.
    /// `SET sql_mode = CONCAT(@@sql_mode, ',STRICT_ALL_TABLES')` and the
    /// other spellings that work the value out from the current one, which
    /// is how Rails opens every connection.
    SqlModeExpression(SqlModeValue),
    /// `SET wait_timeout = <n>`, or `DEFAULT` for the server's own.
    WaitTimeout(Option<u64>),
    /// `SET sql_auto_is_null = 0` or `= 1`.
    SqlAutoIsNull(bool),
    /// `SET sql_safe_updates = 0` or `= 1`.
    SqlSafeUpdates(bool),
    TransactionIsolationLevel {
        level: String,
        /// Whether the level holds for the next transaction alone. Measured
        /// on MySQL 8.4.11: `SET TRANSACTION` and `SET @@transaction_isolation`
        /// written with no scope word do this, while `SESSION`, `LOCAL` and a
        /// plain `transaction_isolation` set the session's level.
        next_transaction_only: bool,
    },
}

/// The settings a `SET` may give the value of a user variable, beside
/// `sql_mode` and the three character-set names, which have readers of their
/// own.
const SETTINGS_READ_FROM_A_USER_VARIABLE: [&str; 4] = [
    "time_zone",
    "foreign_key_checks",
    "unique_checks",
    "sql_notes",
];

/// Parses one supported `SET` of session variables.
///
/// One `SET` may assign several, separated by commas — Rails opens with
/// `SET NAMES utf8mb4, @@SESSION.sql_mode = ..., @@SESSION.wait_timeout = ...`
/// — and they come back in the order written. Returns `None` for anything
/// that is not one of these, so the statement's own parser keeps it. A
/// versioned comment around the statement, which is how `mysqldump` writes
/// every one of them, is read the way MySQL reads it.
pub fn parse_optional_session_settings(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<Vec<MySqlSessionSetting>>, ParseError> {
    let Some(body) = statement_body(sql) else {
        return Ok(None);
    };
    let mut scanner = Scanner::new(body);
    if !scanner.take_keyword("SET") {
        return Ok(None);
    }
    // `SET TRANSACTION ISOLATION LEVEL <level>` is its own statement, with no
    // other assignment beside it. With no scope word it names the next
    // transaction rather than the session.
    let restore = scanner.cursor;
    let scoped = scanner.take_keyword("SESSION") || scanner.take_keyword("LOCAL");
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
        return Ok(Some(vec![MySqlSessionSetting::TransactionIsolationLevel {
            level,
            next_transaction_only: !scoped,
        }]));
    }
    scanner.cursor = restore;
    // Measured on MySQL 8.4.11: a scope word holds for the assignments after
    // it until another one is written — `SET GLOBAL a = 0, b = 0` sets both
    // globally.
    let mut scope = AssignmentScope::Default;
    let mut settings = Vec::new();
    loop {
        if scanner.take_keyword("GLOBAL")
            || scanner.take_keyword("PERSIST")
            || scanner.take_keyword("PERSIST_ONLY")
        {
            scope = AssignmentScope::Global;
        } else if scanner.take_keyword("SESSION") || scanner.take_keyword("LOCAL") {
            scope = AssignmentScope::Session;
        }
        let Some(setting) = take_one_session_setting(&mut scanner, scope, mode)? else {
            return Ok(None);
        };
        settings.push(setting);
        if !scanner.take_byte(b',') {
            break;
        }
    }
    if !scanner.at_end() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(settings))
}

/// The scope an assignment names, by a word before it or by `@@scope.`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssignmentScope {
    Default,
    Session,
    Global,
}

/// A variable name as an assignment wrote it.
struct WrittenVariable {
    name: String,
    scope: AssignmentScope,
    /// Whether it was written with the two `@@` signs.
    signed: bool,
}

/// A `sql_mode` value worked out from the one in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlModeValue {
    Literal(String),
    /// `@@sql_mode` or `@@SESSION.sql_mode`: the session's value.
    Current,
    /// `DEFAULT` or `@@GLOBAL.sql_mode`: the value a new session starts with.
    Default,
    Concat(Vec<SqlModeValue>),
    Replace(Box<SqlModeValue>, Box<SqlModeValue>, Box<SqlModeValue>),
}

impl SqlModeValue {
    /// Works the value out, given the session's value and a new session's.
    pub fn evaluate(&self, current: &str, default: &str) -> String {
        match self {
            Self::Literal(value) => value.clone(),
            Self::Current => current.to_owned(),
            Self::Default => default.to_owned(),
            Self::Concat(parts) => parts
                .iter()
                .map(|part| part.evaluate(current, default))
                .collect(),
            Self::Replace(text, from, to) => {
                let from = from.evaluate(current, default);
                let text = text.evaluate(current, default);
                if from.is_empty() {
                    return text;
                }
                text.replace(&from, &to.evaluate(current, default))
            }
        }
    }

    /// The modes the value names, in the order written.
    pub fn named_modes(&self, current: &str, default: &str) -> Vec<String> {
        named_sql_modes(&self.evaluate(current, default))
    }
}

/// Reads one assignment of a `SET`, or `NAMES`.
fn take_one_session_setting(
    scanner: &mut Scanner<'_>,
    scope: AssignmentScope,
    mode: SessionSqlMode,
) -> Result<Option<MySqlSessionSetting>, ParseError> {
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
        return Ok(Some(MySqlSessionSetting::Names {
            character_set,
            collation,
        }));
    }
    if let Some(name) = scanner.take_user_variable_reference() {
        // MySQL takes both spellings here; `:=` is the only one that also works
        // inside an expression.
        let _ = scanner.take_byte(b':');
        if !scanner.take_byte(b'=') {
            return Ok(None);
        }
        let Some(source) = scanner.take_user_variable_assignment_source(mode) else {
            return unsupported("SET of a user variable to an unsupported expression");
        };
        return Ok(Some(MySqlSessionSetting::UserVariable(
            MySqlUserVariableAssignment { name, source },
        )));
    }
    let Some(WrittenVariable {
        name,
        scope: written_scope,
        signed,
    }) = scanner.take_written_variable()
    else {
        return Ok(None);
    };
    let scope = match written_scope {
        AssignmentScope::Default => scope,
        written => written,
    };
    // Nothing a client does here can change another session, so a global
    // assignment is refused rather than taken as the session's.
    if scope == AssignmentScope::Global {
        return Err(ParseError::Unsupported {
            feature: "SET of a GLOBAL variable",
        });
    }
    // `SET @@transaction_isolation`, with the signs and no scope, names the
    // next transaction alone; every other spelling names the session.
    let unscoped_system_variable = scope == AssignmentScope::Default && signed;
    let _ = scanner.take_byte(b':');
    if !scanner.take_byte(b'=') {
        return Ok(None);
    }
    if let Some(variable) = SETTINGS_READ_FROM_A_USER_VARIABLE
        .iter()
        .find(|variable| name.eq_ignore_ascii_case(variable))
    {
        if let Some(user_variable) = scanner.take_user_variable_reference() {
            return Ok(Some(MySqlSessionSetting::FromUserVariable {
                variable: (*variable).to_owned(),
                user_variable,
            }));
        }
    }
    let setting = if name.eq_ignore_ascii_case("transaction_isolation") {
        let Some(level) = scanner.take_string(mode) else {
            return Ok(None);
        };
        MySqlSessionSetting::TransactionIsolationLevel {
            level,
            next_transaction_only: unscoped_system_variable,
        }
    } else if name.eq_ignore_ascii_case("sql_mode") {
        if let Some(name) = scanner.take_user_variable_reference() {
            MySqlSessionSetting::SqlModeFromUserVariable(name)
        } else if let Some(value) = scanner.take_sql_mode_value(mode) {
            match value {
                SqlModeValue::Literal(value) => {
                    MySqlSessionSetting::SqlMode(named_sql_modes(&value))
                }
                expression => MySqlSessionSetting::SqlModeExpression(expression),
            }
        } else {
            return Ok(None);
        }
    } else if name.eq_ignore_ascii_case("wait_timeout") {
        if scanner.take_keyword("DEFAULT") {
            MySqlSessionSetting::WaitTimeout(None)
        } else if let Some(value) = scanner.take_unsigned() {
            MySqlSessionSetting::WaitTimeout(Some(value))
        } else {
            return Ok(None);
        }
    } else if name.eq_ignore_ascii_case("sql_auto_is_null") {
        let Some(value) = scanner.take_switch() else {
            return Ok(None);
        };
        MySqlSessionSetting::SqlAutoIsNull(value)
    } else if name.eq_ignore_ascii_case("sql_safe_updates") {
        let Some(value) = scanner.take_switch() else {
            return Ok(None);
        };
        MySqlSessionSetting::SqlSafeUpdates(value)
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
    } else if name.eq_ignore_ascii_case("foreign_key_checks") {
        let Some(value) = take_checked_switch(scanner)? else {
            return Ok(None);
        };
        MySqlSessionSetting::ForeignKeyChecks(value)
    } else if name.eq_ignore_ascii_case("unique_checks") {
        let Some(value) = take_checked_switch(scanner)? else {
            return Ok(None);
        };
        MySqlSessionSetting::UniqueChecks(value)
    } else if name.eq_ignore_ascii_case("sql_notes") {
        let Some(value) = take_checked_switch(scanner)? else {
            return Ok(None);
        };
        MySqlSessionSetting::SqlNotes(value)
    } else if name.eq_ignore_ascii_case("character_set_results") {
        if scanner.take_keyword("NULL") {
            MySqlSessionSetting::CharacterSetResultsNull
        } else if let Some(value) = scanner.take_charset_name_or_user_variable(mode) {
            MySqlSessionSetting::CharacterSetResults(value)
        } else {
            return Ok(None);
        }
    } else if name.eq_ignore_ascii_case("character_set_client") {
        let Some(value) = scanner.take_charset_name_or_user_variable(mode) else {
            return Ok(None);
        };
        MySqlSessionSetting::CharacterSetClient(value)
    } else if name.eq_ignore_ascii_case("collation_connection") {
        let Some(value) = scanner.take_charset_name_or_user_variable(mode) else {
            return Ok(None);
        };
        MySqlSessionSetting::CollationConnection(value)
    } else if name.eq_ignore_ascii_case("sql_quote_show_create") {
        let Some(value) = scanner.take_unsigned() else {
            return Ok(None);
        };
        match value {
            0 => MySqlSessionSetting::SqlQuoteShowCreate(false),
            1 => MySqlSessionSetting::SqlQuoteShowCreate(true),
            _ => return unsupported("sql_quote_show_create value; expected 0 or 1"),
        }
    } else {
        return Ok(None);
    };
    Ok(Some(setting))
}

/// Reads a switch whose other numbers MySQL refuses.
///
/// Measured on MySQL 8.4.11: the switch is written `0`/`1` and `OFF`/`ON`
/// alike, both reading back as `0` and `1`, and any other number answers 1231.
fn take_checked_switch(scanner: &mut Scanner<'_>) -> Result<Option<bool>, ParseError> {
    match scanner.take_unsigned() {
        Some(0) => Ok(Some(false)),
        Some(1) => Ok(Some(true)),
        Some(_) => unsupported("switch value; expected 0, 1, ON or OFF"),
        None if scanner.take_keyword("ON") => Ok(Some(true)),
        None if scanner.take_keyword("OFF") => Ok(Some(false)),
        None => Ok(None),
    }
}

/// One `SET @name = value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlUserVariableAssignment {
    name: String,
    source: MySqlUserVariableAssignmentSource,
}

impl MySqlUserVariableAssignment {
    /// Returns the variable named, lowercased and without the `@`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns what the variable is set to.
    pub fn literal_value(&self) -> Option<&MySqlUserVariableValue> {
        match &self.source {
            MySqlUserVariableAssignmentSource::Literal(value) => Some(value),
            _ => None,
        }
    }

    pub fn system_variable(&self) -> Option<&str> {
        match &self.source {
            MySqlUserVariableAssignmentSource::SystemVariable(name) => Some(name),
            _ => None,
        }
    }

    pub fn user_variable(&self) -> Option<&str> {
        match &self.source {
            MySqlUserVariableAssignmentSource::UserVariable(name) => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MySqlUserVariableAssignmentSource {
    Literal(MySqlUserVariableValue),
    SystemVariable(String),
    UserVariable(String),
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
    fn take_written_variable(&mut self) -> Option<WrittenVariable> {
        self.skip_spaces();
        let mut cursor = self.cursor;
        let mut scope = AssignmentScope::Default;
        let signed = self.sql[cursor..].starts_with("@@");
        if signed {
            cursor += 2;
            for (written, named) in [
                ("SESSION.", AssignmentScope::Session),
                ("LOCAL.", AssignmentScope::Session),
                ("GLOBAL.", AssignmentScope::Global),
                ("PERSIST.", AssignmentScope::Global),
                ("PERSIST_ONLY.", AssignmentScope::Global),
            ] {
                if self.sql[cursor..].len() >= written.len()
                    && self.sql[cursor..cursor + written.len()].eq_ignore_ascii_case(written)
                {
                    cursor += written.len();
                    scope = named;
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
        Some(WrittenVariable {
            name,
            scope,
            signed,
        })
    }

    /// Reads a switch written `0`, `1`, `OFF` or `ON`.
    fn take_switch(&mut self) -> Option<bool> {
        match self.take_unsigned() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            Some(_) => None,
            None if self.take_keyword("ON") => Some(true),
            None if self.take_keyword("OFF") => Some(false),
            None => None,
        }
    }

    /// Reads a `sql_mode` value: a string, `DEFAULT`, the current value read
    /// through `@@sql_mode`, or `CONCAT` and `REPLACE` over those.
    fn take_sql_mode_value(&mut self, mode: SessionSqlMode) -> Option<SqlModeValue> {
        self.skip_spaces();
        if let Some(value) = self.take_string(mode) {
            return Some(SqlModeValue::Literal(value));
        }
        if self.sql[self.cursor..].starts_with("@@") {
            let WrittenVariable { name, scope, .. } = self.take_written_variable()?;
            if !name.eq_ignore_ascii_case("sql_mode") {
                return None;
            }
            return Some(if scope == AssignmentScope::Global {
                SqlModeValue::Default
            } else {
                SqlModeValue::Current
            });
        }
        if self.take_keyword("DEFAULT") {
            return Some(SqlModeValue::Default);
        }
        let restore = self.cursor;
        let concat = self.take_keyword("CONCAT");
        if !concat && !self.take_keyword("REPLACE") {
            return None;
        }
        if !self.take_byte(b'(') {
            self.cursor = restore;
            return None;
        }
        let mut arguments = vec![self.take_sql_mode_value(mode)?];
        while self.take_byte(b',') {
            arguments.push(self.take_sql_mode_value(mode)?);
        }
        if !self.take_byte(b')') {
            return None;
        }
        if concat {
            return Some(SqlModeValue::Concat(arguments));
        }
        let [text, from, to] = <[SqlModeValue; 3]>::try_from(arguments).ok()?;
        Some(SqlModeValue::Replace(
            Box::new(text),
            Box::new(from),
            Box::new(to),
        ))
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

    fn take_user_variable_reference(&mut self) -> Option<String> {
        self.skip_spaces();
        if !self.sql[self.cursor..].starts_with('@') || self.sql[self.cursor..].starts_with("@@") {
            return None;
        }
        self.cursor += 1;
        self.take_user_variable_name()
            .map(|name| name.to_ascii_lowercase())
    }

    fn take_system_variable_reference(&mut self) -> Option<String> {
        self.skip_spaces();
        if !self.sql[self.cursor..].starts_with("@@") {
            return None;
        }
        self.cursor += 2;
        for scope in ["SESSION.", "LOCAL."] {
            if self.sql[self.cursor..].len() >= scope.len()
                && self.sql[self.cursor..self.cursor + scope.len()].eq_ignore_ascii_case(scope)
            {
                self.cursor += scope.len();
                break;
            }
        }
        let end = self.word_end(self.cursor);
        if end == self.cursor {
            return None;
        }
        let name = self.sql[self.cursor..end].to_ascii_lowercase();
        self.cursor = end;
        Some(name)
    }

    fn take_user_variable_assignment_source(
        &mut self,
        mode: SessionSqlMode,
    ) -> Option<MySqlUserVariableAssignmentSource> {
        if let Some(name) = self.take_system_variable_reference() {
            return Some(MySqlUserVariableAssignmentSource::SystemVariable(name));
        }
        if let Some(name) = self.take_user_variable_reference() {
            return Some(MySqlUserVariableAssignmentSource::UserVariable(name));
        }
        self.take_user_variable_value(mode)
            .map(MySqlUserVariableAssignmentSource::Literal)
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

    fn take_charset_name_or_user_variable(&mut self, mode: SessionSqlMode) -> Option<String> {
        if let Some(name) = self.take_user_variable_reference() {
            return Some(format!("@{name}"));
        }
        self.take_charset_name(mode)
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
        parse_all(sql).map(|mut settings| {
            assert_eq!(settings.len(), 1, "{sql}");
            settings.remove(0)
        })
    }

    fn parse_all(sql: &str) -> Option<Vec<MySqlSessionSetting>> {
        parse_optional_session_settings(sql, SessionSqlMode::default()).unwrap()
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

    #[test]
    fn reads_connector_j_result_conversion_setting() {
        assert_eq!(
            parse("SET character_set_results = NULL"),
            Some(MySqlSessionSetting::CharacterSetResultsNull)
        );
        assert_eq!(
            parse("SET character_set_results = latin1"),
            Some(MySqlSessionSetting::CharacterSetResults(
                "latin1".to_owned()
            ))
        );
    }

    #[test]
    fn reads_the_session_settings_in_a_compact_mysqldump() {
        assert_eq!(
            parse("SET SQL_QUOTE_SHOW_CREATE=1"),
            Some(MySqlSessionSetting::SqlQuoteShowCreate(true))
        );
        assert_eq!(
            parse("SET SESSION character_set_results = 'binary'"),
            Some(MySqlSessionSetting::CharacterSetResults(
                "binary".to_owned()
            ))
        );
        assert_eq!(
            parse("/*!50503 SET character_set_client = utf8mb4 */"),
            Some(MySqlSessionSetting::CharacterSetClient(
                "utf8mb4".to_owned()
            ))
        );
        assert_eq!(
            parse("/*!50003 SET character_set_results = utf8mb4 */"),
            Some(MySqlSessionSetting::CharacterSetResults(
                "utf8mb4".to_owned()
            ))
        );
        assert_eq!(
            parse("/*!50003 SET collation_connection = utf8mb4_general_ci */"),
            Some(MySqlSessionSetting::CollationConnection(
                "utf8mb4_general_ci".to_owned()
            ))
        );
        assert_eq!(
            parse("/*!50003 SET sql_mode = @saved_sql_mode */"),
            Some(MySqlSessionSetting::SqlModeFromUserVariable(
                "saved_sql_mode".to_owned()
            ))
        );
        assert_eq!(
            parse("/*!50003 SET character_set_client = @saved_cs_client */"),
            Some(MySqlSessionSetting::CharacterSetClient(
                "@saved_cs_client".to_owned()
            ))
        );
    }

    /// Every one of MySQL's four levels is read, with or without a scope word.
    /// Which of them this server can honestly accept is the server's question,
    /// not the parser's, so all four come back here.
    #[test]
    fn reads_set_transaction_isolation_level() {
        for (sql, level, next_transaction_only) in [
            (
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                "REPEATABLE READ",
                true,
            ),
            (
                "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                "REPEATABLE READ",
                false,
            ),
            (
                "set local transaction isolation level read committed",
                "READ COMMITTED",
                false,
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
                "READ UNCOMMITTED",
                true,
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;",
                "SERIALIZABLE",
                true,
            ),
            (
                "SET @@SESSION.transaction_isolation = 'READ-COMMITTED'",
                "READ-COMMITTED",
                false,
            ),
            (
                "SET SESSION transaction_isolation = 'REPEATABLE-READ'",
                "REPEATABLE-READ",
                false,
            ),
            (
                "SET transaction_isolation = 'READ-COMMITTED'",
                "READ-COMMITTED",
                false,
            ),
            (
                "SET @@transaction_isolation = 'REPEATABLE-READ'",
                "REPEATABLE-READ",
                true,
            ),
        ] {
            assert_eq!(
                parse(sql),
                Some(MySqlSessionSetting::TransactionIsolationLevel {
                    level: level.to_owned(),
                    next_transaction_only,
                }),
                "{sql}"
            );
        }

        // GLOBAL changes what other sessions get, which is refused.
        assert!(matches!(
            parse_optional_session_settings(
                "SET GLOBAL TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                SessionSqlMode::default()
            ),
            Err(ParseError::Unsupported { .. })
        ));
        for sql in [
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
            "SET SESSION NET_READ_TIMEOUT= 86400, SESSION NET_WRITE_TIMEOUT= 86400",
            "SET sql_mode",
            "",
        ] {
            assert_eq!(parse(sql), None, "{sql}");
        }
    }

    /// A user variable belongs to the connection. Measured on MySQL 8.4.11:
    /// names are matched whatever their case, `:=` also sets a variable, and
    /// one statement can set several.
    #[test]
    fn reads_a_user_variable_assignment() {
        let read = |sql: &str| {
            parse_all(sql)
                .unwrap()
                .into_iter()
                .map(|setting| {
                    let MySqlSessionSetting::UserVariable(assignment) = setting else {
                        panic!("{sql}: {setting:?}");
                    };
                    (
                        assignment.name().to_owned(),
                        assignment.literal_value().cloned().unwrap(),
                    )
                })
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

        assert_eq!(parse("SELECT @x"), None);

        // An expression is refused rather than half-answered.
        for sql in [
            "SET @y := @x + 1",
            "SET @x = (SELECT 1)",
            "SET @x = 1 extra",
        ] {
            assert!(
                parse_optional_session_settings(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }

    fn saved(setting: &MySqlSessionSetting) -> &MySqlUserVariableAssignment {
        let MySqlSessionSetting::UserVariable(assignment) = setting else {
            panic!("{setting:?} saves no user variable");
        };
        assignment
    }

    #[test]
    fn reads_the_saved_variables_in_a_compact_mysqldump() {
        let setting = parse("/*!50003 SET @saved_cs_client = @@character_set_client */").unwrap();
        assert_eq!(saved(&setting).name(), "saved_cs_client");
        assert_eq!(
            saved(&setting).system_variable(),
            Some("character_set_client")
        );

        let setting = parse("SET @next = @saved_cs_client").unwrap();
        assert_eq!(saved(&setting).user_variable(), Some("saved_cs_client"));
    }

    /// The settings a standard `mysqldump` opens with each save the value in
    /// force before changing it, in one `SET`, and it puts every one back at
    /// the end.
    #[test]
    fn reads_the_settings_a_standard_mysqldump_saves_and_puts_back() {
        let settings =
            parse_all("/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */")
                .unwrap();
        assert_eq!(saved(&settings[0]).name(), "old_unique_checks");
        assert_eq!(saved(&settings[0]).system_variable(), Some("unique_checks"));
        assert_eq!(settings[1], MySqlSessionSetting::UniqueChecks(false));

        let settings =
            parse_all("/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */")
                .unwrap();
        assert_eq!(saved(&settings[0]).system_variable(), Some("sql_mode"));
        assert_eq!(
            settings[1],
            MySqlSessionSetting::SqlMode(vec!["NO_AUTO_VALUE_ON_ZERO".to_owned()])
        );
        assert_eq!(
            parse_all("/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */").unwrap()[1],
            MySqlSessionSetting::SqlNotes(false)
        );
        assert_eq!(
            saved(&parse("/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */").unwrap()).system_variable(),
            Some("time_zone")
        );

        for (sql, variable, user_variable) in [
            (
                "/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */",
                "time_zone",
                "old_time_zone",
            ),
            (
                "/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */",
                "foreign_key_checks",
                "old_foreign_key_checks",
            ),
            (
                "/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */",
                "unique_checks",
                "old_unique_checks",
            ),
            (
                "/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */",
                "sql_notes",
                "old_sql_notes",
            ),
        ] {
            assert_eq!(
                parse(sql),
                Some(MySqlSessionSetting::FromUserVariable {
                    variable: variable.to_owned(),
                    user_variable: user_variable.to_owned(),
                }),
                "{sql}"
            );
        }
        for sql in ["SET unique_checks = 2", "SET sql_notes = 2"] {
            assert!(
                matches!(
                    parse_optional_session_settings(sql, SessionSqlMode::default()),
                    Err(ParseError::Unsupported { .. })
                ),
                "{sql}"
            );
        }
        assert_eq!(
            parse("SET SESSION sql_notes = ON"),
            Some(MySqlSessionSetting::SqlNotes(true))
        );
    }

    #[test]
    fn refuses_a_setting_with_more_after_it() {
        assert!(parse_optional_session_settings(
            "SET sql_mode = '' extra",
            SessionSqlMode::default()
        )
        .is_err());
        // A variable this reader does not know leaves the whole statement to
        // the other readers, which refuse it.
        assert_eq!(parse_all("SET time_zone = '+00:00' , x = 1"), None);
    }

    /// Rails opens every connection with one `SET` of three assignments, and
    /// Laravel with two; each comes back in the order written.
    #[test]
    fn reads_several_assignments_in_one_set() {
        let rails = "SET NAMES utf8mb4,  @@SESSION.sql_mode = CONCAT(CONCAT(@@sql_mode, ',STRICT_ALL_TABLES'), ',NO_AUTO_VALUE_ON_ZERO'),  @@SESSION.wait_timeout = 2147483";
        let settings = parse_all(rails).unwrap();
        assert_eq!(settings.len(), 3);
        assert_eq!(
            settings[0],
            MySqlSessionSetting::Names {
                character_set: "utf8mb4".to_owned(),
                collation: None,
            }
        );
        let MySqlSessionSetting::SqlModeExpression(sql_mode) = &settings[1] else {
            panic!("{:?}", settings[1]);
        };
        assert_eq!(
            sql_mode.named_modes("ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES", ""),
            [
                "ONLY_FULL_GROUP_BY",
                "STRICT_TRANS_TABLES",
                "STRICT_ALL_TABLES",
                "NO_AUTO_VALUE_ON_ZERO"
            ]
        );
        assert_eq!(
            settings[2],
            MySqlSessionSetting::WaitTimeout(Some(2_147_483))
        );

        let laravel = "SET NAMES 'utf8mb4' COLLATE 'utf8mb4_unicode_ci', SESSION sql_mode='ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES';";
        assert_eq!(
            parse_all(laravel),
            Some(vec![
                MySqlSessionSetting::Names {
                    character_set: "utf8mb4".to_owned(),
                    collation: Some("utf8mb4_unicode_ci".to_owned()),
                },
                MySqlSessionSetting::SqlMode(vec![
                    "ONLY_FULL_GROUP_BY".to_owned(),
                    "STRICT_TRANS_TABLES".to_owned()
                ]),
            ])
        );

        assert_eq!(
            parse_all("SET foreign_key_checks=1, sql_safe_updates=0"),
            Some(vec![
                MySqlSessionSetting::ForeignKeyChecks(true),
                MySqlSessionSetting::SqlSafeUpdates(false),
            ])
        );
        assert_eq!(
            parse("SET SQL_AUTO_IS_NULL = 0"),
            Some(MySqlSessionSetting::SqlAutoIsNull(false))
        );
        assert_eq!(
            parse("SET @@SESSION.wait_timeout = DEFAULT"),
            Some(MySqlSessionSetting::WaitTimeout(None))
        );
    }

    /// Rails without strict mode takes the strict modes out with `REPLACE`.
    /// Measured on MySQL 8.4.11: a `REPLACE` of text that is not there leaves
    /// the value alone, and the empty entries it leaves are dropped.
    #[test]
    fn works_out_a_sql_mode_from_the_one_in_force() {
        let Some(MySqlSessionSetting::SqlModeExpression(value)) = parse(
            "SET @@SESSION.sql_mode = CONCAT(REPLACE(REPLACE(REPLACE(@@sql_mode, 'STRICT_TRANS_TABLES', ''), 'STRICT_ALL_TABLES', ''), 'TRADITIONAL', ''), ',NO_AUTO_VALUE_ON_ZERO')",
        ) else {
            panic!("the expression must be read");
        };
        assert_eq!(
            value.named_modes("ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_DATE", ""),
            [
                "ONLY_FULL_GROUP_BY",
                "NO_ZERO_DATE",
                "NO_AUTO_VALUE_ON_ZERO"
            ]
        );
        let Some(MySqlSessionSetting::SqlModeExpression(value)) = parse("SET sql_mode = DEFAULT")
        else {
            panic!("DEFAULT must be read");
        };
        assert_eq!(
            value.named_modes("ANSI_QUOTES", "STRICT_TRANS_TABLES"),
            ["STRICT_TRANS_TABLES"]
        );
        let Some(MySqlSessionSetting::SqlModeExpression(value)) =
            parse("SET sql_mode = CONCAT(@@GLOBAL.sql_mode, ',ANSI_QUOTES')")
        else {
            panic!("the global value must be read");
        };
        assert_eq!(
            value.named_modes("NO_ZERO_DATE", "STRICT_TRANS_TABLES"),
            ["STRICT_TRANS_TABLES", "ANSI_QUOTES"]
        );
    }

    /// Measured on MySQL 8.4.11: a scope word holds for the assignments after
    /// it, so `SET GLOBAL a = 0, b = 0` sets both globally. Nothing here can
    /// change another session, so every global assignment is refused.
    #[test]
    fn refuses_a_global_assignment_wherever_it_is_written() {
        for sql in [
            "SET GLOBAL sql_notes = 0",
            "SET @@GLOBAL.sql_mode = ''",
            "SET SESSION foreign_key_checks = 0, GLOBAL wait_timeout = 10",
            "SET GLOBAL wait_timeout = 10, foreign_key_checks = 0",
            "SET PERSIST wait_timeout = 10",
        ] {
            assert!(
                matches!(
                    parse_optional_session_settings(sql, SessionSqlMode::default()),
                    Err(ParseError::Unsupported { .. })
                ),
                "{sql}"
            );
        }
    }
}
