use super::{
    consume_admin_qualified_table_name, consume_admin_word, is_unquoted_word, skip_admin_comments,
    tokenize_admin_command, AdminToken, MySqlDatabaseName, MySqlTableName, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};
use crate::statement_reads;
use sqlparser::tokenizer::Token;

/// One checked `DROP TABLE` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlDropTableCommand {
    tables: Vec<MySqlTableName>,
    databases: Vec<MySqlDatabaseName>,
    if_exists: bool,
}

impl MySqlDropTableCommand {
    /// Returns the canonical table names targeted by the command, without
    /// the database a name was qualified by, in the order it named them.
    /// Laravel's `migrate:fresh` names every table in one statement.
    pub fn tables(&self) -> &[MySqlTableName] {
        &self.tables
    }

    /// Returns the databases the command's qualified names name, each once.
    /// Laravel's `migrate:fresh` qualifies every table it drops,
    /// `laravel`.`cache`.
    pub fn databases(&self) -> &[MySqlDatabaseName] {
        &self.databases
    }

    /// Returns whether the command used `IF EXISTS`.
    pub const fn if_exists(&self) -> bool {
        self.if_exists
    }
}

/// Parses one strict `DROP TABLE` command.
///
/// An optional single semicolon is accepted. A name may be qualified by its
/// database, which the caller holds to the one it runs in. Comments and every
/// clause other than `IF EXISTS` are rejected once the statement starts with
/// `DROP TABLE`.
pub fn parse_optional_drop_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlDropTableCommand>, ParseError> {
    let sql_tokens =
        statement_reads::tokens(&SessionMySqlDialect::without_executable_comments(mode), sql)
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
    let mut tables = Vec::new();
    let mut databases = Vec::new();
    loop {
        let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor).map_err(
            |error| match error {
                // `mysql`, `information_schema` and the other names a database
                // here may not take are MySQL's own, whose tables are not
                // dropped through this.
                ParseError::InvalidDatabaseName { .. } => ParseError::Unsupported {
                    feature: "DROP TABLE qualified by a system database",
                },
                error => error,
            },
        )?;
        if let Some(database) = database {
            if !databases.contains(&database) {
                databases.push(database);
            }
        }
        if table.as_str().starts_with("sqlite_") || table.as_str().starts_with("__turso_internal_")
        {
            return Err(ParseError::Unsupported {
                feature: "internal table name",
            });
        }
        tables.push(table);
        if !matches!(tokens.get(cursor), Some(AdminToken::Comma)) {
            break;
        }
        cursor += 1;
    }
    // Measured on MySQL 8.4.11: one of these may close the statement and
    // changes nothing — `DROP TABLE parent CASCADE` still answers 3730 while
    // a child's foreign key names it — and writing both is 1064.
    let _ = consume_admin_word(&tokens, &mut cursor, "RESTRICT")
        || consume_admin_word(&tokens, &mut cursor, "CASCADE");
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlDropTableCommand {
        tables,
        databases,
        if_exists,
    }))
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
            assert_eq!(command.tables()[0].as_str(), table);
            assert_eq!(command.if_exists(), if_exists);
        }
        let command =
            parse_optional_drop_table("drop table `users`,`posts`", SessionSqlMode::default())
                .unwrap()
                .unwrap();
        assert_eq!(
            command
                .tables()
                .iter()
                .map(MySqlTableName::as_str)
                .collect::<Vec<_>>(),
            ["users", "posts"]
        );
        // GORM drops a table this way.
        for sql in [
            "DROP TABLE IF EXISTS `users` CASCADE",
            "DROP TABLE users, posts RESTRICT;",
        ] {
            let command = parse_optional_drop_table(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap();
            assert_eq!(command.tables()[0].as_str(), "users", "{sql}");
        }
    }

    /// Laravel's `migrate:fresh` qualifies every table by its database.
    #[test]
    fn drop_table_reads_a_name_qualified_by_its_database() {
        let command = parse_optional_drop_table(
            "drop table `laravel`.`cache`, `Laravel`.`jobs`, users, other.t",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            command
                .tables()
                .iter()
                .map(MySqlTableName::as_str)
                .collect::<Vec<_>>(),
            ["cache", "jobs", "users", "t"]
        );
        assert_eq!(
            command
                .databases()
                .iter()
                .map(MySqlDatabaseName::as_str)
                .collect::<Vec<_>>(),
            ["laravel", "other"]
        );
    }

    #[test]
    fn drop_table_rejects_clauses_names_and_comments() {
        for sql in [
            "DROP TABLE IF x",
            "DROP TABLE IF NOT EXISTS x",
            "DROP TABLE db.",
            "DROP TABLE a,",
            "DROP TABLE a.b.c",
            "DROP TABLE mysql.user",
            "DROP TABLE x CASCADE RESTRICT",
            "DROP TABLE x RESTRICT CASCADE",
            "DROP TABLE x CASCADE, y",
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
