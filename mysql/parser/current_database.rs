//! The selected database, where a statement names it.
//!
//! Rails, Django and Laravel each find out whether a table is there by
//! filtering `information_schema` on `table_schema = DATABASE()` — Laravel
//! spells it `schema()`. The engine has no notion of a logical database, but
//! the session knows which one is selected, and that name is exactly what the
//! call answers. So the call is written into the query as that name before the
//! query is read, which leaves the checked `SELECT` path to compare a column
//! against a written word, as it already does.
//!
//! Prisma and TypeORM write the selected database before a table name, and
//! Prisma before every column name too — `prisma.users`, `prisma.users.id`.
//! Those name the same table and column the bare names do, so the database is
//! left out before the statement is read.

use crate::statement_reads;
use sqlparser::ast::Statement;
use sqlparser::tokenizer::{Location, Span, Token, TokenWithSpan};

use super::{
    named_tables::database_qualifiers_in, parse_one_statement, ParseError, SessionMySqlDialect,
    SessionSqlMode,
};

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
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
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

/// Leaves the selected database out of a `SELECT`, `INSERT`, `UPDATE` or
/// `DELETE` wherever it is written before a table it reads or writes, or
/// before a table and a column.
///
/// Answers `None` when there is nothing to leave out, and also when leaving it
/// out would change what the statement answers: a projected expression other
/// than a column is named after its text as written — measured on MySQL
/// 8.4.11, `SELECT COUNT(probe.users.id)` answers a column named
/// `COUNT(probe.users.id)` — so a statement writing the database inside one is
/// left as written, for the checked path to refuse. Another database is left
/// as written too: the engine reads the selected database alone.
pub fn leave_out_the_current_database(
    sql: &str,
    database: Option<&str>,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let Some(database) = database else {
        return Ok(None);
    };
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let names_the_database = |token: &Token| matches!(token, Token::Word(word) if word.value.eq_ignore_ascii_case(database));
    if !words
        .windows(2)
        .any(|pair| names_the_database(&pair[0].token) && matches!(pair[1].token, Token::Period))
    {
        return Ok(None);
    }
    let Ok(statement) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if !matches!(
        statement,
        Statement::Query(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_)
    ) {
        return Ok(None);
    }
    let qualifiers = database_qualifiers_in(&statement);
    let mut cuts = Vec::new();
    for (qualifier, table) in &qualifiers.tables {
        if qualifier.value.eq_ignore_ascii_case(database) {
            cuts.push((qualifier.span.start, table.span.start));
        }
    }
    let is_period = |token: Option<&&TokenWithSpan>| {
        token.is_some_and(|token| matches!(token.token, Token::Period))
    };
    // A column is the only name written in three parts.
    let mut column_cuts = Vec::new();
    for (at, name) in words.windows(5).enumerate() {
        let [first, _, second, _, third] = name else {
            unreachable!("a window of five holds five tokens");
        };
        if names_the_database(&first.token)
            && matches!(name[1].token, Token::Period)
            && matches!(second.token, Token::Word(_))
            && matches!(name[3].token, Token::Period)
            && matches!(third.token, Token::Word(_) | Token::Mul)
            && !is_period(at.checked_sub(1).and_then(|before| words.get(before)))
            && !is_period(words.get(at + 5))
        {
            column_cuts.push((first.span.start, second.span.start));
        }
    }
    if column_cuts.iter().any(|(start, _)| {
        qualifiers
            .named_after_their_text
            .iter()
            .any(|span| *span == Span::empty() || (span.start <= *start && *start <= span.end))
    }) {
        return Ok(None);
    }
    cuts.extend(column_cuts);
    if cuts.is_empty() {
        return Ok(None);
    }
    cuts.sort();
    cuts.dedup();
    let mut left_out = String::with_capacity(sql.len());
    let mut copied_up_to = 0;
    for (start, end) in cuts {
        let (start, end) = (byte_offset_of(sql, start)?, byte_offset_of(sql, end)?);
        left_out.push_str(&sql[copied_up_to..start]);
        copied_up_to = end;
    }
    left_out.push_str(&sql[copied_up_to..]);
    Ok(Some(left_out))
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

    fn left_out(sql: &str) -> Option<String> {
        leave_out_the_current_database(sql, Some("app"), SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn leaves_the_selected_database_out_of_table_and_column_names() {
        for (sql, expected) in [
            (
                "SELECT * FROM `app`.`migrations` `migrations` ORDER BY `id` DESC",
                "SELECT * FROM `migrations` `migrations` ORDER BY `id` DESC",
            ),
            (
                "SELECT `app`.`users`.`id` FROM `app`.`users` WHERE (`app`.`users`.`email` = ? AND 1=1) LIMIT ? OFFSET ?",
                "SELECT `users`.`id` FROM `users` WHERE (`users`.`email` = ? AND 1=1) LIMIT ? OFFSET ?",
            ),
            (
                "SELECT APP.users.* FROM App.users JOIN (SELECT app.posts.user_id FROM app.posts) AS p ON p.user_id = app . users . id",
                "SELECT users.* FROM users JOIN (SELECT posts.user_id FROM posts) AS p ON p.user_id = users . id",
            ),
            (
                "INSERT IGNORE INTO `app`.`tags` (`name`) VALUES (?), (?)",
                "INSERT IGNORE INTO `tags` (`name`) VALUES (?), (?)",
            ),
            (
                "UPDATE `app`.`users` SET `app`.`users`.`name` = ? WHERE `app`.`users`.`id` = ?",
                "UPDATE `users` SET `users`.`name` = ? WHERE `users`.`id` = ?",
            ),
            (
                "DELETE FROM `app`.`users` WHERE `app`.`users`.`id` = ?",
                "DELETE FROM `users` WHERE `users`.`id` = ?",
            ),
            (
                "SELECT COUNT(app.users.id) AS n FROM users",
                "SELECT COUNT(users.id) AS n FROM users",
            ),
        ] {
            assert_eq!(left_out(sql).as_deref(), Some(expected), "{sql}");
        }
    }

    #[test]
    fn leaves_a_name_that_would_read_differently_as_written() {
        for sql in [
            // Another database, a table alias spelled like the database, and
            // a string.
            "SELECT id FROM other.users",
            "SELECT app.id FROM users app",
            "SELECT 'app.users.id' FROM users",
            // A result column named after the text as written.
            "SELECT COUNT(app.users.id) FROM users",
            "SELECT app.users.id + 1 FROM app.users",
            // A qualified name never reads a `WITH` name.
            "WITH users AS (SELECT 1 AS id) SELECT id FROM app.users",
            // Only statements that read or write rows.
            "DESCRIBE app.users",
            "DROP TABLE app.users",
        ] {
            assert_eq!(left_out(sql), None, "{sql}");
        }
        assert_eq!(
            leave_out_the_current_database(
                "SELECT id FROM app.users",
                None,
                SessionSqlMode::default()
            )
            .unwrap(),
            None
        );
    }
}
