use super::{
    consume_admin_word, skip_admin_comments, tokenize_admin_command, AdminToken, ParseError,
    SessionSqlMode,
};

/// Describes the tables in the selected database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlShowTableStatusCommand;

/// Parses the strict `SHOW TABLE STATUS` command.
pub fn parse_show_table_status(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowTableStatusCommand, ParseError> {
    parse_optional_show_table_status(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW TABLE STATUS statement",
    })
}

/// Accepts an optional single semicolon; `FROM`, `LIKE`, `WHERE` and comments
/// are unsupported.
pub fn parse_optional_show_table_status(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowTableStatusCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "SHOW")
        || !consume_admin_word(&tokens, &mut cursor, "TABLE")
        || !consume_admin_word(&tokens, &mut cursor, "STATUS")
    {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in SHOW TABLE STATUS command",
        });
    }
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowTableStatusCommand))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Option<MySqlShowTableStatusCommand> {
        parse_optional_show_table_status(sql, SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn reads_show_table_status_and_leaves_its_neighbours_alone() {
        for sql in ["SHOW TABLE STATUS", "show table status", "SHOW TABLE STATUS;"] {
            assert_eq!(parse(sql), Some(MySqlShowTableStatusCommand), "{sql}");
        }
        for sql in ["SHOW TABLES", "SHOW STATUS", "SELECT 1", ""] {
            assert_eq!(parse(sql), None, "{sql}");
        }
        for sql in [
            "SHOW TABLE STATUS FROM probe",
            "SHOW TABLE STATUS LIKE 't'",
            "SHOW TABLE STATUS WHERE Name = 't'",
            "/* hidden */ SHOW TABLE STATUS",
        ] {
            assert!(
                parse_optional_show_table_status(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
