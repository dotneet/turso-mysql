//! `DATABASE()` inside an `information_schema` query.
//!
//! Rails, Django and Laravel each find out whether a table is there by
//! filtering `information_schema` on `table_schema = DATABASE()` — Laravel
//! spells it `schema()`. The engine has no notion of a logical database, but
//! the session knows which one is selected, and that name is exactly what the
//! call answers. So the call is written into the query as that name before the
//! query is read, which leaves the checked `SELECT` path to compare a column
//! against a written word, as it already does.

use sqlparser::tokenizer::{Location, Token, Tokenizer};

use super::{ParseError, SessionMySqlDialect, SessionSqlMode};

/// Writes the selected database's name, or `NULL` when none is selected, in
/// place of every `DATABASE()` and `SCHEMA()` after a `FROM` in a query that
/// reads `information_schema`.
///
/// Answers `None` when there is nothing to write. A call in a projection is
/// left alone: MySQL names the result column after the call as written, and
/// the session answers `SELECT DATABASE()` on its own.
pub fn write_the_current_database_in(
    sql: &str,
    database: Option<&str>,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize_with_location()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    if !words.iter().any(|token| {
        matches!(&token.token, Token::Word(word)
            if word.value.eq_ignore_ascii_case("information_schema"))
    }) {
        return Ok(None);
    }
    let name = match database {
        Some(database) => format!("'{}'", database.replace('\'', "''")),
        None => "NULL".to_owned(),
    };
    let mut replaced = String::with_capacity(sql.len());
    let mut copied_up_to = 0;
    let mut after_from = false;
    for (at, token) in words.iter().enumerate() {
        if is_unquoted_word(&token.token, "FROM") {
            after_from = true;
            continue;
        }
        if !after_from
            || !(is_unquoted_word(&token.token, "DATABASE")
                || is_unquoted_word(&token.token, "SCHEMA"))
            || !matches!(
                words.get(at + 1).map(|token| &token.token),
                Some(Token::LParen)
            )
            || !matches!(
                words.get(at + 2).map(|token| &token.token),
                Some(Token::RParen)
            )
        {
            continue;
        }
        let start = byte_offset_of(sql, token.span.start)?;
        let end = byte_offset_of(sql, words[at + 2].span.end)?;
        replaced.push_str(&sql[copied_up_to..start]);
        replaced.push_str(&name);
        copied_up_to = end;
    }
    if copied_up_to == 0 {
        return Ok(None);
    }
    replaced.push_str(&sql[copied_up_to..]);
    Ok(Some(replaced))
}

fn is_unquoted_word(token: &Token, expected: &str) -> bool {
    matches!(token, Token::Word(word)
        if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
}

/// The byte offset one line-and-column location stands at.
fn byte_offset_of(sql: &str, location: Location) -> Result<usize, ParseError> {
    let (mut line, mut column) = (1, 1);
    for (offset, character) in sql.char_indices() {
        if line == location.line && column == location.column {
            return Ok(offset);
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    if line == location.line && column == location.column {
        return Ok(sql.len());
    }
    Err(ParseError::Sqlparser(
        "a token's location lies outside the statement".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(sql: &str, database: Option<&str>) -> Option<String> {
        write_the_current_database_in(sql, database, SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn writes_the_selected_database_where_a_query_filters_on_it() {
        assert_eq!(
            written(
                "SELECT table_name FROM information_schema.tables WHERE table_schema = database() AND table_name = 'users'",
                Some("app"),
            )
            .unwrap(),
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'app' AND table_name = 'users'"
        );
        // Laravel's spelling, inside a subquery, with space before the brackets.
        assert_eq!(
            written(
                "select exists (select 1 from information_schema.tables where table_schema = schema ( ) and table_name = 'migrations') as `exists`",
                Some("o'brien"),
            )
            .unwrap(),
            "select exists (select 1 from information_schema.tables where table_schema = 'o''brien' and table_name = 'migrations') as `exists`"
        );
        assert_eq!(
            written(
                "SELECT table_name FROM information_schema.tables WHERE table_schema = DATABASE()",
                None,
            )
            .unwrap(),
            "SELECT table_name FROM information_schema.tables WHERE table_schema = NULL"
        );
    }

    #[test]
    fn leaves_everything_else_as_it_was_written() {
        for sql in [
            // No information_schema.
            "SELECT id FROM users WHERE db = DATABASE()",
            // A projection names its column after the call.
            "SELECT DATABASE()",
            // A word in a string or a quoted name is not the call.
            "SELECT table_name FROM information_schema.tables WHERE table_name = 'DATABASE()'",
            "SELECT `database` FROM information_schema.tables",
        ] {
            assert_eq!(written(sql, Some("app")), None, "{sql}");
        }
    }
}
