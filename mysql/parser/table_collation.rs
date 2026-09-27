//! A table's default collation, and the text columns that take it.
//!
//! MySQL gives a text column declared with neither `CHARACTER SET` nor
//! `COLLATE` the collation of its table. A column that names only a character
//! set takes that character set's own default instead — measured on MySQL
//! 8.4.11, `a VARCHAR(10) CHARACTER SET utf8mb4` in a table declared
//! `COLLATE=utf8mb4_unicode_ci` is `utf8mb4_0900_ai_ci` — and so does a
//! column an `ALTER TABLE` adds that way.
//!
//! The engine has a collation per column and none per table, so the table's
//! collation is written onto each column that takes it, in the statement as
//! the client sent it, before the statement is read. What is stored then names
//! each column's collation itself, and reading it back never has to know which
//! column took the table's.

use sqlparser::ast::{
    AlterTableOperation, ColumnOption, CreateTableOptions, DataType, Ident, SqlOption, Statement,
};
use sqlparser::tokenizer::{Token, Tokenizer};

use super::{
    a_column_of_words, byte_offset_of_location, parse_one_statement, unsupported, ParseError,
    SessionMySqlDialect, SessionSqlMode,
};

/// The collation a table gives the text columns that do not name one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MySqlTableCollation {
    /// `utf8mb4_0900_ai_ci`, what MySQL 8.4 gives a table that names none.
    #[default]
    Utf8mb40900AiCi,
    /// `utf8mb4_unicode_ci`, the Unicode 4.0.0 collation Laravel and Prisma
    /// ask for.
    Utf8mb4UnicodeCi,
}

impl MySqlTableCollation {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("utf8mb4_0900_ai_ci") {
            Some(Self::Utf8mb40900AiCi)
        } else if name.eq_ignore_ascii_case("utf8mb4_unicode_ci") {
            Some(Self::Utf8mb4UnicodeCi)
        } else {
            None
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Utf8mb40900AiCi => "utf8mb4_0900_ai_ci",
            Self::Utf8mb4UnicodeCi => "utf8mb4_unicode_ci",
        }
    }

    /// What ends the stored `CREATE TABLE` of a table with this collation.
    /// The default is left unwritten, as it always has been.
    pub const fn table_option(self) -> &'static str {
        match self {
            Self::Utf8mb40900AiCi => "",
            Self::Utf8mb4UnicodeCi => " COLLATE=utf8mb4_unicode_ci",
        }
    }
}

/// The collation one `CREATE TABLE` gives its table.
pub fn table_collation_of(
    create_sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlTableCollation, ParseError> {
    let Statement::CreateTable(table) = parse_one_statement(create_sql, mode)? else {
        return Err(ParseError::ExpectedCreateTable);
    };
    Ok(super::check_table_options(&table.table_options)?.collation)
}

/// Writes the collation a new table declares onto each of its text columns
/// that names neither a character set nor a collation.
///
/// Answers `None` where there is nothing to write: the statement is not a
/// `CREATE TABLE`, its table takes the default collation, or every text column
/// already names its own.
pub fn create_table_with_its_collation_on_each_text_column(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    let collation = match &table.table_options {
        CreateTableOptions::Plain(options) => collation_named_in(options),
        _ => None,
    };
    let Some(collation) = collation.filter(|collation| *collation != Default::default()) else {
        return Ok(None);
    };
    let names = table
        .columns
        .iter()
        .filter(|column| {
            takes_the_table_collation(&column.data_type, &column.options, |o| &o.option)
        })
        .map(|column| &column.name)
        .collect::<Vec<_>>();
    write_collation_after(sql, mode, &names, collation, false)
}

/// Writes the collation of the table an `ALTER TABLE` changes onto each text
/// column it adds or restates without naming a character set or collation.
///
/// Measured on MySQL 8.4.11: `ADD COLUMN` and `MODIFY` in a table declared
/// `COLLATE=utf8mb4_unicode_ci` both give the column `utf8mb4_unicode_ci`.
pub fn alter_table_with_its_collation_on_each_text_column(
    sql: &str,
    mode: SessionSqlMode,
    collation: MySqlTableCollation,
) -> Result<Option<String>, ParseError> {
    if collation == MySqlTableCollation::default() {
        return Ok(None);
    }
    let Ok(Statement::AlterTable(alter)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    let names = alter
        .operations
        .iter()
        .filter_map(|operation| match operation {
            AlterTableOperation::AddColumn { column_def, .. } => {
                takes_the_table_collation(&column_def.data_type, &column_def.options, |option| {
                    &option.option
                })
                .then_some(&column_def.name)
            }
            AlterTableOperation::ModifyColumn {
                col_name,
                data_type,
                options,
                ..
            } => takes_the_table_collation(data_type, options, |option| option).then_some(col_name),
            AlterTableOperation::ChangeColumn {
                new_name,
                data_type,
                options,
                ..
            } => takes_the_table_collation(data_type, options, |option| option).then_some(new_name),
            _ => None,
        })
        .collect::<Vec<_>>();
    write_collation_after(sql, mode, &names, collation, true)
}

fn collation_named_in(options: &[SqlOption]) -> Option<MySqlTableCollation> {
    options.iter().find_map(|option| match option {
        SqlOption::KeyValue { key, value } if super::names_a_collation(key) => {
            MySqlTableCollation::from_name(&super::written_word(value)?)
        }
        _ => None,
    })
}

fn takes_the_table_collation<T>(
    data_type: &DataType,
    options: &[T],
    option_of: impl Fn(&T) -> &ColumnOption,
) -> bool {
    a_column_of_words(data_type)
        && !options.iter().any(|option| {
            matches!(
                option_of(option),
                ColumnOption::CharacterSet(_) | ColumnOption::Collation(_)
            )
        })
}

/// Writes ` COLLATE <collation>` at the end of the definition each name
/// starts: before the comma or bracket that closes it, or, in an `ALTER`,
/// before the `FIRST` or `AFTER` that places it.
fn write_collation_after(
    sql: &str,
    mode: SessionSqlMode,
    names: &[&Ident],
    collation: MySqlTableCollation,
    placed_by_a_word: bool,
) -> Result<Option<String>, ParseError> {
    if names.is_empty() {
        return Ok(None);
    }
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize_with_location()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let tokens = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let mut ends = Vec::with_capacity(names.len());
    for name in names {
        let Some(start) = tokens
            .iter()
            .position(|token| token.span.start == name.span.start)
        else {
            return unsupported("column whose place in the statement is unknown");
        };
        let mut depth = 0_usize;
        let mut last = start;
        for (at, token) in tokens.iter().enumerate().skip(start + 1) {
            let closes = match &token.token {
                Token::LParen => {
                    depth += 1;
                    false
                }
                Token::RParen if depth > 0 => {
                    depth -= 1;
                    false
                }
                Token::RParen | Token::SemiColon | Token::EOF => true,
                Token::Comma => depth == 0,
                Token::Word(word) if depth == 0 && placed_by_a_word => {
                    word.quote_style.is_none()
                        && (word.value.eq_ignore_ascii_case("FIRST")
                            || word.value.eq_ignore_ascii_case("AFTER"))
                }
                _ => false,
            };
            if closes {
                break;
            }
            last = at;
        }
        let Some(end) = byte_offset_of_location(sql, tokens[last].span.end) else {
            return unsupported("column whose place in the statement is unknown");
        };
        ends.push(end);
    }
    ends.sort_unstable();
    let mut written = String::with_capacity(sql.len() + ends.len() * 32);
    let mut copied_up_to = 0;
    for end in ends {
        written.push_str(&sql[copied_up_to..end]);
        written.push_str(" COLLATE ");
        written.push_str(collation.name());
        copied_up_to = end;
    }
    written.push_str(&sql[copied_up_to..]);
    Ok(Some(written))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(sql: &str) -> Option<String> {
        create_table_with_its_collation_on_each_text_column(sql, SessionSqlMode::default()).unwrap()
    }

    fn altered(sql: &str) -> Option<String> {
        alter_table_with_its_collation_on_each_text_column(
            sql,
            SessionSqlMode::default(),
            MySqlTableCollation::Utf8mb4UnicodeCi,
        )
        .unwrap()
    }

    #[test]
    fn a_new_tables_collation_is_written_on_each_text_column_that_names_none() {
        assert_eq!(
            created(
                "create table `users` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null, `bio` text, `kind` enum('a','b') default 'a', `code` varchar(10) character set utf8mb4, `slug` varchar(10) collate utf8mb4_bin, `n` decimal(8,2)) default character set utf8mb4 collate 'utf8mb4_unicode_ci'"
            )
            .unwrap(),
            "create table `users` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null COLLATE utf8mb4_unicode_ci, `bio` text COLLATE utf8mb4_unicode_ci, `kind` enum('a','b') default 'a' COLLATE utf8mb4_unicode_ci, `code` varchar(10) character set utf8mb4, `slug` varchar(10) collate utf8mb4_bin, `n` decimal(8,2)) default character set utf8mb4 collate 'utf8mb4_unicode_ci'"
        );
        // Prisma writes its table over several lines and ends it with a
        // semicolon.
        assert_eq!(
            created(
                "CREATE TABLE _prisma_migrations (\n    id VARCHAR(36) PRIMARY KEY NOT NULL,\n    logs TEXT\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;"
            )
            .unwrap(),
            "CREATE TABLE _prisma_migrations (\n    id VARCHAR(36) PRIMARY KEY NOT NULL COLLATE utf8mb4_unicode_ci,\n    logs TEXT COLLATE utf8mb4_unicode_ci\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;"
        );
    }

    #[test]
    fn a_table_with_the_default_collation_is_left_as_it_was_written() {
        for sql in [
            "CREATE TABLE t (name varchar(10))",
            "CREATE TABLE t (name varchar(10)) COLLATE=utf8mb4_0900_ai_ci",
            "CREATE TABLE t (id int) COLLATE=utf8mb4_unicode_ci",
            "SELECT 1",
        ] {
            assert_eq!(created(sql), None, "{sql}");
        }
    }

    #[test]
    fn an_alter_writes_the_tables_collation_on_the_columns_it_adds_or_restates() {
        assert_eq!(
            altered(
                "alter table `users` add `nick` varchar(20) null after `name`, modify `bio` mediumtext, change `a` `b` char(2) first, add `c` varchar(3) collate utf8mb4_bin, drop column `d`"
            )
            .unwrap(),
            "alter table `users` add `nick` varchar(20) null COLLATE utf8mb4_unicode_ci after `name`, modify `bio` mediumtext COLLATE utf8mb4_unicode_ci, change `a` `b` char(2) COLLATE utf8mb4_unicode_ci first, add `c` varchar(3) collate utf8mb4_bin, drop column `d`"
        );
        assert_eq!(altered("ALTER TABLE t ADD COLUMN n INT"), None);
        assert_eq!(
            alter_table_with_its_collation_on_each_text_column(
                "ALTER TABLE t ADD COLUMN s TEXT",
                SessionSqlMode::default(),
                MySqlTableCollation::Utf8mb40900AiCi,
            )
            .unwrap(),
            None
        );
    }
}
