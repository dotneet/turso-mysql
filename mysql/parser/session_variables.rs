use super::{is_unquoted_word, unsupported, ParseError, SessionMySqlDialect, SessionSqlMode};
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

/// Parses `SET [SESSION] sql_notes = 0|1`, answering what it asks for.
///
/// A read of the variable belongs to the system-variable reader, which answers
/// every scope and alias MySQL takes rather than the one bare spelling this
/// once took.
pub fn parse_optional_session_sql_notes(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<bool>, ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let tokens = tokens
        .iter()
        .filter(|token| {
            !matches!(
                token,
                Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)
            )
        })
        .collect::<Vec<_>>();
    let tokens = tokens.strip_suffix(&[&Token::SemiColon]).unwrap_or(&tokens);
    match tokens {
        [set, name, Token::Eq, Token::Number(value, false)]
            if is_unquoted_word(set, "SET") && is_unquoted_word(name, "sql_notes") =>
        {
            parse_value(value).map(Some)
        }
        [set, session, name, Token::Eq, Token::Number(value, false)]
            if is_unquoted_word(set, "SET")
                && is_unquoted_word(session, "SESSION")
                && is_unquoted_word(name, "sql_notes") =>
        {
            parse_value(value).map(Some)
        }
        _ => Ok(None),
    }
}

fn parse_value(value: &str) -> Result<bool, ParseError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => unsupported("sql_notes value; expected 0 or 1"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_checked_session_forms() {
        for sql in [
            "SET sql_notes = 0",
            "set SESSION SQL_NOTES=0;",
            "\nSET\tsql_notes=0\n",
        ] {
            assert_eq!(
                parse_optional_session_sql_notes(sql, SessionSqlMode::default()).unwrap(),
                Some(false)
            );
        }
    }

    #[test]
    fn does_not_accept_other_scopes_comments_or_extra_statements() {
        for sql in [
            "SET GLOBAL sql_notes=0",
            "SET @@session.sql_notes=0",
            "SET sql_notes=ON",
            "SET sql_notes=2",
            "SET `sql_notes`=0",
            "SET sql_notes='0'",
            "SET sql_notes=0; SELECT 1",
            "SET sql_notes=0;;",
            "SET /*x*/ sql_notes=0",
            "/*! SET sql_notes=0 */",
            "SELECT @@sql_notes",
            "SELECT @@SESSION.sql_notes;",
            "SELECT @@GLOBAL.sql_notes",
            "SELECT @@sql_notes AS notes",
        ] {
            assert!(
                !matches!(
                    parse_optional_session_sql_notes(sql, SessionSqlMode::default()),
                    Ok(Some(_))
                ),
                "{sql}"
            );
        }
    }
}
