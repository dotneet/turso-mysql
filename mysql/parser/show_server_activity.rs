//! `SHOW STATUS` and `SHOW PROCESSLIST`, which describe the server rather
//! than a database.

use super::{
    admin_command_ends, consume_admin_like_pattern, consume_admin_word,
    like_pattern::MySqlLikePattern, skip_admin_comments, tokenize_admin_command,
    MySqlVariableScope, ParseError, SessionSqlMode,
};

/// `SHOW [GLOBAL | SESSION] STATUS [LIKE 'pattern']`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowStatusCommand {
    scope: MySqlVariableScope,
    pattern: Option<MySqlLikePattern>,
}

impl MySqlShowStatusCommand {
    /// Returns the scope the command was written with.
    pub fn scope(&self) -> MySqlVariableScope {
        self.scope
    }

    /// Reports whether the command asks for the counter called `name`.
    pub fn selects(&self, name: &str) -> bool {
        self.pattern
            .as_ref()
            .is_none_or(|pattern| pattern.matches(name))
    }
}

/// Parses `SHOW STATUS`, or returns `None` for any other statement.
///
/// A `WHERE` is a predicate over the listing's two columns rather than a
/// pattern, and is refused the way `SHOW VARIABLES` refuses one.
pub fn parse_optional_show_status(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowStatusCommand>, ParseError> {
    let Ok(tokens) = tokenize_admin_command(sql, mode) else {
        return Ok(None);
    };
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    let scope = if consume_admin_word(&tokens, &mut cursor, "GLOBAL") {
        MySqlVariableScope::Global
    } else {
        let _ = consume_admin_word(&tokens, &mut cursor, "SESSION")
            || consume_admin_word(&tokens, &mut cursor, "LOCAL");
        MySqlVariableScope::Session
    };
    if !consume_admin_word(&tokens, &mut cursor, "STATUS") {
        return Ok(None);
    }
    let pattern = consume_admin_like_pattern(&tokens, &mut cursor, mode)?;
    if pattern.is_none() && consume_admin_word(&tokens, &mut cursor, "WHERE") {
        return Err(ParseError::Unsupported {
            feature: "SHOW STATUS with a WHERE",
        });
    }
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowStatusCommand { scope, pattern }))
}

/// Parses `SHOW [FULL] PROCESSLIST`, answering whether it was the `FULL`
/// form, or `None` for any other statement.
pub fn parse_optional_show_processlist(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<bool>, ParseError> {
    let Ok(tokens) = tokenize_admin_command(sql, mode) else {
        return Ok(None);
    };
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    let full = consume_admin_word(&tokens, &mut cursor, "FULL");
    if !consume_admin_word(&tokens, &mut cursor, "PROCESSLIST") {
        return Ok(None);
    }
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(full))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_show_status_in_each_scope() {
        let status = |sql: &str| {
            parse_optional_show_status(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap()
        };
        let uptime = status("SHOW GLOBAL STATUS LIKE 'Uptime'");
        assert_eq!(uptime.scope(), MySqlVariableScope::Global);
        assert!(uptime.selects("Uptime"));
        assert!(!uptime.selects("Uptime_since_flush_status"));
        let threads = status("show session status like 'Threads\\_%'");
        assert_eq!(threads.scope(), MySqlVariableScope::Session);
        assert!(threads.selects("Threads_connected"));
        assert!(status("SHOW STATUS").selects("Uptime"));
        assert!(parse_optional_show_status(
            "SHOW STATUS WHERE Variable_name = 'Uptime'",
            SessionSqlMode::default()
        )
        .is_err());
        assert_eq!(
            parse_optional_show_status("SHOW TABLE STATUS", SessionSqlMode::default()).unwrap(),
            None
        );
    }

    #[test]
    fn reads_show_processlist() {
        let full = |sql: &str| parse_optional_show_processlist(sql, SessionSqlMode::default());
        assert_eq!(full("SHOW PROCESSLIST").unwrap(), Some(false));
        assert_eq!(full("show full processlist;").unwrap(), Some(true));
        assert_eq!(full("SHOW FULL TABLES").unwrap(), None);
        assert!(full("SHOW PROCESSLIST LIKE 'x'").is_err());
    }
}
