//! `SHOW EVENTS`, `SHOW FUNCTION STATUS` and `SHOW PROCEDURE STATUS`, which
//! list stored programs, of which this server keeps none.

use super::{
    admin_command::AdminToken, admin_command_ends, consume_admin_database_name,
    consume_admin_like_pattern, consume_admin_word, skip_admin_comments, tokenize_admin_command,
    MySqlDatabaseName, ParseError, SessionSqlMode,
};

/// Which stored programs a `SHOW` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlStoredProgramKind {
    /// `SHOW EVENTS`, which lists the selected database's events.
    Events,
    /// `SHOW FUNCTION STATUS`, which lists every database's stored functions.
    Functions,
    /// `SHOW PROCEDURE STATUS`, which lists every database's stored
    /// procedures.
    Procedures,
}

/// A `SHOW` listing stored programs.
///
/// Only the filters that can be answered without reading a row are taken:
/// none, `LIKE 'pattern'`, and for the routines the `WHERE Db = 'name'`
/// `mysqldump` writes. Any other `WHERE` is a predicate over the listing's
/// columns, which MySQL checks — an unknown column is 1054 — and is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowStoredProgramsCommand {
    kind: MySqlStoredProgramKind,
    database: Option<MySqlDatabaseName>,
}

impl MySqlShowStoredProgramsCommand {
    pub fn kind(&self) -> MySqlStoredProgramKind {
        self.kind
    }

    /// The database `SHOW EVENTS FROM name` named, if it named one.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }
}

/// Parses a `SHOW` listing stored programs, or returns `None` for any other
/// statement.
pub fn parse_optional_show_stored_programs(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowStoredProgramsCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    let kind = if consume_admin_word(&tokens, &mut cursor, "EVENTS") {
        MySqlStoredProgramKind::Events
    } else if consume_admin_word(&tokens, &mut cursor, "FUNCTION")
        && consume_admin_word(&tokens, &mut cursor, "STATUS")
    {
        MySqlStoredProgramKind::Functions
    } else if consume_admin_word(&tokens, &mut cursor, "PROCEDURE")
        && consume_admin_word(&tokens, &mut cursor, "STATUS")
    {
        MySqlStoredProgramKind::Procedures
    } else {
        return Ok(None);
    };
    let database = if kind == MySqlStoredProgramKind::Events
        && (consume_admin_word(&tokens, &mut cursor, "FROM")
            || consume_admin_word(&tokens, &mut cursor, "IN"))
    {
        Some(consume_admin_database_name(&tokens, &mut cursor)?)
    } else {
        None
    };
    if consume_admin_like_pattern(&tokens, &mut cursor, mode)?.is_none()
        && consume_admin_word(&tokens, &mut cursor, "WHERE")
        && (kind == MySqlStoredProgramKind::Events || !consume_database_test(&tokens, &mut cursor))
    {
        return Err(ParseError::Unsupported {
            feature: "SHOW of stored programs with a WHERE other than Db = 'name'",
        });
    }
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowStoredProgramsCommand { kind, database }))
}

/// Reads `Db = 'name'`, the one `WHERE` `mysqldump` writes.
fn consume_database_test(tokens: &[AdminToken], cursor: &mut usize) -> bool {
    let names_the_database = matches!(
        tokens.get(*cursor),
        Some(AdminToken::Word(name) | AdminToken::QuotedIdentifier(name))
            if name.eq_ignore_ascii_case("Db")
    );
    if !names_the_database
        || tokens.get(*cursor + 1) != Some(&AdminToken::Equals)
        || !matches!(tokens.get(*cursor + 2), Some(AdminToken::StringLiteral(_)))
    {
        return false;
    }
    *cursor += 3;
    // Connector/J's `getProcedures` and `getFunctions` without
    // information_schema add the name pattern, `AND Name LIKE '%'`.
    let names_the_name = matches!(
        tokens.get(*cursor + 1),
        Some(AdminToken::Word(name) | AdminToken::QuotedIdentifier(name))
            if name.eq_ignore_ascii_case("Name")
    );
    if matches!(tokens.get(*cursor), Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case("AND"))
        && names_the_name
        && matches!(tokens.get(*cursor + 2), Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case("LIKE"))
        && matches!(tokens.get(*cursor + 3), Some(AdminToken::StringLiteral(_)))
    {
        *cursor += 4;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(sql: &str) -> Option<MySqlStoredProgramKind> {
        parse_optional_show_stored_programs(sql, SessionSqlMode::default())
            .unwrap()
            .map(|command| command.kind())
    }

    /// What `mysqldump --routines --events` sends, and the other spellings
    /// MySQL takes.
    #[test]
    fn reads_what_mysqldump_sends() {
        assert_eq!(kind("show events"), Some(MySqlStoredProgramKind::Events));
        assert_eq!(
            kind("SHOW FUNCTION STATUS WHERE Db = 'probe'"),
            Some(MySqlStoredProgramKind::Functions)
        );
        assert_eq!(
            kind("SHOW PROCEDURE STATUS WHERE Db = 'probe'"),
            Some(MySqlStoredProgramKind::Procedures)
        );
        // What Connector/J sends for `getFunctions` and `getProcedures`
        // with `useInformationSchema=false`.
        assert_eq!(
            kind("SHOW FUNCTION STATUS WHERE Db = 'dbtools' AND Name LIKE '%'"),
            Some(MySqlStoredProgramKind::Functions)
        );
        assert_eq!(
            kind("SHOW PROCEDURE STATUS WHERE Db = 'dbtools' AND Name LIKE '%'"),
            Some(MySqlStoredProgramKind::Procedures)
        );
        assert_eq!(
            kind("SHOW PROCEDURE STATUS LIKE 'p%'"),
            Some(MySqlStoredProgramKind::Procedures)
        );
        assert_eq!(
            kind("SHOW FUNCTION STATUS"),
            Some(MySqlStoredProgramKind::Functions)
        );
        let from = parse_optional_show_stored_programs(
            "SHOW EVENTS FROM `probe` LIKE 'e%'",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            from.database().map(MySqlDatabaseName::as_str),
            Some("probe")
        );
    }

    #[test]
    fn leaves_every_other_statement_to_its_own_parser() {
        for sql in ["SHOW TABLES", "SHOW FUNCTION", "SHOW CREATE FUNCTION f", ""] {
            assert_eq!(kind(sql), None, "{sql}");
        }
    }

    #[test]
    fn refuses_a_filter_that_needs_the_listing_read() {
        for sql in [
            "SHOW FUNCTION STATUS WHERE Name = 'f'",
            "SHOW FUNCTION STATUS WHERE Db = 'probe' AND Name = 'f'",
            "SHOW EVENTS WHERE Db = 'probe'",
            "SHOW PROCEDURE STATUS FROM probe",
        ] {
            assert!(
                parse_optional_show_stored_programs(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
