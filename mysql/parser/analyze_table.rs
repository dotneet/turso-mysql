use super::{
    consume_admin_table_name, consume_admin_word, skip_admin_comments, tokenize_admin_command,
    AdminToken, MySqlTableName, ParseError, SessionSqlMode,
};

/// Refreshes the planner's statistics for one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlAnalyzeTableCommand {
    table: MySqlTableName,
}

impl MySqlAnalyzeTableCommand {
    /// Returns the table the statistics are for.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }
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
