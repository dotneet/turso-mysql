use super::{
    consume_admin_table_name, consume_admin_word, is_unquoted_word, skip_admin_comments,
    tokenize_admin_command, AdminToken, MySqlTableName, ParseError, SessionMySqlDialect,
    SessionSqlMode,
};
use sqlparser::tokenizer::{Token, Tokenizer};

/// One checked `TRUNCATE TABLE` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlTruncateTableCommand {
    table: MySqlTableName,
}

impl MySqlTruncateTableCommand {
    /// Returns the canonical unqualified table name targeted by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }
}

/// Parses one strict, unqualified `TRUNCATE TABLE` command.
///
/// MySQL takes the `TABLE` keyword as optional, so both spellings are read. An
/// optional single semicolon is accepted. Comments, qualified names, and
/// anything after the table name are rejected once the statement starts with
/// `TRUNCATE`.
pub fn parse_optional_truncate_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlTruncateTableCommand>, ParseError> {
    let sql_tokens = Tokenizer::new(&SessionMySqlDialect::without_executable_comments(mode), sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let mut words = sql_tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)));
    if !words
        .next()
        .is_some_and(|token| is_unquoted_word(token, "TRUNCATE"))
    {
        return Ok(None);
    }
    // `TRUNCATE(x, 2)` is MySQL's rounding function, not the statement.
    if words.next().is_some_and(|token| *token == Token::LParen) {
        return Ok(None);
    }

    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "TRUNCATE") {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in TRUNCATE TABLE command",
        });
    }
    consume_admin_word(&tokens, &mut cursor, "TABLE");

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
    Ok(Some(MySqlTruncateTableCommand { table }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_table_accepts_one_unqualified_name_with_or_without_the_keyword() {
        for sql in [
            "TRUNCATE TABLE records",
            "TRUNCATE records",
            "truncate\ttable\n`Records`;",
        ] {
            let command = parse_optional_truncate_table(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap();
            assert_eq!(command.table().as_str(), "records", "{sql}");
        }
    }

    #[test]
    fn truncate_table_rejects_clauses_names_and_comments() {
        for sql in [
            "TRUNCATE TABLE db.x",
            "TRUNCATE TABLE a, b",
            "TRUNCATE TABLE x RESTART IDENTITY",
            "TRUNCATE TABLE x; SELECT 1",
            "TRUNCATE TABLE;;",
            "TRUNCATE TABLE sqlite_schema",
            "TRUNCATE TABLE __turso_internal_seq_x",
            "/* comment */ TRUNCATE TABLE x",
            "TRUNCATE TABLE x /* comment */",
            "TRUNCATE TABLE `unterminated",
        ] {
            assert!(
                parse_optional_truncate_table(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn truncate_table_does_not_parse_unrelated_sql_containing_truncate_text() {
        for sql in [
            "SELECT TRUNCATE(1.5, 0)",
            "TRUNCATE(1.5, 0)",
            "SELECT 'TRUNCATE TABLE records'",
            "SELECT \"TRUNCATE TABLE records\"",
            "INSERT INTO records (label) VALUES ('TRUNCATE TABLE records')",
            "SELECT '`TRUNCATE TABLE records`'",
            "`TRUNCATE TABLE records`",
            "DROP TABLE records",
        ] {
            assert_eq!(
                parse_optional_truncate_table(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
        assert_eq!(
            parse_optional_truncate_table(
                "SELECT \"TRUNCATE TABLE records\"",
                SessionSqlMode {
                    ansi_quotes: true,
                    no_backslash_escapes: false,
                }
            )
            .unwrap(),
            None
        );
    }
}
