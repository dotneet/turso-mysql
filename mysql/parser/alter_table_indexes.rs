use super::{
    inline_index_columns, mentions_ignoring_case, parse_one_statement, unsupported, MySqlTableName,
    ParseError, SessionSqlMode,
};
use crate::statement_reads;
use sqlparser::ast::{AlterTableOperation, ObjectNamePart, Statement, TableConstraint};
use sqlparser::tokenizer::{Token, Whitespace};

/// The `ALTER TABLE` operations that add or remove one index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlAlterTableIndexOperation {
    Add {
        /// The name the statement wrote, or `None` when it wrote none.
        ///
        /// MySQL names an unnamed key after its first column and then
        /// disambiguates with `_2`, `_3` and so on, which needs the names the
        /// table already carries — so the caller does the naming.
        name: Option<String>,
        unique: bool,
        columns: Vec<String>,
    },
    Drop {
        name: String,
    },
    /// `RENAME INDEX from TO to`, or its `RENAME KEY` spelling.
    ///
    /// Measured on MySQL 8.4.11: the renames of one statement are all read
    /// against the indexes the table carried before it, so `RENAME INDEX a TO
    /// b, RENAME INDEX b TO a` swaps two names, and a statement renaming one
    /// index twice answers 1176 for the second.
    Rename {
        from: String,
        to: String,
    },
}

/// One `ALTER TABLE` that only adds or drops indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlAlterTableIndexes {
    table: MySqlTableName,
    operations: Vec<MySqlAlterTableIndexOperation>,
}

impl MySqlAlterTableIndexes {
    /// Returns the table the statement alters.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the operations, in the order the statement wrote them.
    pub fn operations(&self) -> &[MySqlAlterTableIndexOperation] {
        &self.operations
    }
}

/// Reads an `ALTER TABLE` that only adds or drops indexes.
///
/// The engine has no `ALTER TABLE ADD INDEX`, so each operation becomes a
/// `CREATE INDEX` or a `DROP INDEX` of its own and the caller runs them
/// together. Returns `None` for anything that is not such an `ALTER TABLE`, so
/// the ordinary path keeps answering those.
///
/// An unnamed key keeps its `None`, because MySQL names one after its first
/// column and then disambiguates with `_2` and `_3`, which needs the names the
/// table already carries. Refused here are the index options MySQL takes, since
/// none of them could be printed back. A mixed index/column statement is
/// handled by the general splitter in the same transaction.
pub fn parse_optional_alter_table_indexes(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAlterTableIndexes>, ParseError> {
    if let Some(renamed) = renamed_indexes(sql)? {
        return Ok(Some(renamed));
    }
    let spelled_as_alter = drop_index_spelled_as_alter_table(sql);
    let sql = spelled_as_alter.as_deref().unwrap_or(sql);
    let spelled_as_index = drop_key_spelled_as_drop_index(sql);
    let Ok(Statement::AlterTable(alter)) =
        parse_one_statement(spelled_as_index.as_deref().unwrap_or(sql), mode)
    else {
        return Ok(None);
    };
    if !alter.operations.iter().any(is_index_operation) {
        return Ok(None);
    }
    if !alter.operations.iter().all(is_index_operation) {
        return Ok(None);
    }
    if alter.if_exists
        || alter.only
        || alter.location.is_some()
        || alter.on_cluster.is_some()
        || alter.table_type.is_some()
    {
        return unsupported("ALTER TABLE option");
    }
    let [ObjectNamePart::Identifier(table_ident)] = alter.name.0.as_slice() else {
        return unsupported("schema-qualified ALTER TABLE name");
    };
    let table = MySqlTableName::parse(&table_ident.value).map_err(|_| ParseError::Unsupported {
        feature: "ALTER TABLE name",
    })?;
    let operations = alter
        .operations
        .iter()
        .map(checked_index_operation)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(MySqlAlterTableIndexes { table, operations }))
}

/// Reads an `ALTER TABLE` that renames indexes and does nothing else —
/// `ALTER TABLE t RENAME INDEX a TO b`, which is what Laravel's `renameIndex`
/// and Rails' `rename_index` write.
///
/// `sqlparser` reads `RENAME INDEX` as the start of a column rename and fails,
/// so the words are read here instead. Answers `None` for any other statement,
/// a rename beside some other operation among them, which is left to fail
/// where it is read.
fn renamed_indexes(sql: &str) -> Result<Option<MySqlAlterTableIndexes>, ParseError> {
    if !mentions_ignoring_case(sql, "RENAME") {
        return Ok(None);
    }
    let Ok(tokens) = statement_reads::tokens_of_plain_mysql(sql) else {
        return Ok(None);
    };
    let mut words = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .peekable();
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
    let mut operations = Vec::new();
    loop {
        if !named(words.next(), "RENAME") {
            return Ok(None);
        }
        if !matches!(words.next(), Some(Token::Word(word))
            if word.quote_style.is_none()
                && (word.value.eq_ignore_ascii_case("INDEX") || word.value.eq_ignore_ascii_case("KEY")))
        {
            return Ok(None);
        }
        let Some(Token::Word(from)) = words.next() else {
            return Ok(None);
        };
        if !named(words.next(), "TO") {
            return Ok(None);
        }
        let Some(Token::Word(to)) = words.next() else {
            return Ok(None);
        };
        operations.push(MySqlAlterTableIndexOperation::Rename {
            from: checked_index_name(&from.value)?,
            to: checked_index_name(&to.value)?,
        });
        match words.next() {
            Some(Token::Comma) => continue,
            None | Some(Token::EOF) => break,
            Some(Token::SemiColon) if words.all(|token| matches!(token, Token::EOF)) => break,
            Some(_) => return Ok(None),
        }
    }
    let table = MySqlTableName::parse(&table.value).map_err(|_| ParseError::Unsupported {
        feature: "ALTER TABLE name",
    })?;
    Ok(Some(MySqlAlterTableIndexes { table, operations }))
}

/// The tables one MySQL `RENAME TABLE old TO new, ...` renames, pair by pair
/// in the order written.
///
/// MySQL spells renaming both ways and a migration writes whichever its tool
/// generates: Laravel's `Schema::rename` and Rails' `rename_table` both write
/// this one. Answers `None` for any other statement, and for a name qualified
/// by its database, which is left to be refused where it is read.
pub fn renamed_tables(sql: &str) -> Option<Vec<(MySqlTableName, MySqlTableName)>> {
    if !mentions_ignoring_case(sql, "RENAME") {
        return None;
    }
    let tokens = statement_reads::tokens_of_plain_mysql(sql).ok()?;
    let mut words = tokens.iter().filter(|token| {
        !matches!(token, Token::Whitespace(_)) && !matches!(token, Token::SemiColon)
    });
    let named = |token: Option<&Token>, expected: &str| {
        matches!(token, Some(Token::Word(word))
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
    };
    if !named(words.next(), "RENAME") || !named(words.next(), "TABLE") {
        return None;
    }
    let mut pairs = Vec::new();
    loop {
        let Some(Token::Word(old)) = words.next() else {
            return None;
        };
        if !named(words.next(), "TO") {
            return None;
        }
        let Some(Token::Word(new)) = words.next() else {
            return None;
        };
        pairs.push((
            MySqlTableName::parse(&old.value).ok()?,
            MySqlTableName::parse(&new.value).ok()?,
        ));
        match words.next() {
            Some(Token::Comma) => continue,
            None | Some(Token::EOF) => return Some(pairs),
            Some(_) => return None,
        }
    }
}

/// Writes MySQL's standalone `DROP INDEX name ON table` as the `ALTER TABLE`
/// that means the same thing.
///
/// MySQL spells dropping an index both ways and the engine spells it neither:
/// its own `DROP INDEX` names no table. The `ALTER TABLE` form is already read
/// here, so the words are moved into that shape and the one reader answers
/// both. Answers nothing for any other statement, including the engine's own
/// `DROP INDEX name`, which names no table to move.
fn drop_index_spelled_as_alter_table(sql: &str) -> Option<String> {
    if !mentions_ignoring_case(sql, "DROP") {
        return None;
    }
    let tokens = statement_reads::tokens_of_plain_mysql(sql).ok()?;
    let mut words = tokens.iter().filter(|token| {
        !matches!(token, Token::Whitespace(_)) && !matches!(token, Token::SemiColon)
    });
    let named = |token: Option<&&Token>, expected: &str| {
        matches!(token, Some(Token::Word(word))
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected))
    };
    let first = words.next();
    let second = words.next();
    if !named(first.as_ref(), "DROP") || !named(second.as_ref(), "INDEX") {
        return None;
    }
    let Some(Token::Word(index)) = words.next() else {
        return None;
    };
    if !named(words.next().as_ref(), "ON") {
        return None;
    }
    let Some(Token::Word(table)) = words.next() else {
        return None;
    };
    // Anything after the table is an option MySQL takes and this does not.
    if words.next().is_some() {
        return None;
    }
    Some(format!("ALTER TABLE {table} DROP INDEX {index}"))
}

/// Writes `DROP KEY` as the `DROP INDEX` the parser library reads.
///
/// MySQL spells the same operation both ways and `sqlparser` reads only the
/// second, so the words are swapped before it sees them. The swap is made on
/// the tokens rather than on the text, so a table or a column called `key` is
/// left alone — only the word right after a `DROP` is one MySQL means as the
/// keyword. Answers nothing when the statement has no `DROP KEY` to swap.
pub(crate) fn drop_key_spelled_as_drop_index(sql: &str) -> Option<String> {
    if !mentions_ignoring_case(sql, "KEY") {
        return None;
    }
    let mut tokens =
        std::rc::Rc::unwrap_or_clone(statement_reads::tokens_of_plain_mysql(sql).ok()?);
    let mut swapped = false;
    let mut after_a_drop = false;
    for token in &mut tokens {
        let Token::Word(word) = token else {
            if !matches!(token, Token::Whitespace(Whitespace::Space)) {
                after_a_drop = false;
            }
            continue;
        };
        if word.quote_style.is_some() {
            after_a_drop = false;
            continue;
        }
        if word.value.eq_ignore_ascii_case("DROP") {
            after_a_drop = true;
            continue;
        }
        if std::mem::take(&mut after_a_drop) && word.value.eq_ignore_ascii_case("KEY") {
            word.value = String::from("INDEX");
            word.keyword = sqlparser::keywords::Keyword::INDEX;
            swapped = true;
        }
    }
    if !swapped {
        return None;
    }
    Some(tokens.iter().map(ToString::to_string).collect())
}

pub(crate) fn is_index_operation(operation: &AlterTableOperation) -> bool {
    matches!(
        operation,
        AlterTableOperation::DropIndex { .. }
            | AlterTableOperation::AddConstraint {
                constraint: TableConstraint::Index(_) | TableConstraint::Unique(_),
                ..
            }
    )
}

pub(crate) fn checked_index_operation(
    operation: &AlterTableOperation,
) -> Result<MySqlAlterTableIndexOperation, ParseError> {
    match operation {
        AlterTableOperation::DropIndex { name } => Ok(MySqlAlterTableIndexOperation::Drop {
            name: checked_index_name(&name.value)?,
        }),
        AlterTableOperation::AddConstraint {
            constraint: TableConstraint::Index(index),
            not_valid: false,
        } => {
            if index.index_type.is_some() || !index.index_options.is_empty() {
                return unsupported("index option");
            }
            Ok(MySqlAlterTableIndexOperation::Add {
                name: index
                    .name
                    .as_ref()
                    .map(|name| checked_index_name(&name.value))
                    .transpose()?,
                unique: false,
                columns: inline_index_columns(&index.columns)?,
            })
        }
        AlterTableOperation::AddConstraint {
            constraint: TableConstraint::Unique(unique),
            not_valid: false,
        } => {
            if unique.index_type.is_some()
                || !unique.index_options.is_empty()
                || unique.characteristics.is_some()
            {
                return unsupported("index option");
            }
            // MySQL keeps a `CONSTRAINT name UNIQUE ...` name apart from the
            // index name, and this has only the one name to print back.
            if unique.name.is_some() && unique.index_name.is_some() {
                return unsupported("ADD UNIQUE with both a constraint and an index name");
            }
            Ok(MySqlAlterTableIndexOperation::Add {
                name: unique
                    .index_name
                    .as_ref()
                    .or(unique.name.as_ref())
                    .map(|name| checked_index_name(&name.value))
                    .transpose()?,
                unique: true,
                columns: inline_index_columns(&unique.columns)?,
            })
        }
        // A statement naming an index operation names only those, because the
        // two kinds of change would have to apply together.
        _ => unsupported("ALTER TABLE mixing index and other operations"),
    }
}

fn checked_index_name(name: &str) -> Result<String, ParseError> {
    MySqlTableName::parse(name).map_err(|_| ParseError::Unsupported {
        feature: "index name",
    })?;
    // MySQL calls the primary key's index `PRIMARY`, and adding or dropping a
    // primary key is a different operation than adding or dropping an index.
    if name.eq_ignore_ascii_case("primary") {
        return unsupported("index named PRIMARY");
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(sql: &str) -> MySqlAlterTableIndexes {
        parse_optional_alter_table_indexes(sql, SessionSqlMode::default())
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
            .unwrap_or_else(|| panic!("{sql}: not read as an index ALTER"))
    }

    #[test]
    fn alter_table_reads_the_index_operations_it_names() {
        let added = parsed("ALTER TABLE `Records` ADD INDEX idx_c (c), ADD KEY idx_d (d)");
        assert_eq!(added.table().as_str(), "records");
        assert_eq!(
            added.operations(),
            [
                MySqlAlterTableIndexOperation::Add {
                    name: Some("idx_c".to_owned()),
                    unique: false,
                    columns: vec!["c".to_owned()],
                },
                MySqlAlterTableIndexOperation::Add {
                    name: Some("idx_d".to_owned()),
                    unique: false,
                    columns: vec!["d".to_owned()],
                },
            ]
        );

        let unique = parsed("ALTER TABLE records ADD UNIQUE INDEX uniq_cd (c, d)");
        assert_eq!(
            unique.operations(),
            [MySqlAlterTableIndexOperation::Add {
                name: Some("uniq_cd".to_owned()),
                unique: true,
                columns: vec!["c".to_owned(), "d".to_owned()],
            }]
        );
        assert_eq!(
            parsed("ALTER TABLE records ADD CONSTRAINT UKhgnku1cs6oh0f99ssb6flto0x UNIQUE (name)")
                .operations(),
            [MySqlAlterTableIndexOperation::Add {
                name: Some("UKhgnku1cs6oh0f99ssb6flto0x".to_owned()),
                unique: true,
                columns: vec!["name".to_owned()],
            }]
        );

        // An unnamed key keeps its `None`; the caller names it, because the
        // rule counts the names the table already carries.
        assert_eq!(
            parsed("ALTER TABLE records ADD INDEX (c), ADD UNIQUE (c, d)").operations(),
            [
                MySqlAlterTableIndexOperation::Add {
                    name: None,
                    unique: false,
                    columns: vec!["c".to_owned()],
                },
                MySqlAlterTableIndexOperation::Add {
                    name: None,
                    unique: true,
                    columns: vec!["c".to_owned(), "d".to_owned()],
                },
            ]
        );

        let dropped = parsed("ALTER TABLE records DROP INDEX idx_c");
        assert_eq!(
            dropped.operations(),
            [MySqlAlterTableIndexOperation::Drop {
                name: "idx_c".to_owned(),
            }]
        );
        // `DROP KEY` is MySQL's other spelling for the same thing, written as
        // `DROP INDEX` before the parser library — which reads only the one —
        // sees it.
        for sql in [
            "ALTER TABLE records DROP KEY idx_c",
            "ALTER TABLE records drop key idx_c",
            "ALTER TABLE records DROP  KEY  idx_c",
        ] {
            assert_eq!(
                parsed(sql).operations(),
                [MySqlAlterTableIndexOperation::Drop {
                    name: "idx_c".to_owned(),
                }],
                "{sql}"
            );
        }
        assert_eq!(
            parsed("ALTER TABLE records DROP KEY idx_c, DROP INDEX idx_d").operations(),
            [
                MySqlAlterTableIndexOperation::Drop {
                    name: "idx_c".to_owned(),
                },
                MySqlAlterTableIndexOperation::Drop {
                    name: "idx_d".to_owned(),
                },
            ]
        );

        // The swap is made on the words a `DROP` is followed by, so a column
        // called `key` is left where it is: this is a column operation and no
        // index one, which the index reader leaves alone.
        assert_eq!(
            parse_optional_alter_table_indexes(
                "ALTER TABLE records DROP COLUMN `key`",
                SessionSqlMode::default()
            )
            .unwrap(),
            None
        );
        // A quoted `key` after a `DROP` is a column of that name, not the
        // keyword, so it is left alone too.
        assert_eq!(
            parse_optional_alter_table_indexes(
                "ALTER TABLE records DROP `key`",
                SessionSqlMode::default()
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn alter_table_reads_the_indexes_it_renames() {
        let rename = |from: &str, to: &str| MySqlAlterTableIndexOperation::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        };
        assert_eq!(
            parsed("ALTER TABLE records RENAME INDEX ka TO kz").operations(),
            [rename("ka", "kz")]
        );
        let renamed = parsed("alter table `Records` rename key `a_b` to `c`, RENAME INDEX x TO y;");
        assert_eq!(renamed.table().as_str(), "records");
        assert_eq!(renamed.operations(), [rename("a_b", "c"), rename("x", "y")]);
        for sql in [
            "ALTER TABLE records RENAME INDEX a TO b, ADD COLUMN c INT",
            "ALTER TABLE records RENAME INDEX a",
            "ALTER TABLE records RENAME COLUMN a TO b",
        ] {
            assert!(
                renamed_indexes(sql).unwrap().is_none(),
                "{sql} must be left to the ordinary path"
            );
        }
        // MySQL calls the primary key's index `PRIMARY`: measured, renaming it
        // is 1176 and renaming onto it is 1280.
        for sql in [
            "ALTER TABLE records RENAME INDEX `PRIMARY` TO k",
            "ALTER TABLE records RENAME INDEX k TO `PRIMARY`",
        ] {
            assert!(renamed_indexes(sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn rename_table_reads_each_pair_in_order() {
        let names = |sql: &str| {
            renamed_tables(sql).map(|pairs| {
                pairs
                    .into_iter()
                    .map(|(from, to)| (from.as_str().to_owned(), to.as_str().to_owned()))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            names("RENAME TABLE `posts` TO `articles`;"),
            Some(vec![("posts".to_owned(), "articles".to_owned())])
        );
        assert_eq!(
            names("rename table a to t, b to a, t to b"),
            Some(vec![
                ("a".to_owned(), "t".to_owned()),
                ("b".to_owned(), "a".to_owned()),
                ("t".to_owned(), "b".to_owned()),
            ])
        );
        for sql in [
            "RENAME TABLE a TO b,",
            "RENAME TABLE db.a TO b",
            "RENAME TABLE a b",
            "RENAME USER a TO b",
        ] {
            assert_eq!(names(sql), None, "{sql}");
        }
    }

    #[test]
    fn alter_table_leaves_every_other_statement_alone() {
        for sql in [
            "ALTER TABLE records ADD COLUMN c INT",
            "ALTER TABLE records DROP COLUMN c",
            "ALTER TABLE records RENAME TO archive",
            "CREATE INDEX idx_c ON records (c)",
            "SELECT 1",
        ] {
            assert_eq!(
                parse_optional_alter_table_indexes(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
    }

    #[test]
    fn alter_table_refuses_what_it_cannot_print_back() {
        for sql in [
            "ALTER TABLE records ADD INDEX idx_c USING BTREE (c)",
            "ALTER TABLE records ADD INDEX idx_c (c(4))",
            "ALTER TABLE db.records ADD INDEX idx_c (c)",
        ] {
            assert!(
                parse_optional_alter_table_indexes(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
