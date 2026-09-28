use super::{
    consume_admin_database_name, consume_admin_word, like_pattern::MySqlLikePattern,
    skip_admin_comments, tokenize_admin_command, AdminToken, MySqlDatabaseName, ParseError,
    SessionSqlMode,
};

/// Describes the tables in the selected database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowTableStatusCommand {
    database: Option<MySqlDatabaseName>,
    pattern: Option<MySqlLikePattern>,
    name: Option<String>,
}

impl MySqlShowTableStatusCommand {
    /// Returns the `FROM database` the command was written with, if any.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }

    /// Returns the pattern the command names its tables with, if any.
    ///
    /// Measured on MySQL 8.4.11: `SHOW TABLE STATUS LIKE 'b%'` reports the
    /// tables whose names it matches, and one nothing matches answers no rows.
    pub fn pattern(&self) -> Option<&MySqlLikePattern> {
        self.pattern.as_ref()
    }

    /// Reports whether the command asks for the table called `table`.
    ///
    /// Measured on MySQL 8.4.11: `WHERE Name = 'users'`, which TablePlus
    /// sends, compares the name by its bytes — `'USERS'` finds nothing — where
    /// `LIKE` matches without regard to case.
    pub fn covers(&self, table: &str) -> bool {
        self.pattern
            .as_ref()
            .is_none_or(|pattern| pattern.matches(table))
            && self.name.as_deref().is_none_or(|name| name == table)
    }
}

/// Parses the strict `SHOW TABLE STATUS` command.
pub fn parse_show_table_status(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowTableStatusCommand, ParseError> {
    parse_optional_show_table_status(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW TABLE STATUS statement",
    })
}

/// Accepts a `FROM` or `IN` database, a `LIKE` pattern or `WHERE Name =
/// 'table'`, and an optional single semicolon; any other `WHERE` and comments
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
    // MySQL spells the qualifier either way round, and means the same thing.
    let database = if consume_admin_word(&tokens, &mut cursor, "FROM")
        || consume_admin_word(&tokens, &mut cursor, "IN")
    {
        Some(consume_admin_database_name(&tokens, &mut cursor)?)
    } else {
        None
    };
    let pattern = if consume_admin_word(&tokens, &mut cursor, "LIKE") {
        let Some(AdminToken::StringLiteral(pattern)) = tokens.get(cursor) else {
            return Err(ParseError::ExpectedAdminCommand);
        };
        cursor += 1;
        Some(MySqlLikePattern::new(pattern, mode))
    } else {
        None
    };
    let name = if pattern.is_none() && consume_admin_word(&tokens, &mut cursor, "WHERE") {
        let names_the_name = matches!(
            tokens.get(cursor),
            Some(AdminToken::Word(column) | AdminToken::QuotedIdentifier(column))
                if column.eq_ignore_ascii_case("Name")
        );
        match (
            names_the_name,
            tokens.get(cursor + 1),
            tokens.get(cursor + 2),
        ) {
            (true, Some(AdminToken::Equals), Some(AdminToken::StringLiteral(name))) => {
                cursor += 3;
                Some(name.clone())
            }
            _ => {
                return Err(ParseError::Unsupported {
                    feature: "SHOW TABLE STATUS with a WHERE other than Name = 'table'",
                })
            }
        }
    } else {
        None
    };
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowTableStatusCommand {
        database,
        pattern,
        name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Option<MySqlShowTableStatusCommand> {
        parse_optional_show_table_status(sql, SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn reads_show_table_status_and_leaves_its_neighbours_alone() {
        for sql in [
            "SHOW TABLE STATUS",
            "show table status",
            "SHOW TABLE STATUS;",
        ] {
            let command = parse(sql).unwrap_or_else(|| panic!("{sql}"));
            assert_eq!(command.database(), None, "{sql}");
            assert!(command.pattern().is_none(), "{sql}");
        }
        for sql in ["SHOW TABLES", "SHOW STATUS", "SELECT 1", ""] {
            assert_eq!(parse(sql), None, "{sql}");
        }

        // Measured on MySQL 8.4.11: the qualifier is spelled either way round
        // and the pattern names the tables to report.
        for sql in ["SHOW TABLE STATUS FROM probe", "SHOW TABLE STATUS IN probe"] {
            let command = parse(sql).unwrap_or_else(|| panic!("{sql}"));
            assert_eq!(
                command.database().map(MySqlDatabaseName::as_str),
                Some("probe"),
                "{sql}"
            );
            assert!(command.pattern().is_none(), "{sql}");
        }
        let filtered = parse("SHOW TABLE STATUS FROM probe LIKE 'b%'").unwrap();
        assert_eq!(
            filtered.database().map(MySqlDatabaseName::as_str),
            Some("probe")
        );
        let pattern = filtered.pattern().unwrap();
        assert!(pattern.matches("beta"));
        assert!(!pattern.matches("alpha"));

        let named = parse("SHOW TABLE STATUS FROM `dbtools` WHERE Name = 'users'").unwrap();
        assert!(named.covers("users"));
        assert!(!named.covers("USERS"));
        assert!(!named.covers("posts"));

        for sql in [
            "SHOW TABLE STATUS WHERE Engine = 'InnoDB'",
            "SHOW TABLE STATUS WHERE Name LIKE 't'",
            "SHOW TABLE STATUS LIKE 't' WHERE Name = 't'",
            "SHOW TABLE STATUS LIKE t",
            "SHOW TABLE STATUS LIKE",
            "SHOW TABLE STATUS FROM",
            "SHOW TABLE STATUS LIKE 't' extra",
            "/* hidden */ SHOW TABLE STATUS",
        ] {
            assert!(
                parse_optional_show_table_status(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
