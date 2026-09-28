//! The alias an `UPDATE` gives the one table it changes.
//!
//! Hibernate writes every bulk JPQL update with an alias,
//! `update posts p1_0 set views=(p1_0.views+1) where p1_0.views<5`. Measured on
//! MySQL 8.4.11 that changes the rows the statement without the alias changes,
//! so the alias is taken out and each column written through it is written
//! bare before the statement is read. Entity Framework Core writes the same
//! shape with `AS`: ``UPDATE `Users` AS `u` SET `u`.`Balance` = ...``.

use sqlparser::ast::{ObjectNamePart, Statement, TableFactor};
use sqlparser::tokenizer::Token;

use crate::current_database::byte_offset_of;
use crate::statement_reads;

use super::{parse_one_statement, ParseError, SessionMySqlDialect, SessionSqlMode};

/// Writes a one-table `UPDATE` without the alias it gives its table, or
/// answers `None` when there is no alias to take out.
///
/// A statement holding a subquery is left as written, for the path that reads
/// it to refuse: a subquery may read the same table under the table's own
/// name, which then would name two tables at once. So is one naming a column
/// through the table's own name beside the alias, which MySQL answers 1054.
pub(crate) fn without_the_updated_tables_alias(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    if !starts_with_update(sql) {
        return Ok(None);
    }
    let Ok(Statement::Update(update)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if !update.table.joins.is_empty() || update.from.is_some() {
        return Ok(None);
    }
    let TableFactor::Table {
        name,
        alias: Some(alias),
        ..
    } = &update.table.relation
    else {
        return Ok(None);
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return Ok(None);
    };
    if !alias.columns.is_empty() {
        return Ok(None);
    }
    let alias = &alias.name;
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    if words.iter().any(|token| {
        matches!(&token.token, Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("SELECT"))
    }) {
        return Ok(None);
    }
    let names = |token: &Token, name: &str| matches!(token, Token::Word(word) if word.value.eq_ignore_ascii_case(name));
    let mut rewritten = String::with_capacity(sql.len());
    let mut copied_up_to = 0;
    let mut alias_left_out = false;
    for (at, token) in words.iter().enumerate() {
        let before_a_period = matches!(
            words.get(at + 1).map(|token| &token.token),
            Some(Token::Period)
        );
        // `SET`, `WHERE` and `ORDER BY` name columns, where only the alias may
        // qualify one.
        if before_a_period && names(&token.token, &table.value) && at > 1 {
            return Ok(None);
        }
        if !alias_left_out && token.span.start == alias.span.start {
            // Everything from the end of the table's name to the end of the
            // alias goes, an `AS` between them included.
            rewritten.push_str(&sql[copied_up_to..byte_offset_of(sql, table.span.end)?]);
            copied_up_to = byte_offset_of(sql, token.span.end)?;
            alias_left_out = true;
            continue;
        }
        // The column is written bare rather than through the table's name: the
        // engine does not find a table named with other letter case than the
        // one it stores (`Users.Email` against `users`), and a bare name reads
        // the one table the statement changes. It is quoted, since a column
        // named after a period may be a reserved word.
        if alias_left_out && before_a_period && names(&token.token, &alias.value) {
            let Some(Token::Word(column)) = words.get(at + 2).map(|token| &token.token) else {
                return Ok(None);
            };
            rewritten.push_str(&sql[copied_up_to..byte_offset_of(sql, token.span.start)?]);
            rewritten.push('`');
            rewritten.push_str(&column.value.replace('`', "``"));
            rewritten.push('`');
            copied_up_to = byte_offset_of(sql, words[at + 2].span.end)?;
        }
    }
    if !alias_left_out {
        return Ok(None);
    }
    rewritten.push_str(&sql[copied_up_to..]);
    Ok(Some(rewritten))
}

/// Whether the first word of a statement is `UPDATE`, which is cheaper to
/// ask than to read the statement.
fn starts_with_update(sql: &str) -> bool {
    sql.trim_start()
        .get(..6)
        .is_some_and(|word| word.eq_ignore_ascii_case("UPDATE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(sql: &str) -> Option<String> {
        without_the_updated_tables_alias(sql, SessionSqlMode::default()).unwrap()
    }

    #[test]
    fn hibernate_s_bulk_update_names_its_table_instead_of_its_alias() {
        assert_eq!(
            rewritten("update posts p1_0 set views=(p1_0.views+1) where p1_0.views<5").as_deref(),
            Some("update posts set views=(`views`+1) where `views`<5")
        );
        assert_eq!(
            rewritten("UPDATE posts AS p SET p.views = 1 WHERE p.id = ?").as_deref(),
            Some("UPDATE posts SET `views` = 1 WHERE `id` = ?")
        );
        assert_eq!(
            rewritten("UPDATE `Users` AS `u` SET `u`.`order` = `u`.`order` - 10.0 WHERE `u`.`Email` = 'a'")
                .as_deref(),
            Some("UPDATE `Users` SET `order` = `order` - 10.0 WHERE `Email` = 'a'")
        );
    }

    #[test]
    fn leaves_what_it_cannot_rewrite_as_written() {
        for sql in [
            "update posts set views = 1",
            "update posts p set p.views = 1 where exists (select 1 from posts q where q.id = p.id)",
            "update posts p set posts.views = 1",
            "update posts p join users u on u.id = p.user_id set p.views = 1",
            "select 1",
        ] {
            assert_eq!(rewritten(sql), None, "{sql}");
        }
    }
}
