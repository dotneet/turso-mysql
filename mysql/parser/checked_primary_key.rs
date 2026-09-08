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
    ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, CreateTableOptions, DataType, Expr,
    PrimaryKeyConstraint, Statement, TableConstraint, Value,
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

/// Writes a table's own `PRIMARY KEY (col)` clause onto the column it names.
///
/// MySQL's `SHOW CREATE TABLE` writes a key as a clause of its own, so every
/// dumped schema and every migration built from one spells it that way, while
/// this reads a key only where the column declares it. Moving the words onto
/// the column lets the one reader answer both spellings.
///
/// The table is left as it was wherever the move would say something the
/// statement did not: a key over several columns, one naming a column the
/// table does not have, one carrying a name or an index option, and a table
/// that already declares a key on a column. Each of those is refused below,
/// the way it always was.
fn table_with_its_key_written_inline(mut table: CreateTable) -> CreateTable {
    let Some((position, key)) = the_only_key_clause(&table) else {
        return table;
    };
    let Some(named) = the_one_column_a_key_names(&key) else {
        return table;
    };
    if table.columns.iter().any(|column| {
        column
            .options
            .iter()
            .any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)))
    }) {
        return table;
    }
    let Some(column) = table
        .columns
        .iter_mut()
        .find(|column| column.name.value.eq_ignore_ascii_case(&named))
    else {
        return table;
    };
    column.options.push(ColumnOptionDef {
        name: None,
        option: ColumnOption::PrimaryKey(PrimaryKeyConstraint {
            name: None,
            columns: Vec::new(),
            ..key
        }),
    });
    table.constraints.remove(position);
    table
}

/// The table's one `PRIMARY KEY` clause and where it stands, or nothing where
/// the table writes none or writes more than one.
fn the_only_key_clause(table: &CreateTable) -> Option<(usize, PrimaryKeyConstraint)> {
    let mut found = None;
    for (position, constraint) in table.constraints.iter().enumerate() {
        let TableConstraint::PrimaryKey(key) = constraint else {
            continue;
        };
        if found.replace((position, key.clone())).is_some() {
            return None;
        }
    }
    found
}

/// The column a plain `PRIMARY KEY (col)` names, or nothing where the clause
/// names anything else.
fn the_one_column_a_key_names(key: &PrimaryKeyConstraint) -> Option<String> {
    // Measured on MySQL 8.4.11: a `CONSTRAINT` name on a key is dropped, the
    // key always being named PRIMARY, so the name says nothing to carry. A
    // `USING BTREE` is printed back and an index name is not, so both stay
    // where they are.
    if key.index_name.is_some()
        || key.index_type.is_some()
        || !key.index_options.is_empty()
        || key.characteristics.is_some()
    {
        return None;
    }
    let [column] = key.columns.as_slice() else {
        return None;
    };
    // Measured: an `ASC` is dropped where a `DESC` is printed back.
    if column.operator_class.is_some()
        || column.column.options.asc == Some(false)
        || column.column.options.nulls_first.is_some()
    {
        return None;
    }
    let Expr::Identifier(named) = &column.column.expr else {
        return None;
    };
    Some(named.value.clone())
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
            _ => options.extend(render_column_option(option)?),
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
                _ => options.extend(render_column_option(option)?),
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
