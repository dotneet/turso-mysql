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

use crate::statement_reads;
use sqlparser::ast::{
    AlterTableOperation, ColumnOption, CreateTableOptions, DataType, Ident, SqlOption, Statement,
};
use sqlparser::tokenizer::Token;

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

/// The table options the engine has nowhere to keep, which end the stored
/// `CREATE TABLE` of a table and are carried across every rewrite of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MySqlTableOptions {
    pub collation: MySqlTableCollation,
    /// The table's `COMMENT`, `None` where it has none or an empty one.
    pub comment: Option<String>,
}

impl MySqlTableOptions {
    /// What ends the stored `CREATE TABLE` of a table with these options, in
    /// the order MySQL prints them: the collation, then the comment.
    pub fn written(&self) -> String {
        match &self.comment {
            Some(comment) => format!(
                "{} COMMENT={}",
                self.collation.table_option(),
                super::quoted_mysql_text(comment)
            ),
            None => self.collation.table_option().to_owned(),
        }
    }
}

/// The collation one `CREATE TABLE` gives its table.
pub fn table_collation_of(
    create_sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlTableCollation, ParseError> {
    Ok(table_options_of(create_sql, mode)?.collation)
}

/// The options one `CREATE TABLE` gives its table that the engine does not
/// keep.
pub fn table_options_of(
    create_sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlTableOptions, ParseError> {
    let Statement::CreateTable(table) = parse_one_statement(create_sql, mode)? else {
        return Err(ParseError::ExpectedCreateTable);
    };
    Ok(super::check_table_options(&table.table_options)?.kept())
}

/// Reads an `ALTER TABLE` that changes the table's comment and does nothing
/// else — Laravel's `alter table t comment = 'x'` and Rails' `ALTER TABLE t
/// COMMENT 'x'` — as the table and the comment it is to have, `None` for an
/// empty one.
///
/// `sqlparser` reads no table option in an `ALTER TABLE`, so the words are read
/// here. Answers `None` for any other statement, a comment beside some other
/// operation among them, which is left to fail where it is read.
pub fn table_comment_change(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<(super::MySqlTableName, Option<String>)>, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let Ok(tokens) = statement_reads::tokens(&dialect, sql) else {
        return Ok(None);
    };
    let mut words = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_) | Token::SemiColon | Token::EOF));
    let named = |token: Option<&Token>, expected: &str| {
        matches!(token, Some(Token::Word(word))
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
    };
    if !named(words.next(), "ALTER") || !named(words.next(), "TABLE") {
        return Ok(None);
    }
    let Some(Token::Word(table)) = words.next() else {
        return Ok(None);
    };
    if !named(words.next(), "COMMENT") {
        return Ok(None);
    }
    let comment = match words.next() {
        Some(Token::Eq) => words.next(),
        written => written,
    };
    let Some(Token::SingleQuotedString(comment)) = comment else {
        return Ok(None);
    };
    if words.next().is_some() {
        return Ok(None);
    }
    let table =
        super::MySqlTableName::parse(&table.value).map_err(|_| ParseError::Unsupported {
            feature: "ALTER TABLE name",
        })?;
    Ok(Some((table, super::checked_table_comment(comment)?)))
}

/// Reads an `ALTER TABLE t ENGINE=InnoDB` that does nothing else, as the table
/// it names.
///
/// Django and Rails write it in some migrations. Every table here is the
/// InnoDB table MySQL makes, so this names the engine the table already has.
/// Another engine is refused: measured on MySQL 8.4.11, `ENGINE=MyISAM` is
/// taken and printed back by `SHOW CREATE TABLE`. Answers `None` for any
/// other statement, the engine beside some other operation among them.
pub fn table_engine_restated(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<super::MySqlTableName>, ParseError> {
    let Some((table, option, value)) = one_table_option(sql, mode) else {
        return Ok(None);
    };
    if !option.eq_ignore_ascii_case("ENGINE") {
        return Ok(None);
    }
    let Token::Word(engine) = value else {
        return Ok(None);
    };
    let engine = engine.value;
    if !engine.eq_ignore_ascii_case("InnoDB") {
        return Err(ParseError::Unsupported {
            feature: "ALTER TABLE ENGINE other than InnoDB",
        });
    }
    Ok(Some(table))
}

/// Reads an `ALTER TABLE t AUTO_INCREMENT = n` that does nothing else, as the
/// table and the number its next row is to take.
///
/// A number written any way but a plain whole one is refused: measured on
/// MySQL 8.4.11, `-5` and `'7'` are each 1064 and `1.5` is rounded, a rule
/// this does not repeat. Answers `None` for any other statement.
pub fn table_counter_change(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<(super::MySqlTableName, u64)>, ParseError> {
    let Some((table, option, value)) = one_table_option(sql, mode) else {
        return Ok(None);
    };
    if !option.eq_ignore_ascii_case("AUTO_INCREMENT") {
        return Ok(None);
    }
    let Token::Number(digits, false) = value else {
        return Err(ParseError::Unsupported {
            feature: "ALTER TABLE AUTO_INCREMENT value",
        });
    };
    let next = digits.parse::<u64>().map_err(|_| ParseError::Unsupported {
        feature: "ALTER TABLE AUTO_INCREMENT value",
    })?;
    Ok(Some((table, next)))
}

/// Reads `ALTER TABLE <table> <option> [=] <value>` and nothing more, as the
/// table, the option's name and its value.
fn one_table_option(
    sql: &str,
    mode: SessionSqlMode,
) -> Option<(super::MySqlTableName, String, Token)> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens(&dialect, sql).ok()?;
    let mut words = tokens
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_) | Token::SemiColon | Token::EOF));
    let named = |token: Option<&Token>, expected: &str| {
        matches!(token, Some(Token::Word(word))
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
    };
    if !named(words.next().as_ref(), "ALTER") || !named(words.next().as_ref(), "TABLE") {
        return None;
    }
    let Some(Token::Word(table)) = words.next() else {
        return None;
    };
    let Some(Token::Word(option)) = words.next() else {
        return None;
    };
    if option.quote_style.is_some() {
        return None;
    }
    let value = match words.next()? {
        Token::Eq => words.next()?,
        written => written,
    };
    if words.next().is_some() {
        return None;
    }
    let table = super::MySqlTableName::parse(&table.value).ok()?;
    Some((table, option.value, value))
}

/// Writes a database's collation at the end of a `CREATE TABLE` that names
/// neither a character set nor a collation, as the table's own.
///
/// Measured on MySQL 8.4.11: such a table in a database made `COLLATE
/// utf8mb4_unicode_ci` is exactly the table written `COLLATE=utf8mb4_unicode_ci`
/// — `SHOW CREATE TABLE` and `information_schema` print the same — while one
/// naming only `CHARSET=utf8mb4` takes that character set's own default,
/// `utf8mb4_0900_ai_ci`, and `CREATE TABLE ... LIKE` takes its source's.
/// `CREATE TABLE ... SELECT` gives the new table the database's collation
/// but each column its source column's, which the rewrite here would lose,
/// so it is refused. Answers `None` where there is nothing to write.
pub fn create_table_with_the_database_collation(
    sql: &str,
    mode: SessionSqlMode,
    database_collation: MySqlTableCollation,
) -> Result<Option<String>, ParseError> {
    if database_collation == MySqlTableCollation::default() {
        return Ok(None);
    }
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if table.like.is_some() || table.clone.is_some() {
        return Ok(None);
    }
    if table.query.is_some() {
        return unsupported(
            "CREATE TABLE ... SELECT in a database whose collation is not the default",
        );
    }
    let names_its_own = match &table.table_options {
        CreateTableOptions::None => false,
        CreateTableOptions::Plain(options) => options.iter().any(|option| {
            matches!(option, SqlOption::KeyValue { key, .. }
                if super::names_a_collation(key) || super::names_a_character_set(key))
        }),
        // Refused where the options are read.
        _ => return Ok(None),
    };
    if names_its_own {
        return Ok(None);
    }
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let Some(last) = tokens.iter().rev().find(|token| {
        !matches!(
            token.token,
            Token::Whitespace(_) | Token::SemiColon | Token::EOF
        )
    }) else {
        return Ok(None);
    };
    let Some(end) = byte_offset_of_location(sql, last.span.end) else {
        return unsupported("CREATE TABLE whose end is unknown");
    };
    Ok(Some(format!(
        "{} COLLATE={}{}",
        &sql[..end],
        database_collation.name(),
        &sql[end..]
    )))
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
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
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

    #[test]
    fn reads_an_engine_or_a_counter_written_alone() {
        let mode = SessionSqlMode::default();
        for sql in [
            "ALTER TABLE t ENGINE=InnoDB",
            "alter table `t` engine = innodb;",
            "ALTER TABLE t ENGINE INNODB",
        ] {
            assert_eq!(
                table_engine_restated(sql, mode).unwrap().unwrap().as_str(),
                "t",
                "{sql}"
            );
        }
        assert!(table_engine_restated("ALTER TABLE t ENGINE=MyISAM", mode).is_err());
        for sql in [
            "ALTER TABLE t ENGINE=InnoDB, ADD COLUMN a INT",
            "ALTER TABLE t ADD COLUMN a INT",
            "ALTER TABLE t COMMENT 'x'",
        ] {
            assert_eq!(table_engine_restated(sql, mode).unwrap(), None, "{sql}");
        }

        let (table, next) = table_counter_change("ALTER TABLE t AUTO_INCREMENT = 10", mode)
            .unwrap()
            .unwrap();
        assert_eq!((table.as_str(), next), ("t", 10));
        assert_eq!(
            table_counter_change("ALTER TABLE t AUTO_INCREMENT 7;", mode)
                .unwrap()
                .map(|(_, next)| next),
            Some(7)
        );
        for sql in [
            "ALTER TABLE t AUTO_INCREMENT = '7'",
            "ALTER TABLE t AUTO_INCREMENT = 1.5",
        ] {
            assert!(table_counter_change(sql, mode).is_err(), "{sql}");
        }
        assert_eq!(
            table_counter_change("ALTER TABLE t AUTO_INCREMENT = 5, ENGINE=InnoDB", mode).unwrap(),
            None
        );
    }

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
    fn a_table_naming_no_collation_is_given_its_databases() {
        let given = |sql: &str, collation| {
            create_table_with_the_database_collation(sql, SessionSqlMode::default(), collation)
        };
        let unicode = MySqlTableCollation::Utf8mb4UnicodeCi;
        assert_eq!(
            given("CREATE TABLE t (a VARCHAR(5)) ENGINE=InnoDB; -- made by hand\n", unicode),
            Ok(Some(
                "CREATE TABLE t (a VARCHAR(5)) ENGINE=InnoDB COLLATE=utf8mb4_unicode_ci; -- made by hand\n"
                    .to_owned()
            ))
        );
        assert_eq!(
            given("create temporary table t (a text)", unicode),
            Ok(Some(
                "create temporary table t (a text) COLLATE=utf8mb4_unicode_ci".to_owned()
            ))
        );
        for sql in [
            "CREATE TABLE t (a VARCHAR(5)) DEFAULT CHARSET=utf8mb4",
            "CREATE TABLE t (a VARCHAR(5)) CHARACTER SET = utf8mb4",
            "CREATE TABLE t (a VARCHAR(5)) COLLATE utf8mb4_0900_ai_ci",
            "CREATE TABLE t LIKE u",
            "ALTER TABLE t ADD COLUMN b TEXT",
            "SELECT 1",
        ] {
            assert_eq!(given(sql, unicode), Ok(None), "{sql}");
        }
        assert_eq!(
            given(
                "CREATE TABLE t (a TEXT)",
                MySqlTableCollation::Utf8mb40900AiCi
            ),
            Ok(None)
        );
        assert!(matches!(
            given("CREATE TABLE t AS SELECT a FROM u", unicode),
            Err(ParseError::Unsupported { .. })
        ));
        assert_eq!(
            given(
                "CREATE TABLE t AS SELECT a FROM u",
                MySqlTableCollation::Utf8mb40900AiCi
            ),
            Ok(None)
        );
    }

    /// Measured on MySQL 8.4.11: the comment is printed after the collation,
    /// quoted the way a column's comment is, and an empty one is not printed.
    #[test]
    fn a_table_comment_is_kept_after_the_collation() {
        let options = |sql: &str| table_options_of(sql, SessionSqlMode::default()).unwrap();
        assert_eq!(
            options("CREATE TABLE t (id INT) COMMENT='it''s a \\\\ back'").written(),
            " COMMENT='it''s a \\\\ back'"
        );
        assert_eq!(
            options("CREATE TABLE t (id INT) COLLATE=utf8mb4_unicode_ci COMMENT 'x'").written(),
            " COLLATE=utf8mb4_unicode_ci COMMENT='x'"
        );
        assert_eq!(options("CREATE TABLE t (id INT) COMMENT=''").comment, None);
        assert!(table_options_of(
            &format!("CREATE TABLE t (id INT) COMMENT='{}'", "x".repeat(2049)),
            SessionSqlMode::default()
        )
        .is_err());
    }

    #[test]
    fn an_alter_changing_only_the_comment_is_read() {
        let changed = |sql: &str| {
            table_comment_change(sql, SessionSqlMode::default())
                .unwrap()
                .map(|(table, comment)| (table.as_str().to_owned(), comment))
        };
        assert_eq!(
            changed("alter table `users` comment = 'Users'"),
            Some(("users".to_owned(), Some("Users".to_owned())))
        );
        assert_eq!(
            changed("ALTER TABLE users COMMENT 'it''s';"),
            Some(("users".to_owned(), Some("it's".to_owned())))
        );
        assert_eq!(
            changed("ALTER TABLE users COMMENT ''"),
            Some(("users".to_owned(), None))
        );
        for sql in [
            "ALTER TABLE users COMMENT 'x', ADD COLUMN n INT",
            "ALTER TABLE users ADD COLUMN n INT COMMENT 'x'",
            "ALTER TABLE db.users COMMENT 'x'",
            "CREATE TABLE users (id INT) COMMENT 'x'",
        ] {
            assert_eq!(changed(sql), None, "{sql}");
        }
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
