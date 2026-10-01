//! Columns of one `information_schema` table written with the table's name,
//! or the schema's and the table's, in front.
//!
//! drizzle-kit reads the indexes of a database as `select * from
//! INFORMATION_SCHEMA.STATISTICS WHERE INFORMATION_SCHEMA.STATISTICS.TABLE_SCHEMA
//! = 'drizzle' and INFORMATION_SCHEMA.STATISTICS.INDEX_NAME != 'PRIMARY'`. A
//! `SELECT` reading one table, and nothing else, reads every column off that
//! table, so the qualified name names what the bare one does.

use crate::statement_reads;
use sqlparser::ast::{SetExpr, Statement, TableFactor};
use sqlparser::tokenizer::Token;

use super::{
    current_database::byte_offset_of, mentions_ignoring_case, read_one_statement, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};

/// Leaves `information_schema.T.` and `T.` out of each column a `SELECT`
/// reading the one `information_schema` table `T` names after its `FROM`.
///
/// Answers `None` when there is none to leave out, and for any statement but
/// a `SELECT` whose only table is `information_schema.T` named without an
/// alias, with no join and no subquery.
pub fn leave_out_the_catalog_table_in_its_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    if !mentions_ignoring_case(sql, "information_schema") {
        return Ok(None);
    }
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Query(query)) = read_statement.as_ref() else {
        return Ok(None);
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    let [from] = select.from.as_slice() else {
        return Ok(None);
    };
    if !from.joins.is_empty() {
        return Ok(None);
    }
    let TableFactor::Table {
        name, alias: None, ..
    } = &from.relation
    else {
        return Ok(None);
    };
    let [schema, table] = name.0.as_slice() else {
        return Ok(None);
    };
    let (Some(schema), Some(table)) = (schema.as_ident(), table.as_ident()) else {
        return Ok(None);
    };
    if !schema.value.eq_ignore_ascii_case("information_schema") {
        return Ok(None);
    }
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    // One `SELECT` and one `FROM`: a subquery would bring names of its own.
    let count = |keyword: &str| {
        words
            .iter()
            .filter(|token| {
                matches!(&token.token, Token::Word(word)
                if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(keyword))
            })
            .count()
    };
    if count("SELECT") != 1 || count("FROM") != 1 {
        return Ok(None);
    }
    let names = |token: &Token, expected: &str| matches!(token, Token::Word(word) if word.value.eq_ignore_ascii_case(expected));
    let Some(from_at) = words.iter().position(|token| {
        matches!(&token.token, Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("FROM"))
    }) else {
        return Ok(None);
    };
    // Only the clauses after the table are read that way: a projected
    // expression is named after its text as written, which leaving the
    // table's name out would change.
    let mut cuts = Vec::new();
    let mut at = from_at + 1;
    while at + 2 < words.len() {
        // The table named after `FROM` is the table itself, not a column.
        if at == from_at + 1 {
            at += 3;
            continue;
        }
        let starts_a_name = at == 0 || !matches!(words[at - 1].token, Token::Period);
        let schema_first = starts_a_name
            && at + 4 < words.len()
            && names(&words[at].token, &schema.value)
            && matches!(words[at + 1].token, Token::Period)
            && names(&words[at + 2].token, &table.value)
            && matches!(words[at + 3].token, Token::Period)
            && matches!(words[at + 4].token, Token::Word(_));
        if schema_first {
            cuts.push((words[at].span.start, words[at + 4].span.start));
            at += 5;
            continue;
        }
        let table_first = starts_a_name
            && names(&words[at].token, &table.value)
            && matches!(words[at + 1].token, Token::Period)
            && matches!(words[at + 2].token, Token::Word(_) | Token::Mul)
            && !matches!(
                words.get(at + 3).map(|token| &token.token),
                Some(Token::Period)
            );
        if table_first {
            cuts.push((words[at].span.start, words[at + 2].span.start));
            at += 3;
            continue;
        }
        at += 1;
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
