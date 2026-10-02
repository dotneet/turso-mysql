use super::{
    admin_command::tokenize_lock_tables_command, consume_admin_word, skip_admin_comments,
    AdminToken, ParseError, SessionSqlMode,
};

/// `LOCK TABLES` and `UNLOCK TABLES`.
///
/// MySQL locks each table it names, and holds the lock until `UNLOCK TABLES`
/// or the next `LOCK TABLES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlLockTablesCommand {
    /// `LOCK TABLES <table> [AS <alias>] READ|WRITE [, ...]`.
    Lock(Vec<MySqlLockedTable>),
    /// `UNLOCK TABLES`.
    Unlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlLockedTable {
    pub table: String,
    pub write: bool,
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
    let tokens = tokenize_lock_tables_command(sql, mode)?;
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
    let tables = read_locked_tables(&tokens, &mut cursor)?;
    skip_one_semicolon(&tokens, &mut cursor);
    if cursor != tokens.len() {
        return Err(ParseError::Unsupported {
            feature: "LOCK TABLES option",
        });
    }
    Ok(Some(MySqlLockTablesCommand::Lock(tables)))
}

/// Reads the list of tables and the lock each was asked for.
fn read_locked_tables(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<Vec<MySqlLockedTable>, ParseError> {
    let mut tables = Vec::new();
    loop {
        let Some(table) = read_one_name(tokens, cursor) else {
            return Err(ParseError::Unsupported {
                feature: "LOCK TABLES without a table to lock",
            });
        };
        // `LOCK TABLES t AS a READ` locks the table under an alias, which
        // changes which name the session may use it by in MySQL and nothing
        // here.
        if consume_admin_word(tokens, cursor, "AS") && read_one_name(tokens, cursor).is_none() {
            return Err(ParseError::Unsupported {
                feature: "LOCK TABLES alias",
            });
        }
        // On InnoDB, `READ LOCAL` also blocks concurrent inserts. MySQL 8.4.11
        // returned 1205 for one after a one-second table lock wait, so both
        // READ spellings take the same lock.
        let write = if consume_admin_word(tokens, cursor, "READ") {
            let _ = consume_admin_word(tokens, cursor, "LOCAL");
            false
        } else if consume_admin_word(tokens, cursor, "WRITE") {
            true
        } else {
            return Err(ParseError::Unsupported {
                feature: "LOCK TABLES without READ or WRITE",
            });
        };
        tables.push(MySqlLockedTable {
            table: table.to_ascii_lowercase(),
            write,
        });
        if !matches!(tokens.get(*cursor), Some(AdminToken::Comma)) {
            return Ok(tables);
        }
        *cursor += 1;
    }
}

/// Reads one table name, qualified or not, and answers the table's own name.
fn read_one_name(tokens: &[AdminToken], cursor: &mut usize) -> Option<String> {
    let mut name = identifier_at(tokens, *cursor)?;
    *cursor += 1;
    if matches!(tokens.get(*cursor), Some(AdminToken::Dot)) {
        *cursor += 1;
        name = identifier_at(tokens, *cursor)?;
        *cursor += 1;
    }
    Some(name)
}

fn identifier_at(tokens: &[AdminToken], cursor: usize) -> Option<String> {
    match tokens.get(cursor) {
        Some(AdminToken::Word(name) | AdminToken::QuotedIdentifier(name)) => Some(name.clone()),
        _ => None,
    }
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
    fn the_shapes_that_lock_tables_are_taken_with_each_table_and_its_lock() {
        let mode = SessionSqlMode::default();
        let locked = |locks: &[(&str, bool)]| {
            MySqlLockTablesCommand::Lock(
                locks
                    .iter()
                    .map(|(table, write)| MySqlLockedTable {
                        table: table.to_string(),
                        write: *write,
                    })
                    .collect(),
            )
        };
        for (sql, command) in [
            ("LOCK TABLES records READ", locked(&[("records", false)])),
            ("LOCK TABLES records WRITE;", locked(&[("records", true)])),
            ("lock table `Records` write", locked(&[("records", true)])),
            (
                "LOCK TABLES a READ, b WRITE, reports.c READ",
                locked(&[("a", false), ("b", true), ("c", false)]),
            ),
            ("LOCK TABLES a AS x READ", locked(&[("a", false)])),
            ("LOCK TABLES a READ LOCAL", locked(&[("a", false)])),
            (
                "LOCK TABLES `records` READ /*!32311 LOCAL */",
                locked(&[("records", false)]),
            ),
            (
                "LOCK TABLES a READ /*!32311 LOCAL */, b READ /*!32311 LOCAL */",
                locked(&[("a", false), ("b", false)]),
            ),
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
            "LOCK TABLES a LOW_PRIORITY WRITE",
            "LOCK TABLES a READ /* LOCAL */",
            "LOCK TABLES a READ /*!99999 LOCAL */",
            "LOCK TABLES a READ /*!32311 WRITE */",
            "LOCK TABLES a READ /*!32311 LOCAL */; DROP TABLE a",
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
