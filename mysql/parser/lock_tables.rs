use super::{
    consume_admin_word, skip_admin_comments, tokenize_admin_command, AdminToken, ParseError,
    SessionSqlMode,
};

/// `LOCK TABLES` and `UNLOCK TABLES`.
///
/// MySQL locks each table it names, and holds the lock until `UNLOCK TABLES`
/// or the next `LOCK TABLES`. This server holds one write lock over the whole
/// database, so it locks more than was asked for rather than less — which is
/// why the tables the statement names are read and then not kept: locking any
/// of them locks all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlLockTablesCommand {
    /// `LOCK TABLES <table> [AS <alias>] READ|WRITE [, ...]`.
    Lock,
    /// `UNLOCK TABLES`.
    Unlock,
}

/// Parses `LOCK TABLES` or `UNLOCK TABLES`, or nothing for another statement.
///
/// A statement beginning with another word returns `None` so its own parser
/// keeps it. One beginning with `LOCK` or `UNLOCK` and asking for anything
/// else is refused rather than answered with an OK: `LOCK INSTANCE FOR BACKUP`
/// and `LOCK TABLES ... LOW_PRIORITY WRITE` each name something this server
/// does not have.
pub fn parse_optional_lock_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlLockTablesCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    let unlocking = if consume_admin_word(&tokens, &mut cursor, "UNLOCK") {
        true
    } else if consume_admin_word(&tokens, &mut cursor, "LOCK") {
        false
    } else {
        return Ok(None);
    };
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in LOCK TABLES command",
        });
    }
    if !consume_admin_word(&tokens, &mut cursor, "TABLES")
        && !consume_admin_word(&tokens, &mut cursor, "TABLE")
    {
        return Err(ParseError::Unsupported {
            feature: "LOCK of anything but TABLES",
        });
    }
    if unlocking {
        skip_one_semicolon(&tokens, &mut cursor);
        if cursor != tokens.len() {
            return Err(ParseError::Unsupported {
                feature: "UNLOCK TABLES option",
            });
        }
        return Ok(Some(MySqlLockTablesCommand::Unlock));
    }
    read_locked_tables(&tokens, &mut cursor)?;
    skip_one_semicolon(&tokens, &mut cursor);
    if cursor != tokens.len() {
        return Err(ParseError::Unsupported {
            feature: "LOCK TABLES option",
        });
    }
    Ok(Some(MySqlLockTablesCommand::Lock))
}

/// Reads the list of tables and the lock each was asked for.
///
/// The names are read and let go: one lock covers every table, so which of
/// them were named changes nothing. Reading them is still what tells a
/// malformed statement from one this server answers.
fn read_locked_tables(tokens: &[AdminToken], cursor: &mut usize) -> Result<(), ParseError> {
    loop {
        if !read_one_name(tokens, cursor) {
            return Err(ParseError::Unsupported {
                feature: "LOCK TABLES without a table to lock",
            });
        }
        // `LOCK TABLES t AS a READ` locks the table under an alias, which
        // changes which name the session may use it by in MySQL and nothing
        // here.
        if consume_admin_word(tokens, cursor, "AS") && !read_one_name(tokens, cursor) {
            return Err(ParseError::Unsupported {
                feature: "LOCK TABLES alias",
            });
        }
        // `READ LOCAL` lets other sessions insert while the lock is held,
        // which one write lock cannot do. `LOW_PRIORITY WRITE` changes who
        // waits for whom, which needs a queue this has none of.
        if consume_admin_word(tokens, cursor, "READ") {
            if consume_admin_word(tokens, cursor, "LOCAL") {
                return Err(ParseError::Unsupported {
                    feature: "LOCK TABLES READ LOCAL",
                });
            }
        } else if !consume_admin_word(tokens, cursor, "WRITE") {
            return Err(ParseError::Unsupported {
                feature: "LOCK TABLES without READ or WRITE",
            });
        }
        if !matches!(tokens.get(*cursor), Some(AdminToken::Comma)) {
            return Ok(());
        }
        *cursor += 1;
    }
}

/// Reads one table name, qualified or not, and reports whether it found one.
fn read_one_name(tokens: &[AdminToken], cursor: &mut usize) -> bool {
    if !matches!(
        tokens.get(*cursor),
        Some(AdminToken::Word(_) | AdminToken::QuotedIdentifier(_))
    ) {
        return false;
    }
    *cursor += 1;
    if matches!(tokens.get(*cursor), Some(AdminToken::Dot)) {
        *cursor += 1;
        if !matches!(
            tokens.get(*cursor),
            Some(AdminToken::Word(_) | AdminToken::QuotedIdentifier(_))
        ) {
            return false;
        }
        *cursor += 1;
    }
    true
}

fn skip_one_semicolon(tokens: &[AdminToken], cursor: &mut usize) {
    if matches!(tokens.get(*cursor), Some(AdminToken::Semicolon)) {
        *cursor += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shapes_that_lock_every_table_are_taken_and_the_rest_are_not() {
        let mode = SessionSqlMode::default();
        for (sql, command) in [
            ("LOCK TABLES records READ", MySqlLockTablesCommand::Lock),
            ("LOCK TABLES records WRITE;", MySqlLockTablesCommand::Lock),
            ("lock table `records` write", MySqlLockTablesCommand::Lock),
            (
                "LOCK TABLES a READ, b WRITE, reports.c READ",
                MySqlLockTablesCommand::Lock,
            ),
            ("LOCK TABLES a AS x READ", MySqlLockTablesCommand::Lock),
            ("UNLOCK TABLES", MySqlLockTablesCommand::Unlock),
            ("unlock tables;", MySqlLockTablesCommand::Unlock),
        ] {
            assert_eq!(
                parse_optional_lock_tables(sql, mode),
                Ok(Some(command)),
                "{sql}"
            );
        }
        for sql in [
            // Each of these asks for something one write lock cannot answer.
            "LOCK TABLES a READ LOCAL",
            "LOCK TABLES a LOW_PRIORITY WRITE",
            "LOCK INSTANCE FOR BACKUP",
            "LOCK TABLES",
            "LOCK TABLES a",
            "LOCK TABLES a READ, ",
            "UNLOCK TABLES a",
            "LOCK TABLES a READ; SELECT 1",
            "/* c */ LOCK TABLES a READ",
        ] {
            assert!(parse_optional_lock_tables(sql, mode).is_err(), "{sql}");
        }
        for sql in ["SELECT 1", "SHOW TABLES", "FLUSH TABLES"] {
            assert_eq!(parse_optional_lock_tables(sql, mode), Ok(None), "{sql}");
        }
    }
}
