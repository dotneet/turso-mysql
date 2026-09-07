use super::{
    consume_admin_word, skip_admin_comments, tokenize_admin_command, AdminToken, ParseError,
    SessionSqlMode,
};

/// `FLUSH TABLES` with nothing else attached.
///
/// MySQL closes its table cache here. This server keeps no table cache, so the
/// statement asks for something that has already happened, which is the one
/// shape it can be answered honestly in. Every other `FLUSH` promises something
/// particular — a read lock held across statements, reloaded grants, rotated
/// logs — and each is refused rather than answered with an OK it does not keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlFlushTablesCommand;

/// Parses the strict `FLUSH TABLES` command.
pub fn parse_flush_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlFlushTablesCommand, ParseError> {
    parse_optional_flush_tables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "FLUSH statement",
    })
}

/// Accepts `FLUSH [NO_WRITE_TO_BINLOG | LOCAL] TABLES` and an optional single
/// semicolon.
///
/// A statement beginning with another word returns `None` so its own parser
/// keeps it. One beginning with `FLUSH` and asking for anything else is
/// refused: a table list names tables to close, `WITH READ LOCK` holds a lock
/// this server has no way to hold, and the rest name things it does not keep.
pub fn parse_optional_flush_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlFlushTablesCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    if !consume_admin_word(&tokens, &mut cursor, "FLUSH") {
        return Ok(None);
    }
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in FLUSH command",
        });
    }
    // Both words say the statement is not written to the binary log, which this
    // server has none of either way.
    let _ = consume_admin_word(&tokens, &mut cursor, "NO_WRITE_TO_BINLOG")
        || consume_admin_word(&tokens, &mut cursor, "LOCAL");
    if !consume_admin_word(&tokens, &mut cursor, "TABLES") {
        return Err(ParseError::Unsupported {
            feature: "FLUSH of anything but TABLES",
        });
    }
    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    // What follows is valid MySQL asking for more than a closed table cache —
    // a table list, a read lock — so it is unsupported rather than malformed.
    if cursor != tokens.len() {
        return Err(ParseError::Unsupported {
            feature: "FLUSH TABLES option",
        });
    }
    Ok(Some(MySqlFlushTablesCommand))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_tables_is_taken_and_every_other_flush_is_not() {
        let mode = SessionSqlMode::default();
        for sql in [
            "FLUSH TABLES",
            "flush tables;",
            "FLUSH LOCAL TABLES",
            "FLUSH NO_WRITE_TO_BINLOG TABLES",
        ] {
            assert_eq!(parse_flush_tables(sql, mode), Ok(MySqlFlushTablesCommand));
        }
        for sql in [
            // Each of these promises something this server cannot keep.
            "FLUSH TABLES WITH READ LOCK",
            "FLUSH TABLES records",
            "FLUSH PRIVILEGES",
            "FLUSH LOGS",
            "FLUSH STATUS",
            "FLUSH",
            "FLUSH TABLES;;",
            "FLUSH TABLES; SELECT 1",
            "/* c */ FLUSH TABLES",
        ] {
            assert!(parse_flush_tables(sql, mode).is_err(), "{sql}");
        }
        for sql in ["SELECT 1", "SHOW TABLES", "ANALYZE TABLE records"] {
            assert_eq!(parse_optional_flush_tables(sql, mode), Ok(None), "{sql}");
        }
    }
}
