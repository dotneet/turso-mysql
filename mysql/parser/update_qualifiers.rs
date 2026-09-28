//! Columns an `UPDATE` of one table writes with the table's name in front.
//!
//! Drizzle qualifies every column it writes —
//! `update `users` set `profile` = JSON_SET(`users`.`profile`, '$.city',
//! 'Kyoto') where `users`.`id` = 2`. An `UPDATE` of one table under its own
//! name, reading no other table, reads every column off that one table, so
//! `users.profile` names what `profile` names there.

use crate::statement_reads;
use sqlparser::ast::{ObjectNamePart, Statement, TableFactor};
use sqlparser::tokenizer::Token;

use super::{
    current_database::byte_offset_of, mentions_ignoring_case, parse_one_statement, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};

/// Leaves the table's name out of each column an `UPDATE` of one table
/// writes qualified by it.
///
/// Answers `None` when there is none to leave out, and for any statement but
/// an `UPDATE` of one table named without an alias and reading nothing else —
/// no join, no `FROM` and no subquery, where a name could stand for another
/// table's column.
pub fn leave_out_the_table_in_an_update(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    if !mentions_ignoring_case(sql, "update") || mentions_ignoring_case(sql, "select") {
        return Ok(None);
    }
    let Ok(Statement::Update(update)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if update.from.is_some() || !update.table.joins.is_empty() {
        return Ok(None);
    }
    let TableFactor::Table {
        name, alias: None, ..
    } = &update.table.relation
    else {
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
    let mut cuts = Vec::new();
    for (at, window) in words.windows(3).enumerate() {
        let [qualifier, period, column] = window else {
            unreachable!("a window of three holds three tokens");
        };
        let after_a_period = at > 0 && matches!(words[at - 1].token, Token::Period);
        if !after_a_period
            && matches!(&qualifier.token, Token::Word(word)
                if word.value.eq_ignore_ascii_case(&table.value))
            && matches!(period.token, Token::Period)
            && matches!(column.token, Token::Word(_))
            && !matches!(
                words.get(at + 3).map(|token| &token.token),
                Some(Token::Period)
            )
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
