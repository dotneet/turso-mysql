//! Checked MySQL primary-key table definitions.
//!
//! MySQL treats an inline primary key as `NOT NULL`, while Turso's exact
//! `INTEGER PRIMARY KEY` spelling creates a rowid alias.  This module keeps
//! those two facts separate: the stored MySQL DDL retains the source integer
//! spelling, and the SQLite definition always uses `INT NOT NULL`.

use super::{
    is_plain_inline_primary_key, parse_normalized_create_table, parse_one_statement,
    reject_table_attributes, reject_unsupported_mysql_string_escapes, render_column,
    render_column_option, render_mysql_checked_column, render_mysql_object_name,
    render_table_constraint, unsupported, ParseError, SessionSqlMode,
};
use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, CreateTableOptions, DataType, Expr, Statement,
    TableConstraint, Value,
};
use turso_parser::ast::Stmt;

/// The source spelling of an ordinary signed integer primary-key column.
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
    /// Source spelling of the primary-key integer type.
    pub primary_key_integer_type: CheckedPrimaryKeyIntegerType,
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
    // The common validator deliberately rejects every table option.  Validate
    // the rest of the shape with options removed, then check the one accepted
    // MySQL storage option below instead of silently dropping any option.
    let mut table_without_options = table.clone();
    table_without_options.table_options = CreateTableOptions::None;
    reject_table_attributes(&table_without_options)?;
    if table.temporary {
        return unsupported("TEMPORARY PRIMARY KEY table");
    }
    if table.if_not_exists {
        return unsupported("IF NOT EXISTS PRIMARY KEY table");
    }
    if table.name.0.len() != 1 {
        return unsupported("qualified PRIMARY KEY table name");
    }
    if !table.constraints.is_empty() {
        return unsupported("table-level constraint in PRIMARY KEY table");
    }
    validate_engine_option(&table.table_options)
}

fn validate_engine_option(options: &CreateTableOptions) -> Result<(), ParseError> {
    let options = match options {
        CreateTableOptions::None => return Ok(()),
        CreateTableOptions::Plain(options) => options,
        CreateTableOptions::With(_)
        | CreateTableOptions::Options(_)
        | CreateTableOptions::TableProperties(_) => {
            return unsupported("CREATE TABLE option");
        }
    };
    let [sqlparser::ast::SqlOption::NamedParenthesizedList(engine)] = options.as_slice() else {
        return unsupported("CREATE TABLE option");
    };
    if !engine.key.value.eq_ignore_ascii_case("ENGINE")
        || !engine
            .name
            .as_ref()
            .is_some_and(|name| name.value.eq_ignore_ascii_case("InnoDB"))
        || !engine.values.is_empty()
    {
        return unsupported("CREATE TABLE engine");
    }
    Ok(())
}

fn check_columns(
    table: &CreateTable,
    mode: SessionSqlMode,
) -> Result<(usize, CheckedPrimaryKeyIntegerType), ParseError> {
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
    let primary_key_integer_type = match column.data_type {
        DataType::Int(None) => CheckedPrimaryKeyIntegerType::Int,
        DataType::Integer(None) => CheckedPrimaryKeyIntegerType::Integer,
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
            _ => options.push(render_column_option(option)?),
        }
    }
    let mut definition = format!("{} INT", super::render_ident(&column.name));
    definition.push_str(" NOT NULL");
    if !options.is_empty() {
        definition.push(' ');
        definition.push_str(&options.join(" "));
    }
    definition.push_str(" PRIMARY KEY");
    Ok(definition)
}

fn render_mysql_create_table(
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
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let engine = if has_innodb_engine(&table.table_options) {
        " ENGINE = InnoDB"
    } else {
        ""
    };
    Ok(format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({}){engine}",
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
            DataType::Int(None) => "INT",
            DataType::Integer(None) => "INTEGER",
            _ => return unsupported("PRIMARY KEY column type"),
        };
        let mut options = Vec::new();
        for option in &column.options {
            match &option.option {
                ColumnOption::PrimaryKey(_) => {}
                ColumnOption::Null => return unsupported("NULL PRIMARY KEY"),
                ColumnOption::NotNull => {}
                _ => options.push(render_column_option(option)?),
            }
        }
        let mut definition = format!("{} {data_type} NOT NULL", render_mysql_ident(&column.name));
        if !options.is_empty() {
            definition.push(' ');
            definition.push_str(&options.join(" "));
        }
        definition.push_str(" PRIMARY KEY");
        Ok(definition)
    } else {
        render_mysql_checked_column(column, mode)
    }
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
            assert_eq!(checked.primary_key_integer_type, source_type);
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
        for sql in [
            "CREATE TABLE t (id INT NULL PRIMARY KEY)",
            "CREATE TABLE t (id INT NOT NULL NULL PRIMARY KEY)",
            "CREATE TABLE t (id INT PRIMARY KEY DEFAULT NULL)",
            "CREATE TEMPORARY TABLE t (id INT PRIMARY KEY)",
            "CREATE TABLE IF NOT EXISTS t (id INT PRIMARY KEY)",
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
