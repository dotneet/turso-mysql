use super::{
    admin_command_ends, consume_admin_database_name, consume_admin_like_pattern,
    consume_admin_table_name, consume_admin_word, like_pattern::MySqlLikePattern,
    skip_admin_comments, tokenize_admin_command, MySqlDatabaseName, MySqlTableName, ParseError,
    SessionSqlMode,
};

/// The tables whose triggers `SHOW TRIGGERS` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowTriggersCommand {
    database: Option<MySqlDatabaseName>,
    pattern: Option<MySqlLikePattern>,
}

impl MySqlShowTriggersCommand {
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }

    pub fn pattern(&self) -> Option<&MySqlLikePattern> {
        self.pattern.as_ref()
    }
}

/// The trigger named by `SHOW CREATE TRIGGER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowCreateTriggerCommand {
    name: MySqlTableName,
}

impl MySqlShowCreateTriggerCommand {
    pub fn name(&self) -> &MySqlTableName {
        &self.name
    }
}

pub fn parse_optional_show_triggers(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowTriggersCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW")
        || !consume_admin_word(&tokens, &mut cursor, "TRIGGERS")
    {
        return Ok(None);
    }
    if cursor != 2 {
        return Err(ParseError::ExpectedAdminCommand);
    }
    let database = if consume_admin_word(&tokens, &mut cursor, "FROM")
        || consume_admin_word(&tokens, &mut cursor, "IN")
    {
        Some(consume_admin_database_name(&tokens, &mut cursor)?)
    } else {
        None
    };
    let pattern = consume_admin_like_pattern(&tokens, &mut cursor, mode)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowTriggersCommand { database, pattern }))
}

pub fn parse_optional_show_create_trigger(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowCreateTriggerCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW")
        || !consume_admin_word(&tokens, &mut cursor, "CREATE")
        || !consume_admin_word(&tokens, &mut cursor, "TRIGGER")
    {
        return Ok(None);
    }
    if cursor != 3 {
        return Err(ParseError::ExpectedAdminCommand);
    }
    let name = consume_admin_table_name(&tokens, &mut cursor)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowCreateTriggerCommand { name }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysqldump_trigger_queries_parse_without_accepting_extra_statements() {
        let mode = SessionSqlMode::default();
        let listing = parse_optional_show_triggers("SHOW TRIGGERS LIKE 'dump\\_records'", mode)
            .unwrap()
            .unwrap();
        assert!(listing
            .pattern()
            .unwrap()
            .matches_keeping_case("dump_records"));
        assert!(!listing
            .pattern()
            .unwrap()
            .matches_keeping_case("dumpXrecords"));
        assert_eq!(listing.database(), None);
        let created = parse_optional_show_create_trigger("SHOW CREATE TRIGGER `dump_copy`", mode)
            .unwrap()
            .unwrap();
        assert_eq!(created.name().as_str(), "dump_copy");
        for sql in [
            "SHOW TRIGGERS LIKE x",
            "SHOW TRIGGERS WHERE Trigger = 'x'",
            "SHOW CREATE TRIGGER",
            "SHOW CREATE TRIGGER t; SELECT 1",
        ] {
            let parsed = if sql.starts_with("SHOW CREATE") {
                parse_optional_show_create_trigger(sql, mode).map(|result| result.is_some())
            } else {
                parse_optional_show_triggers(sql, mode).map(|result| result.is_some())
            };
            assert!(parsed.is_err(), "{sql}");
        }
    }
}
