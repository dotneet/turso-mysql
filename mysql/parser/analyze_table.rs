use super::{
    consume_admin_table_name, consume_admin_word, skip_admin_comments, tokenize_admin_command,
    AdminToken, MySqlTableName, ParseError, SessionSqlMode,
};

/// One maintenance statement naming a single table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlAnalyzeTableCommand {
    table: MySqlTableName,
}

impl MySqlAnalyzeTableCommand {
    /// Returns the table the statement names.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }
}

/// Verifies that one table's storage reads back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlCheckTableCommand {
    table: MySqlTableName,
}

impl MySqlCheckTableCommand {
    /// Returns the table being checked.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }
}

/// Parses the strict `CHECK TABLE` command.
pub fn parse_check_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlCheckTableCommand, ParseError> {
    parse_optional_check_table(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "CHECK TABLE statement",
    })
}

/// Accepts one unqualified table name and an optional single semicolon.
///
/// The `FOR UPGRADE`, `QUICK`, `FAST`, `MEDIUM`, `EXTENDED` and `CHANGED`
/// options are unsupported, and so is a list of tables.
pub fn parse_optional_check_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlCheckTableCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "CHECK")
        || !consume_admin_word(&tokens, &mut cursor, "TABLE")
    {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in CHECK TABLE command",
        });
    }
    let table = consume_admin_table_name(&tokens, &mut cursor)?;
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlCheckTableCommand { table }))
}

/// Parses the strict `ANALYZE TABLE` command.
pub fn parse_analyze_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlAnalyzeTableCommand, ParseError> {
    parse_optional_analyze_table(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "ANALYZE TABLE statement",
    })
}

/// Accepts one unqualified table name and an optional single semicolon.
///
/// `NO_WRITE_TO_BINLOG`, `LOCAL`, a list of tables, the histogram forms and
/// comments are unsupported.
pub fn parse_optional_analyze_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAnalyzeTableCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "ANALYZE")
        || !consume_admin_word(&tokens, &mut cursor, "TABLE")
    {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in ANALYZE TABLE command",
        });
    }
    let table = consume_admin_table_name(&tokens, &mut cursor)?;
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlAnalyzeTableCommand { table }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Option<MySqlAnalyzeTableCommand> {
        parse_optional_analyze_table(sql, SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn reads_check_table_and_leaves_its_neighbours_alone() {
        for sql in ["CHECK TABLE t", "check table `t`", "CHECK TABLE t;"] {
            assert_eq!(
                parse_optional_check_table(sql, SessionSqlMode::default())
                    .unwrap()
                    .map(|command| command.table().as_str().to_owned()),
                Some("t".to_owned()),
                "{sql}"
            );
        }
        for sql in ["CHECK", "ANALYZE TABLE t", "SELECT 1", ""] {
            assert_eq!(
                parse_optional_check_table(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
        for sql in [
            "CHECK TABLE a, b",
            "CHECK TABLE app.t",
            "CHECK TABLE t FOR UPGRADE",
            "CHECK TABLE t QUICK",
        ] {
            assert!(
                parse_optional_check_table(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn reads_analyze_table_and_leaves_its_neighbours_alone() {
        for sql in ["ANALYZE TABLE t", "analyze table `t`", "ANALYZE TABLE t;"] {
            assert_eq!(
                parse(sql).map(|command| command.table().as_str().to_owned()),
                Some("t".to_owned()),
                "{sql}"
            );
        }
        for sql in [
            "ANALYZE",
            "OPTIMIZE TABLE t",
            "SELECT 1",
            "",
            // The second word is not TABLE, so this parser does not claim it.
            "ANALYZE NO_WRITE_TO_BINLOG TABLE t",
        ] {
            assert_eq!(parse(sql), None, "{sql}");
        }
        for sql in [
            // One table at a time, and none of the options.
            "ANALYZE TABLE a, b",
            "ANALYZE TABLE app.t",
            "ANALYZE TABLE t UPDATE HISTOGRAM ON n",
            "/* hidden */ ANALYZE TABLE t",
        ] {
            assert!(
                parse_optional_analyze_table(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
