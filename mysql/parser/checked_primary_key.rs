//! Checked MySQL primary-key table definitions.
//!
//! MySQL treats an inline primary key as `NOT NULL`, while Turso's exact
//! `INTEGER PRIMARY KEY` spelling creates a rowid alias.  This module keeps
//! those two facts separate: the stored MySQL DDL retains the source integer
//! spelling, and the SQLite definition writes an integer key as `INT NOT NULL`.
//!
//! A key over a word is written with the type it was declared with, there
//! being no alias to avoid: `version VARCHAR(255) PRIMARY KEY` is the table
//! every migration tool keeps its own bookkeeping in.

use super::{
    is_plain_inline_primary_key, parse_normalized_create_table, parse_one_statement,
    reject_attributes_and_check_options, reject_unsupported_mysql_string_escapes, render_column,
    render_column_option, render_mysql_checked_column, render_mysql_object_name,
    render_table_constraint, table_with_its_key_written_inline, unsupported, written_comment,
    ParseError, SessionSqlMode, WORDS_COLLATION,
};
use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, CreateTableOptions, DataType, Expr, Statement,
    TableConstraint, Value,
};
use turso_parser::ast::Stmt;

/// The source spelling of an ordinary signed integer primary-key column.
///
/// A key over a word has none of these: its type is written as declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedPrimaryKeyIntegerType {
    /// The MySQL `INT` spelling.
    Int,
    /// The MySQL `INTEGER` spelling.
    Integer,
}

impl CheckedPrimaryKeyIntegerType {
    /// Returns the spelling used in the durable MySQL DDL.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Int => "INT",
            Self::Integer => "INTEGER",
        }
    }
}

/// One checked ordinary MySQL `CREATE TABLE` with an inline integer primary key.
///
/// `sqlite_statement` deliberately does not retain the source integer alias:
/// both `INT` and `INTEGER` are lowered to `INT NOT NULL` so Core cannot turn
/// the column into a rowid alias.  Callers must persist
/// `normalized_mysql_ddl` in the schema envelope and use it when rebuilding
/// MySQL metadata or replaying the table definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedPrimaryKeyCreateTable {
    /// Canonical MySQL DDL suitable for the durable schema envelope.
    pub normalized_mysql_ddl: String,
    /// SQLite-compatible table definition with a regular primary-key index.
    pub sqlite_statement: Stmt,
    /// Zero-based stored-column position of the inline primary key.
    pub primary_key_column_ordinal: usize,
    /// Name of the inline primary-key column.
    pub primary_key_column_name: String,
    /// Source spelling of the primary-key integer type, where the key is over
    /// a number. A key over a word has none.
    pub primary_key_integer_type: Option<CheckedPrimaryKeyIntegerType>,
}

/// Parses the checked ordinary `INT`/`INTEGER PRIMARY KEY` table slice.
///
/// This parser accepts the storage-engine no-op `ENGINE=InnoDB` option and
/// preserves it in the durable MySQL DDL.  Other table options and all
/// table-level primary-key forms remain rejected until their metadata and
/// replay contracts are implemented.
pub fn parse_checked_primary_key_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedPrimaryKeyCreateTable, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let table = table_with_its_key_written_inline(table);
    check_table_shape(&table)?;

    let (primary_key_column_ordinal, primary_key_integer_type) = check_columns(&table, mode)?;
    let primary_key_column_name = table.columns[primary_key_column_ordinal].name.value.clone();

    let sqlite_sql = render_sqlite_create_table(&table, primary_key_column_ordinal)?;
    let sqlite_statement = parse_normalized_create_table(&sqlite_sql)?;
    let normalized_mysql_ddl = render_mysql_create_table(&table, mode)?;

    Ok(CheckedPrimaryKeyCreateTable {
        normalized_mysql_ddl,
        sqlite_statement,
        primary_key_column_ordinal,
        primary_key_column_name,
        primary_key_integer_type,
    })
}

fn check_table_shape(table: &CreateTable) -> Result<(), ParseError> {
    reject_attributes_and_check_options(table)?;
    if table.temporary {
        return unsupported("TEMPORARY PRIMARY KEY table");
    }
    if table.name.0.len() != 1 {
        return unsupported("qualified PRIMARY KEY table name");
    }
    // A foreign key is the one table-level constraint a primary-key table
    // takes: both definitions below already render whatever constraints the
    // table carries, and nearly every table with a foreign key has a primary
    // key too. The rest stay refused, each for the reason it always was.
    if table
        .constraints
        .iter()
        .any(|constraint| !matches!(constraint, TableConstraint::ForeignKey(_)))
    {
        return unsupported("table-level constraint in PRIMARY KEY table");
    }
    Ok(())
}

fn check_columns(
    table: &CreateTable,
    mode: SessionSqlMode,
) -> Result<(usize, Option<CheckedPrimaryKeyIntegerType>), ParseError> {
    if table.columns.is_empty() {
        return unsupported("CREATE TABLE without columns");
    }
    for (index, column) in table.columns.iter().enumerate() {
        if table.columns[..index]
            .iter()
            .any(|previous| previous.name.value.eq_ignore_ascii_case(&column.name.value))
        {
            return unsupported("duplicate column name");
        }
    }

    if table
        .constraints
        .iter()
        .any(|constraint| matches!(constraint, TableConstraint::PrimaryKey(_)))
    {
        return unsupported("table-level PRIMARY KEY");
    }

    let mut primary_key = None;
    for (ordinal, column) in table.columns.iter().enumerate() {
        for option in &column.options {
            if !matches!(&option.option, ColumnOption::PrimaryKey(_)) {
                continue;
            }
            if primary_key.replace(ordinal).is_some() {
                return unsupported("multiple PRIMARY KEY constraints");
            }
            if !is_plain_inline_primary_key(&option.option) {
                return unsupported("PRIMARY KEY attribute");
            }
        }
    }
    let Some(primary_key_column_ordinal) = primary_key else {
        return unsupported("inline INT PRIMARY KEY");
    };
    let column = &table.columns[primary_key_column_ordinal];
    // A key over a word is what a migration tool keeps its own record in, and
    // what a table keyed by a name or a code uses. MySQL wants a length on one
    // — measured on 8.4.11, a bare `TEXT PRIMARY KEY` is 1170 — so only the
    // two types that carry one are read here.
    let primary_key_integer_type = match column.data_type {
        DataType::Int(None) => Some(CheckedPrimaryKeyIntegerType::Int),
        DataType::Integer(None) => Some(CheckedPrimaryKeyIntegerType::Integer),
        DataType::Varchar(Some(_)) | DataType::Char(Some(_)) => None,
        _ => return unsupported("PRIMARY KEY column type"),
    };
    check_primary_key_options(column)?;

    // Validate the other columns and table constraints using the same checked
    // renderers as the general CREATE TABLE path.  Their output is reused in
    // both definitions below, so unsupported syntax cannot be discarded.
    for (ordinal, column) in table.columns.iter().enumerate() {
        if ordinal != primary_key_column_ordinal {
            render_column(column)?;
            render_mysql_checked_column(column, mode)?;
        }
    }
    for constraint in &table.constraints {
        render_table_constraint(constraint)?;
    }
    Ok((primary_key_column_ordinal, primary_key_integer_type))
}

fn check_primary_key_options(column: &ColumnDef) -> Result<(), ParseError> {
    let mut nullable_options = 0;
    let mut default_options = 0;
    let mut primary_key_options = 0;
    for option in &column.options {
        match &option.option {
            ColumnOption::Null => {
                // MySQL rejects an explicitly nullable primary key.  Keeping
                // this check next to the effective NOT NULL lowering prevents
                // a later renderer from accidentally changing that contract.
                return unsupported("NULL PRIMARY KEY");
            }
            ColumnOption::NotNull => nullable_options += 1,
            ColumnOption::Default(expr) => {
                default_options += 1;
                if matches!(expr, Expr::Value(value) if matches!(value.value, Value::Null)) {
                    return unsupported("NULL DEFAULT for PRIMARY KEY");
                }
            }
            ColumnOption::PrimaryKey(primary_key) => {
                primary_key_options += 1;
                if option.name.is_some() || !is_plain_inline_primary_key(&option.option) {
                    return unsupported("PRIMARY KEY attribute");
                }
                if !primary_key.index_options.is_empty() {
                    return unsupported("PRIMARY KEY index option");
                }
            }
            // The engine has no attribute for a comment, so it is written
            // nowhere in the SQLite definition and put back into the stored
            // MySQL DDL by the renderer.
            ColumnOption::Comment(_) if option.name.is_none() => {}
            _ => return unsupported("PRIMARY KEY column attribute"),
        }
    }
    if nullable_options > 1 {
        return unsupported("multiple column NULL options");
    }
    if default_options > 1 {
        return unsupported("multiple column DEFAULT options");
    }
    if primary_key_options != 1 {
        return unsupported("inline PRIMARY KEY");
    }
    Ok(())
}

fn render_sqlite_create_table(
    table: &CreateTable,
    primary_key_column_ordinal: usize,
) -> Result<String, ParseError> {
    let columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(ordinal, column)| {
            if ordinal == primary_key_column_ordinal {
                render_sqlite_primary_key_column(column)
            } else {
                render_column(column)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let constraints = table
        .constraints
        .iter()
        .map(render_table_constraint)
        .collect::<Result<Vec<_>, _>>()?;
    let mut definitions = columns;
    definitions.extend(constraints);
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    Ok(format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        super::render_name(&table.name)?,
        definitions.join(", ")
    ))
}

fn render_sqlite_primary_key_column(column: &ColumnDef) -> Result<String, ParseError> {
    let mut options = Vec::new();
    for option in &column.options {
        match &option.option {
            ColumnOption::PrimaryKey(_) => {}
            ColumnOption::Null => return unsupported("NULL PRIMARY KEY"),
            ColumnOption::NotNull => {}
            _ => options.extend(render_column_option(option, &column.data_type)?),
        }
    }
    // The exact `INTEGER PRIMARY KEY` spelling would make the column a rowid
    // alias, so an integer key is always written `INT`. A key over a word has
    // no alias to avoid and is written as it was declared.
    let data_type = match column.data_type {
        DataType::Int(None) | DataType::Integer(None) => "INT".to_owned(),
        _ => the_type_a_column_is_written_with(column)?,
    };
    // A key over words matches them under the collation the column is declared
    // with, and this server matches words without regard to case.
    let collation = if super::a_column_of_words(&column.data_type) {
        WORDS_COLLATION
    } else {
        ""
    };
    let mut definition = format!(
        "{} {data_type}{collation}",
        super::render_ident(&column.name)
    );
    definition.push_str(" NOT NULL");
    if !options.is_empty() {
        definition.push(' ');
        definition.push_str(&options.join(" "));
    }
    definition.push_str(" PRIMARY KEY");
    Ok(definition)
}

pub(crate) fn render_mysql_create_table(
    table: &CreateTable,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let columns = table
        .columns
        .iter()
        .map(|column| render_mysql_source_column(column, mode))
        .collect::<Result<Vec<_>, _>>()?;
    let constraints = table
        .constraints
        .iter()
        .map(|constraint| {
            render_table_constraint(constraint)?;
            Ok(constraint.to_string())
        })
        .collect::<Result<Vec<_>, ParseError>>()?;
    let mut definitions = columns;
    definitions.extend(constraints);
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let engine = if has_innodb_engine(&table.table_options) {
        " ENGINE = InnoDB"
    } else {
        ""
    };
    // The stored DDL is what the table is remembered by, and MySQL's own
    // `SHOW CREATE TABLE` never prints `IF NOT EXISTS` — measured, a table
    // written with it prints back without it — so the words are read and left
    // out of what is stored.
    Ok(format!(
        "CREATE {temporary}TABLE {} ({}){engine}",
        render_mysql_object_name(&table.name)?,
        definitions.join(", ")
    ))
}

fn render_mysql_source_column(
    column: &ColumnDef,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    if column
        .options
        .iter()
        .any(|option| matches!(&option.option, ColumnOption::PrimaryKey(_)))
    {
        let data_type = match column.data_type {
            DataType::Int(None) => "INT".to_owned(),
            DataType::Integer(None) => "INTEGER".to_owned(),
            _ => the_type_a_column_is_written_with(column)?,
        };
        let mut options = Vec::new();
        for option in &column.options {
            match &option.option {
                ColumnOption::PrimaryKey(_) => {}
                ColumnOption::Null => return unsupported("NULL PRIMARY KEY"),
                ColumnOption::NotNull => {}
                _ => options.extend(render_column_option(option, &column.data_type)?),
            }
        }
        let mut definition = format!("{} {data_type} NOT NULL", render_mysql_ident(&column.name));
        if !options.is_empty() {
            definition.push(' ');
            definition.push_str(&options.join(" "));
        }
        definition.push_str(" PRIMARY KEY");
        // MySQL prints a comment last, after the key words.
        definition.push_str(&written_comment(column));
        Ok(definition)
    } else {
        render_mysql_checked_column(column, mode)
    }
}

/// The type text a column is written with, read back off the one renderer
/// that decides it.
///
/// A key column is written by hand here rather than through that renderer,
/// which would write its options too, so the type alone is taken from a copy
/// of the column carrying none.
fn the_type_a_column_is_written_with(column: &ColumnDef) -> Result<String, ParseError> {
    let bare = ColumnDef {
        name: column.name.clone(),
        data_type: column.data_type.clone(),
        options: Vec::new(),
    };
    let written = render_column(&bare)?;
    let name = super::render_ident(&column.name);
    let written = &written[name.len() + 1..];
    // A column of words is written with the collation this server matches
    // words under. That belongs to the engine's definition rather than to the
    // type, and a key column's definition is built here by hand.
    Ok(written
        .strip_suffix(WORDS_COLLATION)
        .unwrap_or(written)
        .to_owned())
}

fn render_mysql_ident(ident: &sqlparser::ast::Ident) -> String {
    format!("`{}`", ident.value.replace('`', "``"))
}

fn has_innodb_engine(options: &CreateTableOptions) -> bool {
    let CreateTableOptions::Plain(options) = options else {
        return false;
    };
    let [sqlparser::ast::SqlOption::NamedParenthesizedList(engine)] = options.as_slice() else {
        return false;
    };
    engine.key.value.eq_ignore_ascii_case("ENGINE")
        && engine
            .name
            .as_ref()
            .is_some_and(|name| name.value.eq_ignore_ascii_case("InnoDB"))
        && engine.values.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_parser::ast::{ColumnConstraint, CreateTableBody, NamedColumnConstraint};

    /// MySQL's own `SHOW CREATE TABLE` writes the key as a clause of its own,
    /// so a dumped schema is read here by moving the words onto the column.
    ///
    /// Measured on MySQL 8.4.11: a `CONSTRAINT` name on the key is dropped, an
    /// `ASC` is dropped, and the column named is matched without regard to
    /// case. A `USING BTREE` is printed back, several columns make a key this
    /// has no rowid for, and the other shapes each say something the move
    /// would lose, so all of them stay refused.
    #[test]
    fn a_key_written_as_a_clause_is_read_as_the_column_declaring_it() {
        for (written, inline) in [
            (
                "CREATE TABLE t (id INT NOT NULL, n INT, PRIMARY KEY (id))",
                "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT)",
            ),
            (
                "CREATE TABLE t (id INT NOT NULL, n INT, PRIMARY KEY (`id`))",
                "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT)",
            ),
            (
                "CREATE TABLE t (id INT NOT NULL, n INT, PRIMARY KEY (ID))",
                "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT)",
            ),
            (
                "CREATE TABLE t (id INT NOT NULL, n INT, CONSTRAINT pk PRIMARY KEY (id))",
                "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT)",
            ),
            (
                "CREATE TABLE t (id INT NOT NULL, n INT, PRIMARY KEY (id ASC))",
                "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, n INT)",
            ),
            (
                "CREATE TABLE t (id INT NOT NULL, n INT, PRIMARY KEY (n))",
                "CREATE TABLE t (id INT NOT NULL, n INT PRIMARY KEY)",
            ),
        ] {
            let written =
                parse_checked_primary_key_create_table(written, SessionSqlMode::default()).unwrap();
            let inline =
                parse_checked_primary_key_create_table(inline, SessionSqlMode::default()).unwrap();
            assert_eq!(written, inline);
        }

        for sql in [
            // A key over several columns has no one rowid to stand for it.
            "CREATE TABLE t (id INT NOT NULL, n INT NOT NULL, PRIMARY KEY (id, n))",
            // A column that is not there, which MySQL answers 1072 for.
            "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (missing))",
            // Two keys, which MySQL answers 1068 for.
            "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, PRIMARY KEY (id))",
            // A `USING BTREE` and a `DESC` are both printed back.
            "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY USING BTREE (id))",
            "CREATE TABLE t (id INT NOT NULL, PRIMARY KEY (id DESC))",
        ] {
            assert!(
                parse_checked_primary_key_create_table(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn preserves_integer_alias_and_innodb_while_lowering_storage_type() {
        for (sql, source_type) in [
            (
                "CREATE TABLE t (id INT PRIMARY KEY) ENGINE=InnoDB",
                CheckedPrimaryKeyIntegerType::Int,
            ),
            (
                "CREATE TABLE t (id INTEGER PRIMARY KEY) ENGINE=InnoDB",
                CheckedPrimaryKeyIntegerType::Integer,
            ),
        ] {
            let checked =
                parse_checked_primary_key_create_table(sql, SessionSqlMode::default()).unwrap();
            assert_eq!(checked.primary_key_integer_type, Some(source_type));
            assert!(checked.normalized_mysql_ddl.contains(source_type.as_str()));
            assert!(checked.normalized_mysql_ddl.ends_with("ENGINE = InnoDB"));
            let Stmt::CreateTable { body, .. } = checked.sqlite_statement else {
                panic!("expected CREATE TABLE");
            };
            let CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
                panic!("expected columns");
            };
            assert_eq!(columns[0].col_type.as_ref().unwrap().name, "INT");
            assert!(matches!(
                columns[0].constraints.as_slice(),
                [
                    NamedColumnConstraint {
                        constraint: ColumnConstraint::NotNull {
                            nullable: false,
                            ..
                        },
                        ..
                    },
                    NamedColumnConstraint {
                        constraint: ColumnConstraint::PrimaryKey { .. },
                        ..
                    }
                ]
            ));
        }
    }

    #[test]
    fn preserves_default_and_rejects_ambiguous_primary_key_shapes() {
        let checked = parse_checked_primary_key_create_table(
            "CREATE TABLE t (id INTEGER DEFAULT 7 PRIMARY KEY)",
            SessionSqlMode::default(),
        )
        .unwrap();
        assert_eq!(
            checked.normalized_mysql_ddl,
            "CREATE TABLE `t` (`id` INTEGER NOT NULL DEFAULT 7 PRIMARY KEY)"
        );
        // `IF NOT EXISTS` says what to do about a table that is already
        // there rather than what the table is, and MySQL never prints it back,
        // so it is read and left out of what is stored.
        let written = parse_checked_primary_key_create_table(
            "CREATE TABLE IF NOT EXISTS t (id INT PRIMARY KEY)",
            SessionSqlMode::default(),
        )
        .unwrap();
        assert_eq!(
            written.normalized_mysql_ddl,
            "CREATE TABLE `t` (`id` INT NOT NULL PRIMARY KEY)"
        );

        for sql in [
            "CREATE TABLE t (id INT NULL PRIMARY KEY)",
            "CREATE TABLE t (id INT NOT NULL NULL PRIMARY KEY)",
            "CREATE TABLE t (id INT PRIMARY KEY DEFAULT NULL)",
            "CREATE TEMPORARY TABLE t (id INT PRIMARY KEY)",
            "CREATE TABLE app.t (id INT PRIMARY KEY)",
            "CREATE TABLE t (id INT UNIQUE PRIMARY KEY)",
            "CREATE TABLE t (id INT CHECK (id > 0) PRIMARY KEY)",
            "CREATE TABLE t (id INT PRIMARY KEY, ID TEXT)",
            "CREATE TABLE t (id INT PRIMARY KEY, PRIMARY KEY (id))",
            "CREATE TABLE t (id INT PRIMARY KEY) ENGINE=MyISAM",
            "CREATE TABLE t (id INT PRIMARY KEY) ENGINE=InnoDB(foo)",
            "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY)",
        ] {
            assert!(
                parse_checked_primary_key_create_table(sql, SessionSqlMode::default()).is_err(),
                "expected rejection for {sql}"
            );
        }
    }
}
