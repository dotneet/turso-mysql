//! `FROM DUAL`, the table MySQL lets a `SELECT` of written values name.
//!
//! Drizzle checks its connection with `select 1 from dual`. Measured on MySQL
//! 8.4.11, `SELECT 1 FROM DUAL` answers what `SELECT 1` answers — the same
//! column, named `1`, a `LONGLONG` of 2 flagged `NOT NULL`, `BINARY` and
//! `NUM` — and so do its forms with `WHERE`, `ORDER BY` and `LIMIT`. `DUAL`
//! is a reserved word there, so only the unquoted spelling names it; a quoted
//! `` `dual` `` is an ordinary table, 1146 when there is none.

use crate::statement_reads;
use sqlparser::ast::{SetExpr, Statement, TableFactor};
use sqlparser::tokenizer::Token;

use super::{
    current_database::byte_offset_of, mentions_ignoring_case, parse_one_statement, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};

/// Leaves `FROM DUAL` out of a `SELECT` that reads no other table.
///
/// Answers `None` when the statement does not name `DUAL` that way, and when
/// it names it anywhere but as the one table of the outermost `SELECT`.
pub fn leave_out_from_dual(sql: &str, mode: SessionSqlMode) -> Result<Option<String>, ParseError> {
    if !mentions_ignoring_case(sql, "dual") {
        return Ok(None);
    }
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let places = words
        .windows(2)
        .filter(|pair| {
            is_unquoted_word(&pair[0].token, "FROM") && is_unquoted_word(&pair[1].token, "DUAL")
        })
        .map(|pair| (pair[0].span.start, pair[1].span.end))
        .collect::<Vec<_>>();
    let [(start, end)] = places.as_slice() else {
        return Ok(None);
    };
    let Ok(Statement::Query(query)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    let [table] = select.from.as_slice() else {
        return Ok(None);
    };
    let names_dual = matches!(&table.relation, TableFactor::Table { name, alias: None, args: None, with_hints, .. }
        if with_hints.is_empty()
            && matches!(name.0.as_slice(), [part] if part.as_ident().is_some_and(|ident| ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("dual"))));
    if !names_dual || !table.joins.is_empty() {
        return Ok(None);
    }
    let (start, end) = (byte_offset_of(sql, *start)?, byte_offset_of(sql, *end)?);
    Ok(Some(format!("{}{}", &sql[..start], &sql[end..])))
}

/// Whether `sql` is an `INSERT` or `REPLACE` of written rows and nothing
/// else — no `SELECT` as its source and no `ON DUPLICATE KEY UPDATE`.
pub fn inserts_written_values_only(sql: &str, mode: SessionSqlMode) -> bool {
    let Ok(Statement::Insert(insert)) = parse_one_statement(sql, mode) else {
        return false;
    };
    insert.on.is_none()
        && insert
            .source
            .as_ref()
            .is_some_and(|source| matches!(source.body.as_ref(), SetExpr::Values(_)))
}

fn is_unquoted_word(token: &Token, expected: &str) -> bool {
    matches!(token, Token::Word(word)
        if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
}
