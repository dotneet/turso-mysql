use super::{
    consume_admin_word, skip_admin_comments, tokenize_admin_command, AdminToken, ParseError,
    SessionSqlMode,
};

/// Lists the storage engines this server offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlShowEnginesCommand;

/// Parses the strict `SHOW ENGINES` command.
pub fn parse_show_engines(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowEnginesCommand, ParseError> {
    parse_optional_show_engines(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW ENGINES statement",
    })
}

/// Accepts an optional single semicolon; `STORAGE`, filters and comments are
/// unsupported.
pub fn parse_optional_show_engines(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowEnginesCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "SHOW")
        || !consume_admin_word(&tokens, &mut cursor, "ENGINES")
    {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in SHOW ENGINES command",
        });
    }
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowEnginesCommand))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Option<MySqlShowEnginesCommand> {
        parse_optional_show_engines(sql, SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn reads_show_engines_and_leaves_its_neighbours_alone() {
        for sql in ["SHOW ENGINES", "show engines", "SHOW ENGINES;"] {
            assert_eq!(parse(sql), Some(MySqlShowEnginesCommand), "{sql}");
        }
        for sql in ["SHOW TABLES", "SHOW ENGINE INNODB STATUS", "SELECT 1", ""] {
            assert_eq!(parse(sql), None, "{sql}");
        }
        for sql in [
            // `SHOW STORAGE ENGINES` is MySQL's older spelling; it is not read
            // here, and neither is anything trailing.
            "SHOW ENGINES LIKE 'InnoDB'",
            "SHOW ENGINES;;",
            "/* hidden */ SHOW ENGINES",
        ] {
            assert!(
                parse_optional_show_engines(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
