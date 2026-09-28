//! The `SELECT`s a session answers on its own, without reading a table.
//!
//! Each keeps the spelling the client sent, because MySQL names the result
//! column after the expression as written.

use super::{MySqlVariableScope, ParseError, SessionSqlMode};

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
    let alias = if scanner.at_keyword("LIMIT") {
        None
    } else {
        scanner.take_alias()?
    };
    scanner.skip_gaps();
    // A list of calls, or a limit, is for the reader of session calls.
    if scanner.at_byte(b',') || scanner.at_keyword("LIMIT") {
        return Ok(None);
    }
    if !scanner.at_end() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlSelectDatabaseQuery {
        column_name: alias.unwrap_or(call),
    }))
}

/// A checked `SELECT` of system variables, which the session answers from what
/// it knows about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlSystemVariableQuery {
    reads: Vec<MySqlSystemVariableRead>,
}

impl MySqlSystemVariableQuery {
    /// Returns the variables the statement reads, in projection order.
    pub fn reads(&self) -> &[MySqlSystemVariableRead] {
        &self.reads
    }
}

/// A call answering what the session knows about itself rather than a
/// variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlSessionCall {
    /// `DATABASE()` or `SCHEMA()`.
    Database,
    /// `USER()`, `SESSION_USER()` or `SYSTEM_USER()`: the name the client
    /// logged in with and the host it came from.
    User,
    /// `CURRENT_USER()` or `CURRENT_USER`: the account the server matched.
    CurrentUser,
    /// `CONNECTION_ID()`.
    ConnectionId,
    /// `ROW_COUNT()`: what the last statement changed.
    RowCount,
    /// `FOUND_ROWS()`: how many rows the last `SELECT` answered.
    FoundRows,
}

/// One system variable a `SELECT` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlSystemVariableRead {
    name: String,
    scope: MySqlVariableScope,
    session_call: Option<MySqlSessionCall>,
    /// Whether `SESSION.` or `LOCAL.` was written before the name.
    session_named: bool,
    called: bool,
    zone_conversion: Option<(String, String)>,
    column_name: String,
}

impl MySqlSystemVariableRead {
    /// Returns the variable named, without the `@@` or a scope prefix.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the scope the read named, which decides which value answers it.
    ///
    /// `@@name` and `@@session.name` read what this session is using, and
    /// `@@global.name` reads what a new session would start from. Measured on
    /// MySQL 8.4.11: after `SET SESSION autocommit = 0`, `@@global.autocommit`
    /// still answers 1.
    pub fn scope(&self) -> MySqlVariableScope {
        self.scope
    }

    /// Returns whether the read named the session's value in so many words —
    /// `@@SESSION.name` or `@@LOCAL.name` — which MySQL refuses for a
    /// variable only the server has.
    pub fn names_the_session(&self) -> bool {
        self.session_named
    }

    /// Returns the call this reads, when it reads what the session knows
    /// about itself rather than a variable. [`Self::name`] is empty then.
    pub fn session_call(&self) -> Option<MySqlSessionCall> {
        self.session_call
    }

    /// Returns whether the version was asked for as a call rather than as a
    /// variable — `VERSION()` rather than `@@version`.
    ///
    /// Measured on MySQL 8.4.11: the call answers a `VAR_STRING` of length 24
    /// that is NOT NULL where the variable answers one of length 87380 that is
    /// not, and an alias over either leaves that alone.
    pub fn called(&self) -> bool {
        self.called
    }

    /// Returns the two zones of a `CONVERT_TZ('<moment>', from, to) IS NOT
    /// NULL`, which asks whether the server can convert between them.
    ///
    /// Django asks this beside `VERSION()` and four variables whenever it
    /// connects, to learn whether the server has named zones.
    pub fn zone_conversion(&self) -> Option<(&str, &str)> {
        self.zone_conversion
            .as_ref()
            .map(|(from, to)| (from.as_str(), to.as_str()))
    }

    /// Returns the name MySQL gives the result column.
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
/// A driver opens the connection by reading a row of these at once, so a list
/// of them is read rather than only one.
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
    let mut reads = Vec::new();
    loop {
        scanner.skip_gaps();
        let Some(read) = take_system_variable_read(&mut scanner, sql)? else {
            return Ok(None);
        };
        reads.push(read);
        scanner.skip_gaps();
        if !scanner.take_byte(b',') {
            break;
        }
    }
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
    // Anything left over means another parser owns the statement.
    if !scanner.at_end() {
        return Ok(None);
    }
    Ok(Some(MySqlSystemVariableQuery { reads }))
}

/// Reads one `@@name` or `VERSION()`, with the alias that may follow it.
fn take_system_variable_read(
    scanner: &mut Scanner,
    sql: &str,
) -> Result<Option<MySqlSystemVariableRead>, ParseError> {
    let start = scanner.cursor;
    let mut scope = MySqlVariableScope::Session;
    let mut session_named = false;
    let mut called = false;
    let mut zone_conversion = None;
    let mut session_call = None;
    let name = if scanner.take_keyword("CONVERT_TZ") {
        let Some(zones) = take_zone_conversion_probe(scanner) else {
            return Ok(None);
        };
        zone_conversion = Some(zones);
        String::new()
    } else if scanner.take_keyword("VERSION") {
        scanner.skip_gaps();
        if !scanner.take_byte(b'(') {
            return Ok(None);
        }
        scanner.skip_gaps();
        if !scanner.take_byte(b')') {
            return Ok(None);
        }
        called = true;
        "version".to_owned()
    } else if let Some(call) = take_session_call(scanner) {
        session_call = Some(call);
        String::new()
    } else {
        if !scanner.take_byte(b'@') || !scanner.take_byte(b'@') {
            return Ok(None);
        }
        for (prefix, named) in [
            ("SESSION.", MySqlVariableScope::Session),
            ("LOCAL.", MySqlVariableScope::Session),
            ("GLOBAL.", MySqlVariableScope::Global),
        ] {
            if scanner.take_keyword(&prefix[..prefix.len() - 1]) {
                if !scanner.take_byte(b'.') {
                    return Ok(None);
                }
                scope = named;
                session_named = named == MySqlVariableScope::Session;
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
    Ok(Some(MySqlSystemVariableRead {
        name,
        scope,
        session_call,
        session_named,
        called,
        zone_conversion,
        column_name: alias.unwrap_or(expression),
    }))
}

/// Reads one call answering what the session knows about itself.
///
/// Each takes an empty argument list; `CURRENT_USER` may also be written
/// without one, which MySQL names the column after as written.
fn take_session_call(scanner: &mut Scanner) -> Option<MySqlSessionCall> {
    let start = scanner.cursor;
    for (name, call) in [
        ("DATABASE", MySqlSessionCall::Database),
        ("SCHEMA", MySqlSessionCall::Database),
        ("USER", MySqlSessionCall::User),
        ("SESSION_USER", MySqlSessionCall::User),
        ("SYSTEM_USER", MySqlSessionCall::User),
        ("CURRENT_USER", MySqlSessionCall::CurrentUser),
        ("CONNECTION_ID", MySqlSessionCall::ConnectionId),
        ("ROW_COUNT", MySqlSessionCall::RowCount),
        ("FOUND_ROWS", MySqlSessionCall::FoundRows),
    ] {
        if !scanner.take_keyword(name) {
            continue;
        }
        let after_name = scanner.cursor;
        scanner.skip_gaps();
        if scanner.take_byte(b'(') {
            scanner.skip_gaps();
            if scanner.take_byte(b')') {
                return Some(call);
            }
        } else if call == MySqlSessionCall::CurrentUser {
            scanner.cursor = after_name;
            return Some(call);
        }
        scanner.cursor = start;
        return None;
    }
    None
}

/// Reads the rest of `CONVERT_TZ('<moment>', from, to) IS NOT NULL`.
///
/// The moment has to be written the one way MySQL reads without a rule of
/// its own, `YYYY-MM-DD hh:mm:ss`, since a moment it cannot read makes the
/// call NULL whatever the zones are.
fn take_zone_conversion_probe(scanner: &mut Scanner) -> Option<(String, String)> {
    scanner.skip_gaps();
    if !scanner.take_byte(b'(') {
        return None;
    }
    scanner.skip_gaps();
    let moment = scanner.take_nullable_string()??;
    if !is_a_plain_moment(&moment) {
        return None;
    }
    let mut zones = [String::new(), String::new()];
    for zone in &mut zones {
        scanner.skip_gaps();
        if !scanner.take_byte(b',') {
            return None;
        }
        scanner.skip_gaps();
        *zone = scanner.take_nullable_string()??;
    }
    scanner.skip_gaps();
    if !scanner.take_byte(b')') {
        return None;
    }
    for word in ["IS", "NOT", "NULL"] {
        scanner.skip_gaps();
        if !scanner.take_keyword(word) {
            return None;
        }
    }
    let [from, to] = zones;
    Some((from, to))
}

fn is_a_plain_moment(moment: &str) -> bool {
    let bytes = moment.as_bytes();
    let number = |range: std::ops::Range<usize>| {
        bytes[range.clone()]
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| moment[range].parse::<u32>().ok())
            .flatten()
    };
    bytes.len() == 19
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b' '
        && bytes[13] == b':'
        && bytes[16] == b':'
        && number(0..4).is_some_and(|year| year >= 1)
        && number(5..7).is_some_and(|month| (1..=12).contains(&month))
        && number(8..10).is_some_and(|day| (1..=28).contains(&day))
        && number(11..13).is_some_and(|hour| hour < 24)
        && number(14..16).is_some_and(|minute| minute < 60)
        && number(17..19).is_some_and(|second| second < 60)
}

/// A checked `SELECT` of user variables, which the session answers from what
/// it was told to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlUserVariableQuery {
    reads: Vec<MySqlUserVariableRead>,
}

impl MySqlUserVariableQuery {
    /// Returns the variables the statement reads, in projection order.
    pub fn reads(&self) -> &[MySqlUserVariableRead] {
        &self.reads
    }
}

/// One user variable a `SELECT` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlUserVariableRead {
    name: String,
    column_name: String,
}

impl MySqlUserVariableRead {
    /// Returns the variable named, lowercased and without the `@`.
    ///
    /// Measured on MySQL 8.4.11: `SET @x = 1; SELECT @X` answers 1, so the
    /// names are matched whatever their case.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the name MySQL gives the result column.
    ///
    /// Without an alias this is the variable as the client wrote it, `@X` and
    /// `@x` naming their columns differently even though they are one
    /// variable.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }
}

/// Parses `SELECT @name[, @name...]` when that is the whole statement.
///
/// A projection that names anything but user variables returns `None`, so the
/// ordinary `SELECT` path keeps it. `@@name` is a system variable and belongs
/// to [`parse_optional_system_variable_query`], so it is left alone here.
pub fn parse_optional_user_variable_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlUserVariableQuery>, ParseError> {
    let mut scanner = Scanner::new(sql, mode);
    scanner.skip_gaps();
    if !scanner.take_keyword("SELECT") {
        return Ok(None);
    }
    let mut reads = Vec::new();
    loop {
        scanner.skip_gaps();
        let start = scanner.cursor;
        if !scanner.take_byte(b'@') || scanner.at_byte(b'@') {
            return Ok(None);
        }
        let Some(name) = scanner.take_word() else {
            return Ok(None);
        };
        let expression = sql[start..scanner.cursor].to_owned();
        scanner.skip_gaps();
        let alias = scanner.take_alias()?;
        reads.push(MySqlUserVariableRead {
            name: name.to_ascii_lowercase(),
            column_name: alias.unwrap_or(expression),
        });
        scanner.skip_gaps();
        if !scanner.take_byte(b',') {
            break;
        }
    }
    // Anything left over — a FROM, a WHERE, another kind of term — means the
    // ordinary SELECT path owns the statement.
    if !scanner.at_end() {
        return Ok(None);
    }
    Ok(Some(MySqlUserVariableQuery { reads }))
}

/// A `SELECT` of calls on MySQL's named locks, which the server answers from
/// what every session holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlNamedLockQuery {
    calls: Vec<MySqlNamedLockCall>,
}

impl MySqlNamedLockQuery {
    /// Returns the calls the statement makes, in projection order, which is
    /// the order MySQL makes them in.
    pub fn calls(&self) -> &[MySqlNamedLockCall] {
        &self.calls
    }
}

/// One call on a named lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlNamedLockCall {
    function: MySqlNamedLockFunction,
    column_name: String,
}

impl MySqlNamedLockCall {
    pub fn function(&self) -> &MySqlNamedLockFunction {
        &self.function
    }

    /// Returns the name MySQL gives the result column: the call as the client
    /// wrote it, or its alias.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }
}

/// What one call asks of a named lock. A name of `None` was written `NULL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlNamedLockFunction {
    /// `GET_LOCK(name, timeout)`, with the timeout in whole seconds. Measured
    /// on MySQL 8.4.11: a `NULL` timeout waits no time at all, and a negative
    /// one waits without end.
    Get {
        name: Option<String>,
        timeout_seconds: i64,
    },
    Release {
        name: Option<String>,
    },
    IsFree {
        name: Option<String>,
    },
    IsUsed {
        name: Option<String>,
    },
    ReleaseAll,
}

/// Parses a `SELECT` whose every term is a call on a named lock — how Rails,
/// Prisma and Flyway take one around a migration.
///
/// Anything else returns `None`, so the ordinary `SELECT` path keeps it. A
/// timeout written as anything but a whole number or `NULL` is left to that
/// path too: MySQL reads a fraction as whole seconds by a rule of its own.
pub fn parse_optional_named_lock_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlNamedLockQuery>, ParseError> {
    let mut scanner = Scanner::new(sql, mode);
    scanner.skip_gaps();
    if !scanner.take_keyword("SELECT") {
        return Ok(None);
    }
    let mut calls = Vec::new();
    loop {
        scanner.skip_gaps();
        let start = scanner.cursor;
        let Some(function) = take_named_lock_function(&mut scanner) else {
            return Ok(None);
        };
        let expression = sql[start..scanner.cursor].to_owned();
        scanner.skip_gaps();
        let alias = scanner.take_alias()?;
        calls.push(MySqlNamedLockCall {
            function,
            column_name: alias.unwrap_or(expression),
        });
        scanner.skip_gaps();
        if !scanner.take_byte(b',') {
            break;
        }
    }
    if !scanner.at_end() {
        return Ok(None);
    }
    Ok(Some(MySqlNamedLockQuery { calls }))
}

fn take_named_lock_function(scanner: &mut Scanner) -> Option<MySqlNamedLockFunction> {
    let function = if scanner.take_keyword("GET_LOCK") {
        NamedLockName::Get
    } else if scanner.take_keyword("RELEASE_LOCK") {
        NamedLockName::Release
    } else if scanner.take_keyword("IS_FREE_LOCK") {
        NamedLockName::IsFree
    } else if scanner.take_keyword("IS_USED_LOCK") {
        NamedLockName::IsUsed
    } else if scanner.take_keyword("RELEASE_ALL_LOCKS") {
        NamedLockName::ReleaseAll
    } else {
        return None;
    };
    scanner.skip_gaps();
    if !scanner.take_byte(b'(') {
        return None;
    }
    scanner.skip_gaps();
    if function == NamedLockName::ReleaseAll {
        return scanner
            .take_byte(b')')
            .then_some(MySqlNamedLockFunction::ReleaseAll);
    }
    let name = scanner.take_nullable_string()?;
    scanner.skip_gaps();
    let function = match function {
        NamedLockName::Get => {
            if !scanner.take_byte(b',') {
                return None;
            }
            scanner.skip_gaps();
            let timeout_seconds = if scanner.take_keyword("NULL") {
                0
            } else {
                scanner.take_whole_number()?
            };
            scanner.skip_gaps();
            MySqlNamedLockFunction::Get {
                name,
                timeout_seconds,
            }
        }
        NamedLockName::Release => MySqlNamedLockFunction::Release { name },
        NamedLockName::IsFree => MySqlNamedLockFunction::IsFree { name },
        NamedLockName::IsUsed => MySqlNamedLockFunction::IsUsed { name },
        NamedLockName::ReleaseAll => unreachable!("RELEASE_ALL_LOCKS takes no argument"),
    };
    scanner.take_byte(b')').then_some(function)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedLockName {
    Get,
    Release,
    IsFree,
    IsUsed,
    ReleaseAll,
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

    fn at_byte(&self, expected: u8) -> bool {
        self.bytes.get(self.cursor) == Some(&expected)
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

    /// Reads a quoted string, or `NULL` as `None`.
    ///
    /// A backslash is left to the ordinary path when it escapes, since what it
    /// escapes to is that path's to work out.
    fn take_nullable_string(&mut self) -> Option<Option<String>> {
        if self.take_keyword("NULL") {
            return Some(None);
        }
        let quote = match self.bytes.get(self.cursor) {
            Some(b'\'') => b'\'',
            Some(b'"') if !self.mode.ansi_quotes => b'"',
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
                return Some(Some(value));
            }
            if *byte == b'\\' && !self.mode.no_backslash_escapes {
                return None;
            }
            let end = next_character_end(self.sql, cursor);
            value.push_str(&self.sql[cursor..end]);
            cursor = end;
        }
        None
    }

    /// Reads a whole number, with a minus sign if it has one.
    fn take_whole_number(&mut self) -> Option<i64> {
        let start = self.cursor;
        let _ = self.take_byte(b'-');
        let digits = self.word_end();
        if digits == self.cursor
            || !self.sql[self.cursor..digits]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            self.cursor = start;
            return None;
        }
        self.cursor = digits;
        self.sql[start..digits].parse().ok()
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
    /// The lock calls Rails, Prisma and Flyway make around a migration, each
    /// named after the call as written unless given an alias.
    #[test]
    fn reads_calls_on_named_locks() {
        let read =
            |sql: &str| parse_optional_named_lock_query(sql, SessionSqlMode::default()).unwrap();

        let query = read("SELECT GET_LOCK('7458657555131878620', 0)").unwrap();
        assert_eq!(
            query.calls()[0].function(),
            &MySqlNamedLockFunction::Get {
                name: Some("7458657555131878620".to_owned()),
                timeout_seconds: 0,
            }
        );
        assert_eq!(
            query.calls()[0].column_name(),
            "GET_LOCK('7458657555131878620', 0)"
        );

        let query = read(
            "select get_lock('a', -1) as got, RELEASE_LOCK(NULL), IS_FREE_LOCK('b'), RELEASE_ALL_LOCKS();",
        )
        .unwrap();
        assert_eq!(
            query
                .calls()
                .iter()
                .map(|call| (call.function().clone(), call.column_name()))
                .collect::<Vec<_>>(),
            [
                (
                    MySqlNamedLockFunction::Get {
                        name: Some("a".to_owned()),
                        timeout_seconds: -1,
                    },
                    "got"
                ),
                (
                    MySqlNamedLockFunction::Release { name: None },
                    "RELEASE_LOCK(NULL)"
                ),
                (
                    MySqlNamedLockFunction::IsFree {
                        name: Some("b".to_owned())
                    },
                    "IS_FREE_LOCK('b')"
                ),
                (MySqlNamedLockFunction::ReleaseAll, "RELEASE_ALL_LOCKS()"),
            ]
        );
        assert_eq!(
            read("SELECT GET_LOCK('n', NULL)").unwrap().calls()[0].function(),
            &MySqlNamedLockFunction::Get {
                name: Some("n".to_owned()),
                timeout_seconds: 0,
            }
        );

        // A fraction, a word, a bound name, an escape, and a call beside
        // anything else all belong to the ordinary path.
        for sql in [
            "SELECT GET_LOCK('n', 1.5)",
            "SELECT GET_LOCK('n', '2')",
            "SELECT GET_LOCK(?, 10)",
            "SELECT GET_LOCK('a\\b', 0)",
            "SELECT GET_LOCK('n', 0), 1",
            "SELECT GET_LOCK('n', 0) FROM t",
            "SELECT 1",
        ] {
            assert_eq!(read(sql), None, "{sql}");
        }
    }

    /// A user variable is read with one `@`, where a system variable has two.
    /// Measured on MySQL 8.4.11: the column is named after the variable as the
    /// client wrote it, so `@X` and `@x` name different columns even though
    /// they are one variable.
    #[test]
    fn reads_a_user_variable_projection() {
        let read =
            |sql: &str| parse_optional_user_variable_query(sql, SessionSqlMode::default()).unwrap();

        let query = read("SELECT @X").unwrap();
        assert_eq!(query.reads().len(), 1);
        assert_eq!(query.reads()[0].name(), "x");
        assert_eq!(query.reads()[0].column_name(), "@X");

        let query = read("select @x, @s AS held;").unwrap();
        assert_eq!(
            query
                .reads()
                .iter()
                .map(|read| (read.name(), read.column_name()))
                .collect::<Vec<_>>(),
            [("x", "@x"), ("s", "held")]
        );

        // A system variable, a table read and a mixed projection all belong to
        // another parser.
        for sql in [
            "SELECT @@version",
            "SELECT 1",
            "SELECT @x FROM t",
            "SELECT @x, id FROM t",
            "SELECT @x := 1",
        ] {
            assert_eq!(read(sql), None, "{sql}");
        }
    }

    #[test]
    fn reads_the_system_variable_a_client_asks_for_at_startup() {
        let read = |sql: &str| {
            parse_optional_system_variable_query(sql, SessionSqlMode::default()).unwrap()
        };
        let one = |sql: &str| {
            let query = read(sql).unwrap();
            assert_eq!(query.reads().len(), 1, "{sql}");
            query.reads()[0].clone()
        };
        // The `mysql` client opens with exactly this, LIMIT and all.
        let query = one("select @@version_comment limit 1");
        assert_eq!(query.name(), "version_comment");
        assert_eq!(query.column_name(), "@@version_comment");

        for (sql, name, column, called) in [
            ("SELECT @@version", "version", "@@version", false),
            (
                "SELECT @@SESSION.version",
                "version",
                "@@SESSION.version",
                false,
            ),
            (
                "SELECT @@global.version",
                "version",
                "@@global.version",
                false,
            ),
            ("SELECT VERSION()", "version", "VERSION()", true),
            ("SELECT version ()", "version", "version ()", true),
            ("SELECT @@version AS v", "version", "v", false),
            // An alias hides the parentheses, and MySQL still answers the
            // call's own narrower NOT NULL column, so the shape is carried
            // rather than read back off the column name.
            ("SELECT VERSION() AS v", "version", "v", true),
        ] {
            let query = one(sql);
            assert_eq!(
                (query.name(), query.column_name(), query.called()),
                (name, column, called),
                "{sql}"
            );
        }

        // A driver opens the connection by reading a row of these at once.
        let query = read("SELECT @@max_allowed_packet AS m, @@wait_timeout, VERSION()").unwrap();
        assert_eq!(
            query
                .reads()
                .iter()
                .map(|read| (read.name(), read.column_name()))
                .collect::<Vec<_>>(),
            [
                ("max_allowed_packet", "m"),
                ("wait_timeout", "@@wait_timeout"),
                ("version", "VERSION()"),
            ]
        );

        // Everything else belongs to its own parser, including a LIMIT of
        // zero, which asks for no row.
        for sql in [
            "SELECT 1",
            "SELECT id FROM users",
            "SELECT @@version, 1",
            "SELECT @@version,",
            "SELECT @@version LIMIT 0",
            "",
        ] {
            assert_eq!(read(sql), None, "{sql}");
        }
    }

    /// What the `mysql` client's `status` and Django's connect read, and the
    /// other calls answering what the session knows about itself. Measured
    /// on MySQL 8.4.11: each column is named after the call as written, and
    /// `CURRENT_USER` may go without its parentheses.
    #[test]
    fn reads_the_calls_a_session_answers_about_itself() {
        let calls = |sql: &str| {
            parse_optional_system_variable_query(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap()
                .reads()
                .iter()
                .map(|read| (read.session_call(), read.column_name().to_owned()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            calls("select DATABASE(), USER() limit 1"),
            [
                (Some(MySqlSessionCall::Database), "DATABASE()".to_owned()),
                (Some(MySqlSessionCall::User), "USER()".to_owned()),
            ]
        );
        assert_eq!(
            calls("SELECT VERSION(), @@character_set_client, schema () AS s"),
            [
                (None, "VERSION()".to_owned()),
                (None, "@@character_set_client".to_owned()),
                (Some(MySqlSessionCall::Database), "s".to_owned()),
            ]
        );
        assert_eq!(
            calls(
                "SELECT CURRENT_USER, current_user(), SESSION_USER(), SYSTEM_USER(), \
                 CONNECTION_ID(), ROW_COUNT(), FOUND_ROWS()"
            ),
            [
                (
                    Some(MySqlSessionCall::CurrentUser),
                    "CURRENT_USER".to_owned()
                ),
                (
                    Some(MySqlSessionCall::CurrentUser),
                    "current_user()".to_owned()
                ),
                (Some(MySqlSessionCall::User), "SESSION_USER()".to_owned()),
                (Some(MySqlSessionCall::User), "SYSTEM_USER()".to_owned()),
                (
                    Some(MySqlSessionCall::ConnectionId),
                    "CONNECTION_ID()".to_owned()
                ),
                (Some(MySqlSessionCall::RowCount), "ROW_COUNT()".to_owned()),
                (Some(MySqlSessionCall::FoundRows), "FOUND_ROWS()".to_owned()),
            ]
        );
        // A bare name is a column, and a call with an argument is not one of
        // these.
        for sql in [
            "SELECT USER",
            "SELECT USER FROM t",
            "SELECT DATABASE",
            "SELECT USER('x')",
            "SELECT CONNECTION_ID",
        ] {
            assert_eq!(
                parse_optional_system_variable_query(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
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
            // A list, or a limit, is read with the other session calls.
            "SELECT DATABASE(), 1",
            "SELECT DATABASE() AS d, USER()",
            "SELECT DATABASE() LIMIT 1",
            "",
        ] {
            assert_eq!(column_name(sql), None, "{sql}");
        }
    }

    #[test]
    fn refuses_the_shapes_it_cannot_answer() {
        for sql in [
            // MySQL answers this; this query surface takes one column only.
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
