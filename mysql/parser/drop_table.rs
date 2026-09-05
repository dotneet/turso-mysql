use super::{
    consume_admin_table_name, consume_admin_word, is_unquoted_word, skip_admin_comments,
    tokenize_admin_command, AdminToken, MySqlTableName, ParseError, SessionMySqlDialect,
    SessionSqlMode,
};
use sqlparser::tokenizer::{Token, Tokenizer};

/// One checked `DROP TABLE` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlDropTableCommand {
    table: MySqlTableName,
    if_exists: bool,
}

impl MySqlDropTableCommand {
    /// Returns the canonical unqualified table name targeted by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns whether the command used `IF EXISTS`.
    pub const fn if_exists(&self) -> bool {
        self.if_exists
    }
}

/// Parses one strict, unqualified `DROP TABLE` command.
///
/// An optional single semicolon is accepted. Comments, qualified names,
/// multiple names, and every clause other than `IF EXISTS` are rejected once
/// the statement starts with `DROP TABLE`.
pub fn parse_optional_drop_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlDropTableCommand>, ParseError> {
    let sql_tokens = Tokenizer::new(&SessionMySqlDialect::without_executable_comments(mode), sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let mut words = sql_tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)));
    if !words
        .next()
        .is_some_and(|token| is_unquoted_word(token, "DROP"))
        || !words
            .next()
            .is_some_and(|token| is_unquoted_word(token, "TABLE"))
    {
        return Ok(None);
    }

    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "DROP")
        || !consume_admin_word(&tokens, &mut cursor, "TABLE")
    {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in DROP TABLE command",
        });
    }

    let if_exists = if consume_admin_word(&tokens, &mut cursor, "IF") {
        if !consume_admin_word(&tokens, &mut cursor, "EXISTS") {
            return Err(ParseError::ExpectedAdminCommand);
        }
        true
    } else {
        false
    };
    let table = consume_admin_table_name(&tokens, &mut cursor)?;
    if table.as_str().starts_with("sqlite_") || table.as_str().starts_with("__turso_internal_") {
        return Err(ParseError::Unsupported {
            feature: "internal table name",
        });
    }
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlDropTableCommand { table, if_exists }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_table_accepts_one_unqualified_name_and_if_exists() {
        for (sql, table, if_exists) in [
            ("DROP TABLE records", "records", false),
            ("drop\ttable\nIF\tEXISTS `Records`;", "records", true),
        ] {
            let command = parse_optional_drop_table(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap();
            assert_eq!(command.table().as_str(), table);
            assert_eq!(command.if_exists(), if_exists);
        }
    }

    #[test]
    fn drop_table_rejects_clauses_names_and_comments() {
        for sql in [
            "DROP TABLE IF x",
            "DROP TABLE IF NOT EXISTS x",
            "DROP TABLE db.x",
            "DROP TABLE a, b",
            "DROP TABLE x CASCADE",
            "DROP TABLE x RESTRICT",
            "DROP TABLE x; SELECT 1",
            "DROP TABLE;;",
            "DROP TABLE sqlite_schema",
            "DROP TABLE __turso_internal_seq_x",
            "/* comment */ DROP TABLE x",
            "DROP TABLE x /* comment */",
            "DROP TABLE `unterminated",
        ] {
            assert!(
                parse_optional_drop_table(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn drop_table_does_not_parse_unrelated_sql_containing_drop_text() {
        for sql in [
            "SELECT '`'",
            "INSERT INTO records (label) VALUES ('`')",
            "SELECT 'DROP TABLE `'",
            "SELECT 'DROP TABLE records'",
            "SELECT \"DROP TABLE records\"",
            "INSERT INTO records (label) VALUES ('DROP TABLE records')",
            "SELECT '`DROP TABLE records`'",
            "DROP /* comment */ TABLE records",
            "`DROP TABLE records`",
        ] {
            assert_eq!(
                parse_optional_drop_table(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
        assert_eq!(
            parse_optional_drop_table(
                "SELECT \"DROP TABLE records\"",
                SessionSqlMode {
                    ansi_quotes: true,
                    no_backslash_escapes: false,
                }
            )
            .unwrap(),
            None
        );
        assert_eq!(
            parse_optional_drop_table("DROP VIEW records", SessionSqlMode::default()).unwrap(),
            None
        );
    }
}
