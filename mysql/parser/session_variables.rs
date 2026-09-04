use super::{is_unquoted_word, unsupported, ParseError, SessionMySqlDialect, SessionSqlMode};
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

/// The supported session-local `sql_notes` statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlSessionSqlNotes {
    Set(bool),
    Select { column_name: String },
}

/// Parses only a boolean assignment or a single, unaliased session variable read.
pub fn parse_optional_session_sql_notes(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlSessionSqlNotes>, ParseError> {
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
        [select, variable]
            if is_unquoted_word(select, "SELECT") && is_unquoted_word(variable, "@@sql_notes") =>
        {
            Ok(Some(MySqlSessionSqlNotes::Select {
                column_name: variable.to_string(),
            }))
        }
        [select, session, Token::Period, name]
            if is_unquoted_word(select, "SELECT")
                && is_unquoted_word(session, "@@SESSION")
                && is_unquoted_word(name, "sql_notes") =>
        {
            Ok(Some(MySqlSessionSqlNotes::Select {
                column_name: format!("{session}.{name}"),
            }))
        }
        _ => Ok(None),
    }
}

fn parse_value(value: &str) -> Result<MySqlSessionSqlNotes, ParseError> {
    match value {
        "0" => Ok(MySqlSessionSqlNotes::Set(false)),
        "1" => Ok(MySqlSessionSqlNotes::Set(true)),
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
                Some(MySqlSessionSqlNotes::Set(false))
            );
        }
        for sql in ["SELECT @@sql_notes", "SELECT @@SESSION.sql_notes;"] {
            assert!(matches!(
                parse_optional_session_sql_notes(sql, SessionSqlMode::default()).unwrap(),
                Some(MySqlSessionSqlNotes::Select { .. })
            ));
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
            "SELECT @@GLOBAL.sql_notes",
            "SELECT @@sql_notes AS notes",
            "SELECT @@sql_notes, 1",
            "SELECT @@sql_notes FROM t",
            "SELECT @@sql_notes -- comment",
            "/*x*/ SELECT @@sql_notes",
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
