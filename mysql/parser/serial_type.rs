//! `SERIAL`, MySQL's short name for a counted key column.
//!
//! Drizzle keeps its migrations in `id serial primary key`. MySQL documents
//! `SERIAL` as `BIGINT UNSIGNED NOT NULL AUTO_INCREMENT UNIQUE`, and measured
//! on 8.4.11 that is what it makes: `SHOW CREATE TABLE` prints the column as
//! `bigint unsigned NOT NULL AUTO_INCREMENT` with `PRIMARY KEY` and a
//! `UNIQUE KEY` named after the column beside it, the same as for the long
//! spelling.

use crate::statement_reads;
use sqlparser::ast::{DataType, Statement};
use sqlparser::tokenizer::Token;

use super::{
    current_database::byte_offset_of, mentions_ignoring_case, parse_one_statement, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};

const WRITTEN_OUT: &str = "BIGINT UNSIGNED NOT NULL AUTO_INCREMENT UNIQUE";

/// Writes `SERIAL` out in full wherever a `CREATE TABLE` declares a column
/// with it.
///
/// Answers `None` when the statement declares none, and when the word stands
/// anywhere else in the statement too, which is left for the checked path to
/// read or refuse.
pub fn write_serial_out(sql: &str, mode: SessionSqlMode) -> Result<Option<String>, ParseError> {
    if !mentions_ignoring_case(sql, "serial") {
        return Ok(None);
    }
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    let declared = table
        .columns
        .iter()
        .filter(|column| {
            matches!(&column.data_type, DataType::Custom(name, modifiers)
                if modifiers.is_empty()
                    && matches!(name.0.as_slice(), [part] if part.as_ident().is_some_and(|ident| ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("serial"))))
        })
        .count();
    if declared == 0 {
        return Ok(None);
    }
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let places = tokens
        .iter()
        .filter(|token| {
            matches!(&token.token, Token::Word(word)
                if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("serial"))
        })
        .map(|token| (token.span.start, token.span.end))
        .collect::<Vec<_>>();
    if places.len() != declared {
        return Ok(None);
    }
    let mut written = String::with_capacity(sql.len() + places.len() * WRITTEN_OUT.len());
    let mut copied_up_to = 0;
    for (start, end) in places {
        let (start, end) = (byte_offset_of(sql, start)?, byte_offset_of(sql, end)?);
        written.push_str(&sql[copied_up_to..start]);
        written.push_str(WRITTEN_OUT);
        copied_up_to = end;
    }
    written.push_str(&sql[copied_up_to..]);
    Ok(Some(written))
}
