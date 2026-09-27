use super::*;

/// One checked `DROP VIEW` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlDropViewCommand {
    views: Vec<MySqlTableName>,
    if_exists: bool,
}

impl MySqlDropViewCommand {
    /// Returns the canonical unqualified view names, in the order the command
    /// named them.
    pub fn views(&self) -> &[MySqlTableName] {
        &self.views
    }

    /// Returns whether the command used `IF EXISTS`.
    pub const fn if_exists(&self) -> bool {
        self.if_exists
    }
}

/// Parses one `DROP VIEW [IF EXISTS] name [, name] ... [RESTRICT | CASCADE]`.
///
/// Measured on MySQL 8.4.11, `RESTRICT` and `CASCADE` are read and change
/// nothing. A name qualified by its database is refused.
pub fn parse_optional_drop_view(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlDropViewCommand>, ParseError> {
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
    let if_exists = if consume_admin_word(&tokens, &mut cursor, "IF") {
        if !consume_admin_word(&tokens, &mut cursor, "EXISTS") {
            return Err(ParseError::ExpectedAdminCommand);
        }
        true
    } else {
        false
    };
    let mut views = Vec::new();
    loop {
        let view = consume_admin_table_name(&tokens, &mut cursor)?;
        if view.as_str().starts_with("sqlite_") || view.as_str().starts_with("__turso_internal_") {
            return Err(ParseError::Unsupported {
                feature: "internal view name",
            });
        }
        views.push(view);
        if !matches!(tokens.get(cursor), Some(AdminToken::Comma)) {
            break;
        }
        cursor += 1;
    }
    if !consume_admin_word(&tokens, &mut cursor, "RESTRICT") {
        consume_admin_word(&tokens, &mut cursor, "CASCADE");
    }
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlDropViewCommand { views, if_exists }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_view_reads_its_names_and_if_exists() {
        let command = parse_optional_drop_view("DROP VIEW `Records`;", SessionSqlMode::default())
            .unwrap()
            .unwrap();
        assert_eq!(command.views()[0].as_str(), "records");
        assert!(!command.if_exists());
        let command = parse_optional_drop_view(
            "drop view if exists a, `B` cascade",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            command
                .views()
                .iter()
                .map(MySqlTableName::as_str)
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(command.if_exists());
        assert!(
            parse_optional_drop_view("DROP VIEW v RESTRICT", SessionSqlMode::default())
                .unwrap()
                .is_some()
        );
        for sql in [
            "DROP VIEW db.v",
            "DROP VIEW IF v",
            "DROP VIEW v RESTRICT CASCADE",
            "DROP VIEW v; SELECT 1",
            "DROP VIEW sqlite_schema",
            "DROP VIEW `unterminated",
            "DROP VIEW 'string'",
            "DROP VIEW a,",
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
