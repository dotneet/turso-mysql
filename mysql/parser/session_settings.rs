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
    /// `SET NAMES <charset> [COLLATE <collation>]`, with what it named.
    Names {
        character_set: String,
        collation: Option<String>,
    },
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
    } else {
        return Ok(None);
    };
    if !scanner.at_end() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(setting))
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
