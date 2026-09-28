//! `VALUES(tbl.col)` in an `ON DUPLICATE KEY UPDATE`, which names the column
//! the way a bare `VALUES(col)` does.
//!
//! Drizzle qualifies every column it writes, `values(`tags`.`name`)` among
//! them. Measured on MySQL 8.4.11: `VALUES(tg.name)` writes what
//! `VALUES(name)` writes, and a table other than the one the statement
//! inserts into is 1054.

use crate::statement_reads;
use sqlparser::ast::{ObjectNamePart, Statement, TableObject};
use sqlparser::tokenizer::Token;

use super::{
    current_database::byte_offset_of, mentions_ignoring_case, parse_one_statement, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};

/// Leaves the table out of each `VALUES(tbl.col)` where `tbl` is the table
/// the statement inserts into.
///
/// Answers `None` when there is none, and when the statement is not an
/// `INSERT ... ON DUPLICATE KEY UPDATE` into one unqualified table.
pub fn leave_out_the_table_in_values_calls(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    if !mentions_ignoring_case(sql, "values") || !mentions_ignoring_case(sql, "duplicate") {
        return Ok(None);
    }
    let Ok(Statement::Insert(insert)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if insert.on.is_none() {
        return Ok(None);
    }
    let TableObject::TableName(name) = &insert.table else {
        return Ok(None);
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return Ok(None);
    };
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    // Only the clause after `ON DUPLICATE KEY UPDATE` reads the offered row;
    // the `VALUES` before it is the rows themselves.
    let Some(clause) = words.iter().position(|token| {
        matches!(&token.token, Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("DUPLICATE"))
    }) else {
        return Ok(None);
    };
    let mut cuts = Vec::new();
    for window in words[clause..].windows(6) {
        let [call, open, qualifier, period, column, close] = window else {
            unreachable!("a window of six holds six tokens");
        };
        let names_the_table = matches!(&qualifier.token, Token::Word(word)
            if word.value.eq_ignore_ascii_case(&table.value));
        if matches!(&call.token, Token::Word(word)
                if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("VALUES"))
            && matches!(open.token, Token::LParen)
            && names_the_table
            && matches!(period.token, Token::Period)
            && matches!(column.token, Token::Word(_))
            && matches!(close.token, Token::RParen)
        {
            cuts.push((qualifier.span.start, column.span.start));
        }
    }
    if cuts.is_empty() {
        return Ok(None);
    }
    let mut written = String::with_capacity(sql.len());
    let mut copied_up_to = 0;
    for (start, end) in cuts {
        let (start, end) = (byte_offset_of(sql, start)?, byte_offset_of(sql, end)?);
        written.push_str(&sql[copied_up_to..start]);
        copied_up_to = end;
    }
    written.push_str(&sql[copied_up_to..]);
    Ok(Some(written))
}
