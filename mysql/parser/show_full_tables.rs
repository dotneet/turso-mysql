use super::{
    consume_admin_word, skip_admin_comments, tokenize_admin_command, AdminToken, ParseError,
    SessionSqlMode,
};

/// Lists names and object kinds in the selected database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlShowFullTablesCommand;

/// Parses the strict `SHOW FULL TABLES` command.
pub fn parse_show_full_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowFullTablesCommand, ParseError> {
    parse_optional_show_full_tables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW FULL TABLES statement",
    })
}

/// Accepts an optional single semicolon; filters, qualifiers and comments are unsupported.
pub fn parse_optional_show_full_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowFullTablesCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "SHOW")
        || !consume_admin_word(&tokens, &mut cursor, "FULL")
        || !consume_admin_word(&tokens, &mut cursor, "TABLES")
    {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in SHOW FULL TABLES command",
        });
    }
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowFullTablesCommand))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_full_tables_accepts_only_the_selected_database_form() {
        for sql in [
            "SHOW FULL TABLES",
            "show full tables;",
            " SHOW\nFULL\tTABLES ; ",
        ] {
            assert_eq!(
                parse_show_full_tables(sql, SessionSqlMode::default()),
                Ok(MySqlShowFullTablesCommand)
            );
        }
        for sql in [
            "SHOW FULL TABLES FROM app",
            "SHOW FULL TABLES IN app",
            "SHOW FULL TABLES LIKE '%'",
            "SHOW FULL TABLES WHERE TRUE",
            "SHOW FULL TABLES;;",
            "SHOW FULL TABLES; SELECT 1",
            "/* comment */ SHOW FULL TABLES",
            "SHOW FULL TABLES /* comment */",
            "SHOW /* comment */ FULL TABLES",
            "SHOW FULL TABLES -- comment\n",
        ] {
            assert!(
                parse_show_full_tables(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
        for sql in ["SHOW TABLES", "SHOW FULL COLUMNS FROM records", "SELECT 1"] {
            assert_eq!(
                parse_optional_show_full_tables(sql, SessionSqlMode::default()),
                Ok(None)
            );
        }
    }
}
