use super::*;

/// Parses one unqualified `DROP VIEW name` without optional clauses.
pub fn parse_optional_drop_view(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlTableName>, ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let sql_tokens = Tokenizer::new(&dialect, sql)
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
            .is_some_and(|token| is_unquoted_word(token, "VIEW"))
    {
        return Ok(None);
    }
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "DROP")
        || !consume_admin_word(&tokens, &mut cursor, "VIEW")
    {
        return Ok(None);
    }
    let table = consume_admin_table_name(&tokens, &mut cursor)?;
    if table.as_str().starts_with("sqlite_") || table.as_str().starts_with("__turso_internal_") {
        return Err(ParseError::Unsupported {
            feature: "internal view name",
        });
    }
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_view_accepts_only_one_unqualified_name() {
        assert_eq!(
            parse_optional_drop_view("DROP VIEW `Records`;", SessionSqlMode::default())
                .unwrap()
                .unwrap()
                .as_str(),
            "records"
        );
        for sql in [
            "DROP VIEW IF EXISTS v",
            "DROP VIEW db.v",
            "DROP VIEW a, b",
            "DROP VIEW v CASCADE",
            "DROP VIEW v; SELECT 1",
            "DROP VIEW sqlite_schema",
            "DROP VIEW `unterminated",
            "DROP VIEW 'string'",
        ] {
            assert!(
                parse_optional_drop_view(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
        assert_eq!(
            parse_optional_drop_view("DROP TABLE records", SessionSqlMode::default()).unwrap(),
            None
        );
    }

    #[test]
    fn drop_view_does_not_parse_backticks_inside_other_statements_strings() {
        for sql in [
            "SELECT '`'",
            "INSERT INTO records (label) VALUES ('`')",
            "SELECT 'DROP VIEW `'",
        ] {
            assert_eq!(
                parse_optional_drop_view(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
    }
}
